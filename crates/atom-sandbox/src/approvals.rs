//! Layer 2 — approval gate.
//!
//! Three decisions: [`Decision::AllowOnce`] runs without memory,
//! [`Decision::AllowSession`] also grants the command family for this
//! session (in memory only), [`Decision::DenyOnce`] refuses.
//!
//! Only the user's hand-authored `rules` block in
//! [`crate::policy::SandboxConfig`] is durable; decisions never write
//! rules.

use crate::policy::{prefix_for_command, RuleMatch, Rules, SandboxConfig};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the user (or an auto-approver) decided for one prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Run this once, no memory of the grant.
    AllowOnce,
    /// Run this + grant the command family for this session only.
    AllowSession,
    /// Refuse this run, no memory.
    #[default]
    DenyOnce,
}

impl Decision {
    pub fn allows(&self) -> bool {
        matches!(self, Decision::AllowOnce | Decision::AllowSession)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::AllowOnce => "allow_once",
            Decision::AllowSession => "allow_session",
            Decision::DenyOnce => "deny_once",
        }
    }

    /// Map older wire names onto the three-decision set: `allow_*`
    /// becomes AllowSession, `deny_*` becomes DenyOnce.
    pub fn from_legacy_wire(s: &str) -> Option<Self> {
        match s {
            "allow_once" => Some(Decision::AllowOnce),
            "allow_session" | "allow_always" | "allow_all" | "allow_global" => {
                Some(Decision::AllowSession)
            }
            "deny_once" | "deny" | "deny_always" | "deny_all" => Some(Decision::DenyOnce),
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
    /// Workspace root the command was analyzed against. An empty value
    /// falls back to `cwd`, which is what every pre-existing caller
    /// passed.
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub workspace_root: PathBuf,
    /// Matched rule id, or "" when the verdict came from the unknown-
    /// command fallback.
    pub rule_id: String,
    pub reason: String,
    /// Prefix-rule preview for `[a]` — the family a grant would cover
    /// for the rest of the session. Pre-computed by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_all_preview: Option<String>,
    /// Guardrail flag: a flagged request never consults or creates a
    /// session grant — the user answers every prompt.
    #[serde(default)]
    pub flagged: bool,
}

impl ApprovalRequest {
    /// The workspace root for analysis, falling back to `cwd` when a
    /// caller left it unset.
    pub fn workspace(&self) -> &Path {
        if self.workspace_root.as_os_str().is_empty() {
            &self.cwd
        } else {
            &self.workspace_root
        }
    }
}

fn path_is_empty(path: &PathBuf) -> bool {
    path.as_os_str().is_empty()
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

/// Process-global store: cached user rules plus the in-memory session
/// grants. Nothing here is durable — a restart forgets every grant.
pub struct ApprovalStore {
    inner: Mutex<Inner>,
}

struct Inner {
    config_path: PathBuf,
    rules: Rules,
    /// Granted (session_id, command-family prefix) pairs.
    grants: HashSet<(String, String)>,
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
                grants: HashSet::new(),
            }),
        }
    }

    /// In-memory store that never persists (unit tests).
    pub fn in_memory() -> Self {
        ApprovalStore {
            inner: Mutex::new(Inner {
                config_path: PathBuf::new(),
                rules: Rules::default(),
                grants: HashSet::new(),
            }),
        }
    }

    /// Refresh the cached rules from disk (the user may have edited
    /// sandbox.json while the server runs).
    pub fn reload(&self) {
        if let Ok(mut g) = self.inner.lock() {
            let cfg = SandboxConfig::load_from(&g.config_path);
            g.rules = cfg.rules;
        }
    }

    /// Consult the user's rules: `Allow` runs the command silently,
    /// `Deny` keeps it in Tier 2 with the rule name as the reason, and
    /// `None` leaves the analyzer's reason alone.
    pub fn classify(&self, command: &str) -> Option<RuleMatch> {
        let g = self.inner.lock().ok()?;
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: g.rules.clone(),
            confine: true,
            path: None,
        };
        cfg.classify(command)
    }

    /// Record a decision: only `AllowSession` on an unflagged command
    /// creates a grant, and only for this session. Nothing is written
    /// to sandbox.json — that file is for hand-authored rules.
    pub fn record(&self, session_id: &str, command: &str, flagged: bool, decision: Decision) {
        if flagged || decision != Decision::AllowSession {
            return;
        }
        let prefix = prefix_for_command(command);
        if prefix == "*" {
            // A grant covering everything is a degenerate input.
            return;
        }
        if let Ok(mut g) = self.inner.lock() {
            g.grants.insert((session_id.to_string(), prefix));
        }
    }

    /// Drop every grant from one session, so deleted sessions don't
    /// leak into the process-global store.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.grants.retain(|(sid, _)| sid != session_id);
        }
    }

    /// Consult the user's rules, then this session's grants; if neither
    /// applies, ask the approver. Flagged requests skip both shortcuts
    /// — they always prompt — and their answer never becomes a grant.
    pub async fn gate(&self, req: &ApprovalRequest, approver: &dyn Approver) -> Decision {
        if !req.flagged {
            if let Some(RuleMatch::Allow(_)) = self.classify(&req.command) {
                return Decision::AllowOnce;
            }
            let prefix = prefix_for_command(&req.command);
            if let Ok(g) = self.inner.lock() {
                if g.grants.contains(&(req.session_id.clone(), prefix)) {
                    return Decision::AllowSession;
                }
            }
        }
        let decision = approver.decide(req.clone()).await;
        self.record(&req.session_id, &req.command, req.flagged, decision);
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

    /// Test-only approver: counts prompts and returns a fixed decision.
    struct Counting<'a>(&'a std::sync::atomic::AtomicUsize, Decision);
    #[async_trait]
    impl Approver for Counting<'_> {
        async fn decide(&self, _req: ApprovalRequest) -> Decision {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.1
        }
    }

    #[tokio::test]
    async fn auto_and_deny_approvers() {
        let req = ApprovalRequest {
            session_id: "s".into(),
            command: "curl x".into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "curl".into(),
            reason: "net".into(),
            accept_all_preview: None,
            flagged: false,
        };
        assert_eq!(
            AutoApprover(Decision::AllowOnce).decide(req.clone()).await,
            Decision::AllowOnce
        );
        assert_eq!(DenyAllApprover.decide(req).await, Decision::DenyOnce);
    }

    #[tokio::test]
    async fn allow_session_grant_skips_prompt_in_same_session_only() {
        let store = ApprovalStore::in_memory();
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let mk = |sid: &str, cmd: &str| ApprovalRequest {
            session_id: sid.into(),
            command: cmd.into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "awk".into(),
            reason: "r".into(),
            accept_all_preview: None,
            flagged: false,
        };
        let cmd = "awk 'BEGIN{print 1}'";
        // First ask in session s1: prompts, user grants for the session.
        let d = store
            .gate(&mk("s1", cmd), &Counting(&prompts, Decision::AllowSession))
            .await;
        assert_eq!(d, Decision::AllowSession);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Second ask in s1: grant covers the family, no prompt.
        let d = store
            .gate(&mk("s1", cmd), &Counting(&prompts, Decision::DenyOnce))
            .await;
        assert_eq!(d, Decision::AllowSession);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Same family in a different session: still prompts.
        let d = store
            .gate(&mk("s2", cmd), &Counting(&prompts, Decision::DenyOnce))
            .await;
        assert_eq!(d, Decision::DenyOnce);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn forget_session_drops_only_that_sessions_grants() {
        let store = ApprovalStore::in_memory();
        let cmd = "awk 'BEGIN{print 1}'";
        let prefix = prefix_for_command(cmd);
        store.record("s1", cmd, false, Decision::AllowSession);
        store.record("s2", cmd, false, Decision::AllowSession);
        store.forget_session("s1");
        let granted = |sid: &str| {
            store
                .inner
                .lock()
                .unwrap()
                .grants
                .contains(&(sid.to_string(), prefix.clone()))
        };
        assert!(!granted("s1"), "forgotten session's grant must be gone");
        assert!(granted("s2"), "other sessions keep their grants");
    }

    #[tokio::test]
    async fn allow_once_and_deny_once_do_not_grant() {
        let store = ApprovalStore::in_memory();
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let mk = |cmd: &str| ApprovalRequest {
            session_id: "s".into(),
            command: cmd.into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "awk".into(),
            reason: "r".into(),
            accept_all_preview: None,
            flagged: false,
        };
        for d in [Decision::AllowOnce, Decision::DenyOnce] {
            assert_eq!(
                store
                    .gate(&mk("awk 'BEGIN{print 1}'"), &Counting(&prompts, d))
                    .await,
                d
            );
        }
        // Both asks prompted; neither left a grant behind.
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            store
                .gate(
                    &mk("awk 'BEGIN{print 1}'"),
                    &Counting(&prompts, Decision::DenyOnce)
                )
                .await,
            Decision::DenyOnce
        );
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn record_never_writes_sandbox_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        store.record("s1", "cargo test --release", false, Decision::AllowSession);
        store.record("s1", "rm -rf /tmp/foo", false, Decision::DenyOnce);
        assert!(!path.exists(), "decisions must not persist rules");
    }

    #[tokio::test]
    async fn accept_all_on_flagged_command_grants_nothing() {
        let store = ApprovalStore::in_memory();
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let cmd = "rm -rf /tmp/foo";
        let prefix = prefix_for_command(cmd);
        let mk = |flagged: bool| ApprovalRequest {
            session_id: "s".into(),
            command: cmd.into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "rm-root".into(),
            reason: "removes the filesystem root".into(),
            accept_all_preview: Some(prefix.clone()),
            flagged,
        };
        // Accept All on the flagged guardrail: runs, but records no grant.
        let d = store
            .gate(&mk(true), &Counting(&prompts, Decision::AllowSession))
            .await;
        assert_eq!(d, Decision::AllowSession);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let granted = store
            .inner
            .lock()
            .unwrap()
            .grants
            .contains(&("s".to_string(), prefix.clone()));
        assert!(!granted, "flagged Accept All must not create a grant");
        // Same command prompts again — the grant never existed.
        let d = store
            .gate(&mk(true), &Counting(&prompts, Decision::DenyOnce))
            .await;
        assert_eq!(d, Decision::DenyOnce);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn accept_all_on_unflagged_tier2_still_grants() {
        let store = ApprovalStore::in_memory();
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let cmd = "awk 'BEGIN{print 1}'";
        let prefix = prefix_for_command(cmd);
        let mk = || ApprovalRequest {
            session_id: "s".into(),
            command: cmd.into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "unknown-command".into(),
            reason: "r".into(),
            accept_all_preview: Some(prefix.clone()),
            flagged: false,
        };
        let d = store
            .gate(&mk(), &Counting(&prompts, Decision::AllowSession))
            .await;
        assert_eq!(d, Decision::AllowSession);
        // The grant covers the family: no prompt next time.
        let d = store
            .gate(&mk(), &Counting(&prompts, Decision::DenyOnce))
            .await;
        assert_eq!(d, Decision::AllowSession);
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gate_prompts_when_no_rule_or_grant_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path);
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let req = ApprovalRequest {
            session_id: "s".into(),
            command: "wget http://x".into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "wget".into(),
            reason: "r".into(),
            accept_all_preview: None,
            flagged: false,
        };
        assert_eq!(
            store
                .gate(&req, &Counting(&prompts, Decision::AllowOnce))
                .await,
            Decision::AllowOnce
        );
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gate_short_circuits_on_user_allow_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        // Pre-seed a hand-authored allow rule.
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules {
                allow: vec!["cargo test *".into()],
                deny: vec![],
            },
            confine: true,
            path: Some(path.clone()),
        };
        cfg.save_to(&path).unwrap();
        let store = ApprovalStore::with_config_path(path);
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        let req = ApprovalRequest {
            session_id: "s1".into(),
            command: "cargo test --release".into(),
            cwd: "/ws".into(),
            workspace_root: "/ws".into(),
            rule_id: "cargo-test-bench".into(),
            reason: "r".into(),
            accept_all_preview: None,
            flagged: false,
        };
        let d = store
            .gate(&req, &Counting(&prompts, Decision::DenyOnce))
            .await;
        assert_eq!(d, Decision::AllowOnce);
        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "allow rule short-circuits the prompt"
        );
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
        for s in ["allow_session", "allow_always", "allow_all", "allow_global"] {
            assert_eq!(
                Decision::from_legacy_wire(s),
                Some(Decision::AllowSession),
                "{s}"
            );
        }
        for s in ["deny", "deny_once", "deny_always", "deny_all"] {
            assert_eq!(
                Decision::from_legacy_wire(s),
                Some(Decision::DenyOnce),
                "{s}"
            );
        }
        assert_eq!(Decision::from_legacy_wire("nonsense"), None);
    }

    #[test]
    fn as_str_is_canonical() {
        assert_eq!(Decision::AllowOnce.as_str(), "allow_once");
        assert_eq!(Decision::AllowSession.as_str(), "allow_session");
        assert_eq!(Decision::DenyOnce.as_str(), "deny_once");
    }
}
