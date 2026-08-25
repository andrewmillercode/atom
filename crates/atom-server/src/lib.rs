//! atom-server: unix-socket session server, ported from server.go.
//!
//! Route table, NDJSON event protocol, turn management (pausing,
//! compaction interrupts), dispatch subagent bridge, and the sandbox
//! approval bridge all mirror the Go implementation; `client` is the
//! self-contained unix-socket HTTP client used by the bin/TUI.

pub mod cancel;
pub mod client;
pub mod dispatch;
pub mod http;
pub mod instructions;
pub mod state;
pub mod turn;

pub use state::{AppState, ConnTracker};
