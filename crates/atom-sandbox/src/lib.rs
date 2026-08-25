//! atom-sandbox: multi-layer execution sandboxing for tool calls.
//!
//! Layer 1 — static analysis + permission rules (pattern-based
//!   allow/ask/deny with command arity checks).
//! Layer 2 — approval gate (session-scoped allow, user prompt otherwise).
//! Layer 3 — kernel confinement via macOS Seatbelt profiles.

pub mod approvals;
pub mod exec;
pub mod policy;
pub mod rules;
pub mod seatbelt;
