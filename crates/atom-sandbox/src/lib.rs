//! atom-sandbox: static analysis + approval gate for tool calls.
//!
//! Layer 1 — static rule table (`rules.rs`) over tokenized commands:
//! matching commands run silently, the rest prompt. Guardrail rules
//! (recursive deletes, privilege escalation, credential exfil,
//! path-escape writes, …) flag the command instead — it always prompts
//! and never inherits a session grant.
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
