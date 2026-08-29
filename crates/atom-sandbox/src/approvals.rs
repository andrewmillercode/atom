//! Layer 2 — approval gate.
//!
//! v2 narrows the decision set to four buttons:
//!
//! - [`Decision::AllowOnce`] — run, no memory.
//! - [`Decision::AllowAll`] — run + save a prefix rule to
//!   `sandbox.json` so the command family lands in Tier 1.
//! - [`Decision::DenyOnce`] — refuse this run.
//! - [`Decision::DenyAll`] — refuse + save a deny rule so the family
//!   never goes silent (still prompts, but with the rule name as
//!   reason).
//!
//! Session-scoped grants are gone. The only persistent state is the
//! user's `rules` block in [`crate::policy::SandboxConfig`].

use crate::policy::{prefix_for_command, RuleKind, RuleMatch, Rules, SandboxConfig};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the user (or an auto-approver) decided for one prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Run this once, no memory of the grant.
    AllowOnce,
    /// Run this + save a prefix allow rule to sandbox.json.
    AllowAll,
    /// Refuse this run, no memory.
    #[default]
    DenyOnce,
    /// Refuse this run + save a prefix deny rule.
    DenyAll,
}

impl Decision {
    pub fn allows(&self) -> bool {
        matches!(self, Decision::AllowOnce | Decision::AllowAll)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::AllowOnce => "allow_once",
            Decision::AllowAll => "allow_always",
            Decision::DenyOnce => "deny_once",
            Decision::DenyAll => "deny_always",
        }
    }

    /// Map the v1 wire names onto v2's smaller set so existing TUI /
    /// server code keeps compiling during the rename. Old
    /// AllowSession → AllowOnce (the session-scoped concept no longer
    /// exists); old AllowGlobal → AllowAll.
    pub fn from_legacy_wire(s: &str) -> Option<Self> {
        match s {
            "allow_once" => Some(Decision::AllowOnce),
            "allow_session" => Some(Decision::AllowOnce),
            "allow_always" | "allow_all" | "allow_global" => Some(Decision::AllowAll),
            "deny_once" | "deny" => Some(Decision::DenyOnce),
            "deny_always" | "deny_all" => Some(Decision::DenyAll),
            _ => None,
        }
    }
}

/// One pending approval surfaced to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub command: String,
    pub cwd: PathBuf,
    /// Matched rule id, or "" when the verdict came from the unknown-
    /// command fallback.
    pub rule_id: String,
    pub reason: String,
    /// Optional prefix-rule preview for `[a] accept-all`. Pre-computed
    /// by the server so the TUI doesn't have to re-tokenize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_all_preview: Option<String>,
}

/// Who answers approval prompts. The server wires this to an
/// `approval_request` event + `POST /approval/:session`.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, req: ApprovalRequest) -> Decision;
}

/// Test/CI approver returning a fixed decision.
pub struct AutoApprover(pub Decision);

#[async_trait]
impl Approver for AutoApprover {
    async fn decide(&self, _req: ApprovalRequest) -> Decision {
        self.0
    }
}

/// Always denies — used when no interactive approver is available.
pub struct DenyAllApprover;

#[async_trait]
impl Approver for DenyAllApprover {
    async fn decide(&self, _req: ApprovalRequest) -> Decision {
        Decision::DenyOnce
    }
}

/// Process-global approval bookkeeping. v2 keeps only a tiny in-memory
/// cache for `AllowAll`/`DenyAll` echoes (so a single session can see
/// its own rule before the file system flush lands) and a single-
/// writer mutex around the config so concurrent sessions don't
/// trample each other's rule writes. The durable state is
/// [`SandboxConfig`].
pub struct ApprovalStore {
    inner: Mutex<Inner>,
}

