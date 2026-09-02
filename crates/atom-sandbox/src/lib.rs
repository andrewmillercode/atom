//! atom-sandbox: static analysis + approval gate for tool calls.
//!
//! Layer 1 — static rule table (`rules.rs`) over tokenized commands:
//! Tier 1 (silent allow) / Tier 2 (prompt) / guardrail (Deny). Wide
//! allowlist of reads, builds, package installs, network fetches,
//! local VCS, FS creation, system read-only, dev helpers; guardrails
//! floor covers recursive deletes, privilege escalation, process
//! kill, system automation, credential exfil, keychain, network-to-
//! interpreter, and path-escape writes.
//!
//! Layer 2 — approval gate (`approvals.rs`): four buttons
//! (AllowOnce / AllowAll / DenyOnce / DenyAll) backed by user
//! prefix-rules in `sandbox.json`.
//!
//! Layer 3 — `exec.rs` runs the pipeline `analyze → guardrail floor →
//! approval gate → spawn → audit`. v2 drops kernel confinement; the
//! guardrail floor replaces the deny-by-default sandbox. Subprocess
//! env is scrubbed of provider credentials before each spawn.

pub mod approvals;
pub mod exec;
pub mod policy;
pub mod protected;
pub mod rules;
