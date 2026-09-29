//! `atom -mcp-bridge`: the stdio MCP server an ACP agent launches (see
//! `acp_turn::acp_session_params`) to reach atom's `subagent` tool.
//! Tool calls are forwarded to the session server over its unix socket.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub const SESSION_ENV: &str = "_ATOM_MCP_SESSION";
pub const SOCKET_ENV: &str = "_ATOM_MCP_SOCKET";

const PROTOCOL_VERSION: &str = "2025-06-18";

// Outlasts the server's own wait=all deadline so it always answers first.
const CALL_TIMEOUT: Duration =
    Duration::from_secs(crate::dispatch::RESULT_WAIT_TIMEOUT.as_secs() + 60);

/// run_mcp_bridge serves newline-delimited JSON-RPC on stdio until the
/// agent closes stdin. Requests run concurrently so a blocking
/// `wait=all` call never stalls pings or other calls.
pub async fn run_mcp_bridge(session_id: String, socket: PathBuf) -> anyhow::Result<()> {
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let session_id = Arc::new(session_id);
    let socket = Arc::new(socket);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications (no id) and responses (no method) need no reply.
        let (Some(id), Some(method)) = (
            msg.get("id").cloned(),
            msg.get("method")
                .and_then(Value::as_str)
                .map(str::to_string),
        ) else {
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let (stdout, session_id, socket) = (stdout.clone(), session_id.clone(), socket.clone());
        tokio::spawn(async move {
            let reply = match handle(&method, &params, &session_id, &socket).await {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": code, "message": message},
                }),
            };
            let mut line = reply.to_string();
            line.push('\n');
            let mut out = stdout.lock().await;
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.flush().await;
        });
    }
    Ok(())
}

async fn handle(
    method: &str,
    params: &Value,
    session_id: &str,
    socket: &Path,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!(PROTOCOL_VERSION)),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "atom", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let def = atom_tools::defs::subagent_def().function;
            Ok(json!({"tools": [{
                "name": def.name,
                "description": def.description,
                "inputSchema": def.parameters,
            }]}))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name != "subagent" {
                return Err((-32602, format!("unknown tool: {name}")));
            }
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let text = match crate::client::post_on(
                socket,
                CALL_TIMEOUT,
                &format!("/api/sessions/{session_id}/subagent"),
                &json!({"arguments": arguments}),
            )
            .await
            {
                Ok(v) => v
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Err(err) => format!("error: atom server: {err}"),
            };
            Ok(json!({
                "content": [{"type": "text", "text": text}],
                "isError": text.starts_with("error"),
            }))
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_subagent_tool_with_schema() {
        let res = handle("tools/list", &Value::Null, "s", Path::new("/nonexistent"))
            .await
            .unwrap();
        let tool = &res["tools"][0];
        assert_eq!(tool["name"], "subagent");
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn initialize_echoes_protocol_version() {
        let res = handle(
            "initialize",
            &json!({"protocolVersion": "2025-03-26"}),
            "s",
            Path::new("/nonexistent"),
        )
        .await
        .unwrap();
        assert_eq!(res["protocolVersion"], "2025-03-26");
        assert!(res["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn call_reports_server_errors_as_tool_errors() {
        let res = handle(
            "tools/call",
            &json!({"name": "subagent", "arguments": {"action": "inspect"}}),
            "s",
            Path::new("/nonexistent/atom.sock"),
        )
        .await
        .unwrap();
        assert_eq!(res["isError"], true);
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("error: atom server"));
    }

    #[tokio::test]
    async fn rejects_unknown_tools_and_methods() {
        let sock = Path::new("/nonexistent");
        let err = handle("tools/call", &json!({"name": "bash"}), "s", sock)
            .await
            .unwrap_err();
        assert_eq!(err.0, -32602);
        let err = handle("resources/list", &Value::Null, "s", sock)
            .await
            .unwrap_err();
        assert_eq!(err.0, -32601);
    }
}