struct Inner {
    config_path: PathBuf,
    rules: Rules,
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalStore {
    /// Store backed by dataDir()/sandbox.json.
    pub fn new() -> Self {
        Self::with_config_path(crate::policy::SandboxConfig::path())
    }

    /// Store with a custom config path (tests use unique temp dirs).
    pub fn with_config_path(path: PathBuf) -> Self {
        let cfg = SandboxConfig::load_from(&path);
        ApprovalStore {
            inner: Mutex::new(Inner {
                config_path: path,
                rules: cfg.rules,
            }),
        }
    }

    /// In-memory store that never persists (unit tests).
    pub fn in_memory() -> Self {
        ApprovalStore {
            inner: Mutex::new(Inner {
                config_path: PathBuf::new(),
                rules: Rules::default(),
            }),
        }
    }

    /// Refresh the in-memory cache from disk. Concurrent writers call
    /// this first so they don't drop a sibling session's rule.
    pub fn reload(&self) {
        if let Ok(mut g) = self.inner.lock() {
            let cfg = SandboxConfig::load_from(&g.config_path);
            g.rules = cfg.rules;
        }
    }

    /// Consult the rules for `command`. Returns `Allow(rule)` if a
    /// user-level allow rule covers it (Tier 1 — run silently),
    /// `Deny(rule)` if a deny rule covers it (still Tier 2 — prompt,
    /// with the rule name in the reason), or `None` if the prompt
    /// should run with whatever reason the analyzer produced.
    pub fn classify(&self, command: &str) -> Option<RuleMatch> {
        let g = self.inner.lock().ok()?;
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: g.rules.clone(),
            path: None,
        };
        cfg.classify(command)
    }

    /// Record a decision. `AllowAll`/`DenyAll` persist a prefix rule
    /// to `sandbox.json`; `AllowOnce`/`DenyOnce` are no-ops at this
    /// layer (they live only in the prompt outcome, not in any store).
    /// `path` is the config path to persist to (typically the
    /// per-call `SandboxConfig::save_path()`).
    pub fn record(&self, command: &str, decision: Decision, path: &Path) -> anyhow::Result<()> {
        let kind = match decision {
            Decision::AllowAll => RuleKind::Allow,
            Decision::DenyAll => RuleKind::Deny,
            Decision::AllowOnce | Decision::DenyOnce => return Ok(()),
        };
        let prefix = prefix_for_command(command);
        if prefix == "*" {
            // Refuse to save a rule that would shadow literally
            // everything (a degenerate input).
            return Ok(());
        }
        // Re-read from disk first so concurrent writers don't drop
        // each other's rules.
        let mut cfg = SandboxConfig::load_from(path);
        let trimmed = prefix.trim();
        let target = match kind {
            RuleKind::Allow => &mut cfg.rules.allow,
            RuleKind::Deny => &mut cfg.rules.deny,
        };
        if !target.iter().any(|r| r == trimmed) {
            target.push(trimmed.to_string());
            target.sort();
        }
        let other = match kind {
            RuleKind::Allow => &mut cfg.rules.deny,
            RuleKind::Deny => &mut cfg.rules.allow,
        };
        other.retain(|r| r != trimmed);
        cfg.save_to(path)?;
        // Update in-memory cache if we're the configured store.
        if let Ok(mut g) = self.inner.lock() {
            if g.config_path == path {
                g.rules = cfg.rules.clone();
            }
        }
        Ok(())
    }

    /// Gate helper: consult the in-memory rule cache first (no
    /// prompt), otherwise ask the approver. Returns the decision the
    /// gate settled on. The caller is responsible for the spawn /
    /// audit side of the decision.
    pub async fn gate(
        &self,
        req: &ApprovalRequest,
        approver: &dyn Approver,
        config_path: &Path,
    ) -> Decision {
        if let Some(RuleMatch::Allow(_)) = self.classify(&req.command) {
            return Decision::AllowAll;
        }
        let decision = approver.decide(req.clone()).await;
        if let Err(e) = self.record(&req.command, decision, config_path) {
            // Persistence failures must not fail the gate — the run
            // can still go through. The next save attempt will surface
            // the issue via copied_msg / log.
            eprintln!("atom: failed to record sandbox rule: {e}");
        }
        decision
    }
}

