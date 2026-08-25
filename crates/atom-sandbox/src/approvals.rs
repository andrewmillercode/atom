//! Layer 2 — approval gate. Session-scoped "always allow" plus a
//! persisted global set; other sessions always re-prompt.

use crate::rules::command_fallback_key;
use async_trait::async_trait;
use atom_core::session::store::data_dir;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the user (or an auto-approver) decided for one prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    AllowSession,
    AllowGlobal,
    Deny,
}

impl Decision {
    pub fn allows(&self) -> bool {
        !matches!(self, Decision::Deny)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::AllowOnce => "allow_once",
            Decision::AllowSession => "allow_session",
            Decision::AllowGlobal => "allow_global",
            Decision::Deny => "deny",
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
        Decision::Deny
    }
}

/// Approval bookkeeping:
/// - session-scoped grants live in memory only, keyed `(session_id, key)`
///   so an allow in session A never satisfies session B;
/// - global grants persist to `dataDir()/approvals.json`, written ONLY
///   when a decision is AllowGlobal.
pub struct ApprovalStore {
    sessions: Mutex<HashMap<(String, String), ()>>,
    globals: Mutex<HashSet<String>>,
    global_path: Option<PathBuf>,
}

/// Stable key for a grant: matched rule id when present, otherwise the
/// sha256 of the command text — combined with the cwd per the spec.
pub fn key_for(rule_id: &str, command: &str, cwd: &Path) -> String {
    let id_part = if rule_id.is_empty() {
        command_fallback_key(command)
    } else {
        rule_id.to_string()
    };
    format!("{}\u{1f}{}", id_part, cwd.display())
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalStore {
    /// Store backed by dataDir()/approvals.json.
    pub fn new() -> Self {
        Self::with_global_path(data_dir().join("approvals.json"))
    }

    /// Store with a custom persistence path (tests use unique temp dirs).
    pub fn with_global_path(path: PathBuf) -> Self {
        let globals = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HashSet<String>>(&bytes).ok())
            .unwrap_or_default();
        ApprovalStore {
            sessions: Mutex::new(HashMap::new()),
            globals: Mutex::new(globals),
            global_path: Some(path),
        }
    }

    /// In-memory store that never persists (unit tests).
    pub fn in_memory() -> Self {
        ApprovalStore {
            sessions: Mutex::new(HashMap::new()),
            globals: Mutex::new(HashSet::new()),
            global_path: None,
        }
    }

    /// Has this session (or the global set) already approved `key`?
    pub fn check(&self, session_id: &str, key: &str) -> bool {
        if self
            .sessions
            .lock()
            .map(|s| s.contains_key(&(session_id.to_string(), key.to_string())))
            .unwrap_or(false)
        {
            return true;
        }
        self.globals
            .lock()
            .map(|g| g.contains(key))
            .unwrap_or(false)
    }

    /// Record a decision. Only AllowSession and AllowGlobal have lasting
    /// effect; AllowGlobal is the sole trigger for a disk write.
    pub fn record(&self, session_id: &str, key: &str, decision: Decision) {
        match decision {
            Decision::AllowOnce => {}
            Decision::AllowSession => {
                if let Ok(mut s) = self.sessions.lock() {
                    s.insert((session_id.to_string(), key.to_string()), ());
                }
            }
            Decision::AllowGlobal => {
                if let Ok(mut g) = self.globals.lock() {
                    g.insert(key.to_string());
                    self.persist_globals_locked(&g);
                }
            }
            Decision::Deny => {}
        }
    }

    /// Gate helper used by exec.rs: consult the store first (no prompt),
    /// otherwise ask the approver and record the outcome.
    pub async fn gate(
        &self,
        req: &ApprovalRequest,
        key: &str,
        approver: &dyn Approver,
    ) -> Decision {
        if self.check(&req.session_id, key) {
            return Decision::AllowSession;
        }
        let decision = approver.decide(req.clone()).await;
        self.record(&req.session_id, key, decision);
        decision
    }

    fn persist_globals_locked(&self, globals: &HashSet<String>) {
        let Some(path) = &self.global_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut sorted: Vec<&String> = globals.iter().collect();
        sorted.sort();
        if let Ok(json) = serde_json::to_vec_pretty(&sorted) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "curl\x1f/ws";

    #[tokio::test]
    async fn auto_and_deny_approvers() {
        let req = ApprovalRequest {
            session_id: "s".into(),
            command: "curl x".into(),
            cwd: "/ws".into(),
            rule_id: "curl".into(),
            reason: "net".into(),
        };
        assert_eq!(
            AutoApprover(Decision::AllowOnce).decide(req.clone()).await,
            Decision::AllowOnce
        );
        assert_eq!(DenyAllApprover.decide(req).await, Decision::Deny);
    }

    #[tokio::test]
    async fn session_allow_does_not_leak_to_other_sessions() {
        let store = ApprovalStore::in_memory();
        store.record("session-a", KEY, Decision::AllowSession);
        assert!(store.check("session-a", KEY));
        assert!(
            !store.check("session-b", KEY),
            "session A allow leaked to B"
        );
        // Global grants satisfy every session...
        store.record("session-a", KEY, Decision::AllowGlobal);
        assert!(store.check("session-b", KEY));
    }

    #[tokio::test]
    async fn gate_consults_store_before_approver() {
        let store = ApprovalStore::in_memory();
        let prompts = std::sync::atomic::AtomicUsize::new(0);
        struct Counting<'a>(&'a std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl Approver for Counting<'_> {
            async fn decide(&self, _req: ApprovalRequest) -> Decision {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Decision::AllowSession
            }
        }
        let req = ApprovalRequest {
            session_id: "s1".into(),
            command: "wget http://x".into(),
            cwd: "/ws".into(),
            rule_id: "wget".into(),
            reason: "r".into(),
        };
        let key = key_for("wget", &req.command, &req.cwd);
        assert_eq!(
            store.gate(&req, &key, &Counting(&prompts)).await,
            Decision::AllowSession
        );
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Second ask in same session: satisfied from the store, no prompt.
        assert_eq!(
            store.gate(&req, &key, &Counting(&prompts)).await,
            Decision::AllowSession
        );
        assert_eq!(prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn global_grants_persist_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.json");
        assert!(!path.exists());
        let first = ApprovalStore::with_global_path(path.clone());
        assert!(!first.check("any", KEY));
        // AllowGlobal writes the file; AllowSession must not.
        first.record("s", KEY, Decision::AllowGlobal);
        first.record("s", "other-key\x1f/x", Decision::AllowSession);
        assert!(path.exists(), "AllowGlobal must persist");

        let second = ApprovalStore::with_global_path(path);
        assert!(
            second.check("fresh-session", KEY),
            "global round trip failed"
        );
        assert!(
            !second.check("fresh-session", "other-key\x1f/x"),
            "session-scoped grant must not be persisted"
        );
    }

    #[test]
    fn deny_records_nothing() {
        let store = ApprovalStore::in_memory();
        store.record("s", KEY, Decision::Deny);
        store.record("s", KEY, Decision::AllowOnce);
        assert!(!store.check("s", KEY));
    }

    #[test]
    fn key_for_prefers_rule_id_then_hashes_command() {
        assert_eq!(
            key_for("curl", "curl x", Path::new("/ws")),
            format!("curl\u{1f}/ws")
        );
        let k = key_for("", "weird -cmd", Path::new("/w"));
        assert!(k.starts_with(&command_fallback_key("weird -cmd")));
        assert!(k.ends_with("/w"));
    }
}
