//! atom-sandbox: static analysis + approval gate for tool calls.
//!
//! Layer 1 — static rule table (`rules.rs`) over tokenized commands:
//! matching commands run silently, the rest prompt. Guardrail rules
//! (recursive deletes, privilege escalation, credential exfil, …) flag
//! the command instead — it always prompts and never inherits a session
//! grant. Writes outside the workspace ask without the flag: they reach
//! the reviewer like any Tier-2 command, and an answer can grant the
//! family.
//!
//! Layer 2 — approval gate (`approvals.rs`): three buttons,
//! AllowOnce / AllowSession / DenyOnce. AllowSession grants the command
//! family for the current session in memory; `sandbox.json` holds only
//! the user's hand-authored rules.
//!
//! Layer 3 — `exec.rs` runs the pipeline `analyze → approval gate →
//! spawn → audit`, scrubbing provider credentials from the child env.

pub mod approvals;
pub mod exec;
pub mod policy;
pub mod protected;
pub mod rules;
pub mod seatbelt;

/// Shared lock for tests that mutate process-global env vars (exec.rs
/// scrubs ATOM_TEST_* keys, protected.rs redirects PATH): parallel
/// tests would otherwise clobber each other's environment — a clobbered
/// PATH even breaks command lookups in exec tests. Every env-mutating
/// test must hold the guard for its whole body and restore the prior
/// value when done.
#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
