//! atom-tools: tool definitions and execution, ported from main.go
//! executeToolFor/builtinToolDefinitions plus file_edit.go, search.go,
//! skills.go, mcp.go, clipboard.go, vector_search.go and dispatch.go.
//!
//! The bash tool routes through [`atom_sandbox::exec::run`]; everything
//! else mirrors Go's model-visible strings.

pub mod clipboard;
pub mod customize;
pub mod defs;
pub mod dispatch;
pub mod exec;
pub mod file_edit;
pub mod mcp;
pub mod mcp_oauth;
pub mod read_file;
pub mod search;
pub mod skills;
pub mod vector_search;
pub mod visualize;
pub mod web_fetch;
pub mod web_search;

pub use dispatch::{is_dispatch_session_id, parse_dispatch_session_id, DispatchPlan};
pub use exec::{execute_tool, SubagentHandle, ToolCtx, ToolOutcome};
pub use file_edit::FileSeen;
pub use mcp::{close_all_mcp, has_deferred_tools};

use atom_core::types::ToolDef;
use std::path::Path;

/// toolDefinitions returns builtins only (no MCP). Existing tests use this.
pub fn tool_definitions() -> Vec<ToolDef> {
    defs::builtin_tool_definitions()
}

/// toolDefinitionsFor returns builtins plus MCP tools discovered for cwd.
pub async fn tool_definitions_with_mcp(cwd: &Path) -> Vec<ToolDef> {
    let mut out = defs::builtin_tool_definitions();
    let selected = atom_core::config::load().resolved_web_search();
    let hidden_name = mcp::sanitize_mcp_name(&selected.server, &selected.tool);
    out.extend(
        mcp::mcp_tools_for(cwd)
            .await
            .into_iter()
            .filter(|tool| tool.function.name != hidden_name),
    );
    out
}