/// Backwards-compatible key helper. v1 callers used this to build a
/// session-scoped grant key; v2 has no sessions. Returns the sha256
/// of the command text — useful for audit / dedup.
pub fn key_for(_rule_id: &str, command: &str, _cwd: &Path) -> String {
    crate::rules::command_fallback_key(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::command_fallback_key;

    #[tokio::test]
    async fn auto_and_deny_approvers() {
        let req = ApprovalRequest {
            session_id: "s".into(),
            command: "curl x".into(),
            cwd: "/ws".into(),
            rule_id: "curl".into(),
            reason: "net".into(),
            accept_all_preview: None,
        };
        assert_eq!(
            AutoApprover(Decision::AllowOnce).decide(req.clone()).await,
            Decision::AllowOnce
        );
        assert_eq!(DenyAllApprover.decide(req).await, Decision::DenyOnce);
    }

    #[tokio::test]
    async fn allow_all_persists_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        store
            .record("cargo test --release", Decision::AllowAll, &path)
            .unwrap();
        let cfg = SandboxConfig::load_from(&path);
        assert!(cfg.rules.allow.contains(&"cargo test *".to_string()));
    }

    #[tokio::test]
    async fn deny_all_persists_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        store
            .record("rm -rf /tmp/foo", Decision::DenyAll, &path)
            .unwrap();
        let cfg = SandboxConfig::load_from(&path);
        assert!(cfg.rules.deny.contains(&"rm *".to_string()));
    }

    #[tokio::test]
    async fn allow_once_does_not_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        store
            .record("cargo test --release", Decision::AllowOnce, &path)
            .unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn gate_returns_allow_all_when_classify_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        // Pre-seed an allow rule.
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules {
                allow: vec!["cargo test *".into()],
                deny: vec![],
            },
            path: Some(path.clone()),
        };
        cfg.save_to(&path).unwrap();
        let store = ApprovalStore::with_config_path(path.clone());
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        struct Counting<'a>(&'a std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl Approver for Counting<'_> {
            async fn decide(&self, _req: ApprovalRequest) -> Decision {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Decision::DenyOnce
            }
        }
        let req = ApprovalRequest {
            session_id: "s1".into(),
            command: "cargo test --release".into(),
            cwd: "/ws".into(),
            rule_id: "cargo-test-bench".into(),
            reason: "r".into(),
            accept_all_preview: None,
        };
        let d = store.gate(&req, &Counting(&prompts), &path).await;
        assert_eq!(d, Decision::AllowAll);
        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "allow rule short-circuits the prompt"
        );
    }

    #[tokio::test]
    async fn gate_prompts_when_no_rule_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        struct Counting<'a>(&'a std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl Approver for Counting<'_> {
            async fn decide(&self, _req: ApprovalRequest) -> Decision {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Decision::AllowOnce
            }
        }
        let req = ApprovalRequest {
            session_id: "s".into(),
            command: "wget http://x".into(),
            cwd: "/ws".into(),
            rule_id: "wget".into(),
            reason: "r".into(),
            accept_all_preview: None,
        };
        assert_eq!(
            store.gate(&req, &Counting(&prompts), &path).await,
            Decision::AllowOnce
        );
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn key_for_returns_command_sha256() {
        assert_eq!(
            key_for("curl", "weird -cmd", Path::new("/ws")),
            command_fallback_key("weird -cmd")
        );
    }

    #[test]
    fn legacy_wire_names_map() {
        assert_eq!(
            Decision::from_legacy_wire("allow_once"),
            Some(Decision::AllowOnce)
        );
        assert_eq!(
            Decision::from_legacy_wire("allow_session"),
            Some(Decision::AllowOnce)
        );
        assert_eq!(
            Decision::from_legacy_wire("allow_global"),
            Some(Decision::AllowAll)
        );
        assert_eq!(
            Decision::from_legacy_wire("allow_always"),
            Some(Decision::AllowAll)
        );
        assert_eq!(Decision::from_legacy_wire("deny"), Some(Decision::DenyOnce));
        assert_eq!(
            Decision::from_legacy_wire("deny_always"),
            Some(Decision::DenyAll)
        );
        assert_eq!(Decision::from_legacy_wire("nonsense"), None);
    }
}
