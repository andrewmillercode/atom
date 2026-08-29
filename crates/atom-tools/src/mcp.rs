//! MCP client support, ported from mcp.go: config discovery with
//! override semantics, sanitized/unique tool names (mcp_<server>_<tool>),
//! a connection hub cached per (server, cwd), and stdio/streamable-HTTP
//! JSON-RPC transports. Disabled servers are skipped; failed connects are
//! remembered so they don't retry on every turn.

use atom_core::types::ToolDef;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPServerConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "type", default)]
    pub typ: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Statically registered OAuth client id; skips dynamic client
    /// registration when the remote server requires OAuth.
    #[serde(default)]
    pub client_id: String,
    /// "oauth" opts this server into the interactive browser sign-in on
    /// 401. Stored tokens are always used when present; without this the
    /// server must be authorized by other means (headers/env).
    #[serde(default)]
    pub auth: String,
    /// defer: true always defers this server's tools behind find_tool;
    /// false never defers; unset defers automatically when the server
    /// exposes more than AUTO_DEFER_TOOLS. Deferred tools are invisible
    /// until the model discovers them via find_tool.
    #[serde(default)]
    pub defer: Option<bool>,
}

/// Servers above this tool count defer their tools to find_tool by
/// default; the per-server `defer` config overrides the heuristic.
pub const AUTO_DEFER_TOOLS: usize = 20;

fn server_deferred(cfg: &MCPServerConfig, tool_count: usize) -> bool {
    match cfg.defer {
        Some(true) => true,
        Some(false) => false,
        None => tool_count > AUTO_DEFER_TOOLS,
    }
}

#[derive(Debug, Clone)]
pub struct McpToolRef {
    pub server: String,
    pub name: String,
}

/// One listed remote tool before name conversion.
#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Config discovery.
// ---------------------------------------------------------------------------

pub(crate) fn cache_key(name: &str, cwd: &str) -> String {
    let cwd = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| PathBuf::from(cwd))
        .to_string_lossy()
        .into_owned();
    format!("{name}\x00{cwd}")
}

pub fn load_mcp_configs(cwd: &str) -> BTreeMap<String, MCPServerConfig> {
    let config = crate::skills::atom_config_dir();
    let home = dirs::home_dir();
    load_mcp_configs_in(cwd, config.as_deref(), home.as_deref())
}

pub fn load_mcp_configs_in(
    cwd: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> BTreeMap<String, MCPServerConfig> {
    let mut out = BTreeMap::new();
    if let Some(dir) = config_dir {
        merge_mcp_file(&mut out, &dir.join("mcp.json"));
    }
    let dirs = crate::skills::walk_project_dirs_in(cwd, home);
    for d in dirs.iter().rev() {
        merge_mcp_file(&mut out, &d.join(".atom").join("mcp.json"));
        merge_mcp_file(&mut out, &d.join(".cursor").join("mcp.json"));
    }
    out
}

fn merge_mcp_file(out: &mut BTreeMap<String, MCPServerConfig>, path: &Path) {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };
    #[derive(Deserialize)]
    struct File {
        #[serde(rename = "mcpServers", default)]
        mcpservers: BTreeMap<String, serde_json::Value>,
    }
    let Ok(file) = serde_json::from_slice::<File>(&b) else {
        return;
    };
    for (name, raw) in file.mcpservers {
        let Ok(mut cfg) = serde_json::from_value::<MCPServerConfig>(raw) else {
            continue;
        };
        if cfg.disabled {
            out.remove(&name);
            continue;
        }
        if cfg.env.is_empty() && !cfg.environment.is_empty() {
            cfg.env = cfg.environment.clone();
        }
        out.insert(name, cfg);
    }
}

/// expandEnvRefs replaces OpenCode-style {env:NAME} tokens with the
/// matching process environment value.
pub fn expand_env_refs(s: &str) -> String {
    let mut s = s.to_string();
    loop {
        let Some(i) = s.find("{env:") else {
            return s;
        };
        let Some(rel) = s[i..].find('}') else {
            return s;
        };
        let j = i + rel;
        let name = &s[i + 5..j];
        let value = std::env::var(name).unwrap_or_default();
        s = format!("{}{}{}", &s[..i], value, &s[j + 1..]);
    }
}

pub fn expand_env_map(in_map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    in_map
        .iter()
        .map(|(k, v)| (k.clone(), expand_env_refs(v)))
        .collect()
}

// ---------------------------------------------------------------------------
// Name sanitization and tool conversion.
// ---------------------------------------------------------------------------

fn is_mcp_name_rune(r: char) -> bool {
    r.is_ascii_alphanumeric() || r == '_' || r == '-'
}

pub fn sanitize_mcp_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp_{}_{}", server, tool);
    let mut b = String::new();
    let mut prev_underscore = false;
    for r in raw.chars() {
        if is_mcp_name_rune(r) {
            if r == '_' {
                if prev_underscore {
                    continue;
                }
                prev_underscore = true;
            } else {
                prev_underscore = false;
            }
            b.push(r);
        } else if !prev_underscore {
            b.push('_');
            prev_underscore = true;
        }
    }
    let s = b.trim_matches('_').to_string();
    if s.is_empty() {
        return "mcp_tool".to_string();
    }
    s
}

pub fn unique_mcp_name(base: &str, used: &mut HashMap<String, bool>) -> String {
    if !used.get(base).copied().unwrap_or(false) {
        used.insert(base.to_string(), true);
        return base.to_string();
    }
    let mut i = 2;
    loop {
        let n = format!("{base}_{i}");
        if !used.get(&n).copied().unwrap_or(false) {
            used.insert(n.clone(), true);
            return n;
        }
        i += 1;
    }
}

fn empty_params() -> serde_json::Value {
    serde_json::from_str("{\"type\":\"object\",\"properties\":{}}").unwrap()
}

pub fn mcp_params_json(schema: Option<&serde_json::Value>) -> serde_json::Value {
    match schema {
        None | Some(serde_json::Value::Null) => empty_params(),
        Some(v) => {
            let b = v.to_string();
            if b.is_empty() || b == "null" {
                empty_params()
            } else {
                v.clone()
            }
        }
    }
}

pub fn convert_mcp_tools(
    server: &str,
    tools: &[McpTool],
    used: &mut HashMap<String, bool>,
) -> (Vec<ToolDef>, HashMap<String, McpToolRef>) {
    let mut out = Vec::with_capacity(tools.len());
    let mut mapping = HashMap::new();
    for t in tools {
        if t.name.is_empty() {
            continue;
        }
        let sanitized = unique_mcp_name(&sanitize_mcp_name(server, &t.name), used);
        let desc = if t.description.is_empty() {
            t.name.clone()
        } else {
            t.description.clone()
        };
        out.push(ToolDef {
            kind: "function".to_string(),
            function: atom_core::types::ToolDefFunction {
                name: sanitized.clone(),
                description: format!("[mcp:{server}] {desc}"),
                parameters: mcp_params_json(t.input_schema.as_ref()),
            },
        });
        mapping.insert(
            sanitized,
            McpToolRef {
                server: server.to_string(),
                name: t.name.clone(),
            },
        );
    }
    (out, mapping)
}

// ---------------------------------------------------------------------------
// Transports.
// ---------------------------------------------------------------------------

/// Formatted content items of a CallToolResult (Go's formatMCPResult).
pub struct McpCallResult {
    pub parts: Vec<String>,
    pub is_error: bool,
}

pub(crate) enum Transport {
    Stdio(Box<StdioSession>),
    Http(HttpSession),
}

pub(crate) struct StdioSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub(crate) struct HttpSession {
    client: reqwest::Client,
    url: String,
    session_id: Option<String>,
    extra_headers: BTreeMap<String, String>,
    next_id: u64,
}

impl Transport {
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match self {
            Transport::Stdio(s) => s.request(method, params).await,
            Transport::Http(s) => s.request(method, params).await,
        }
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "atom", "version": "1.0.0"},
            }),
        )
        .await
        .map(|_| ())
    }

    fn is_http(&self) -> bool {
        matches!(self, Transport::Http(_))
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        match self {
            Transport::Stdio(s) => s.list_tools().await,
            Transport::Http(s) => s.list_tools().await,
        }
    }

    async fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<McpCallResult, String> {
        match self {
            Transport::Stdio(s) => s.call_tool(name, args).await,
            Transport::Http(s) => s.call_tool(name, args).await,
        }
    }
}

impl StdioSession {
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use tokio::io::AsyncWriteExt;
        self.next_id += 1;
        let id = self.next_id;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())?;
        loop {
            let line = match self.stdout.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => return Err("server closed stdout".to_string()),
                Err(e) => return Err(e.to_string()),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if v.get("id") != Some(&serde_json::json!(id)) {
                continue; // notifications / other responses
            }
            if let Some(err) = v.get("error") {
                return Err(err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("tool error")
                    .to_string());
            }
            return Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }

    async fn notify_initialized(&mut self) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let mut all = Vec::new();
        let mut cursor = String::new();
        loop {
            let params = if cursor.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({"cursor": cursor})
            };
            let res = self.request("tools/list", params).await?;
            let tools: Vec<McpTool> = res
                .get("tools")
                .and_then(|t| serde_json::from_value(t.clone()).ok())
                .unwrap_or_default();
            all.extend(tools);
            cursor = res
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                break;
            }
        }
        Ok(all)
    }

    async fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<McpCallResult, String> {
        let res = self
            .request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": args}),
            )
            .await?;
        Ok(parse_call_result(&res))
    }
}

fn parse_call_result(res: &serde_json::Value) -> McpCallResult {
    let mut parts = Vec::new();
    if let Some(content) = res.get("content").and_then(|c| c.as_array()) {
        for c in content {
            let ty = c.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match ty {
                "text" => parts.push(
                    c.get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                "image" => parts.push("[image]".to_string()),
                "audio" => parts.push("[audio]".to_string()),
                "resource" | "resource_link" => parts.push("[resource]".to_string()),
                _ => parts.push("[content]".to_string()),
            }
        }
    }
    McpCallResult {
        parts,
        is_error: res
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
    }
}

pub fn format_mcp_result(res: Option<&McpCallResult>) -> String {
    let Some(res) = res else {
        return String::new();
    };
    let out = res.parts.join("\n");
    if res.is_error {
        if out.is_empty() {
            return "error: tool failed".to_string();
        }
        return format!("error: {out}");
    }
    out
}

impl HttpSession {
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut req = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            let detail: String = text.chars().take(500).collect();
            return Err(format!(
                "HTTP {}{}",
                status.as_u16(),
                if detail.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.trim())
                }
            ));
        }

        let payload = if content_type.contains("text/event-stream") {
            extract_sse_data(&text, id)?
        } else {
            text
        };
        let trimmed = payload.trim();
        if trimmed.is_empty() {
            return Err("empty response".to_string());
        }
        let v: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|e| format!("bad json-rpc response: {e}"))?;
        if v.get("id") != Some(&serde_json::json!(id)) {
            return Err("response id mismatch".to_string());
        }
        if let Some(err) = v.get("error") {
            return Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("request failed")
                .to_string());
        }
        Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let mut all = Vec::new();
        let mut cursor = String::new();
        loop {
            let params = if cursor.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({"cursor": cursor})
            };
            let res = self.request("tools/list", params).await?;
            let tools: Vec<McpTool> = res
                .get("tools")
                .and_then(|t| serde_json::from_value(t.clone()).ok())
                .unwrap_or_default();
            all.extend(tools);
            cursor = res
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                break;
            }
        }
        Ok(all)
    }

    async fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<McpCallResult, String> {
        let res = self
            .request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": args}),
            )
            .await?;
        Ok(parse_call_result(&res))
    }

    /// Attaches an OAuth bearer used on subsequent requests (after a 401
    /// retry the header is stored here rather than baked into the client).
    fn set_bearer(&mut self, token: &str) {
        self.extra_headers
            .insert("Authorization".to_string(), format!("Bearer {token}"));
    }
}

/// Pulls the JSON payload whose id matches from an SSE body.
fn extract_sse_data(text: &str, id: u64) -> Result<String, String> {
    for block in text.split("\n\n") {
        for line in block.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                if v.get("id") == Some(&serde_json::json!(id)) {
                    return Ok(data.to_string());
                }
            }
        }
    }
    Err("no matching SSE message".to_string())
}

async fn connect_transport(
    name: &str,
    cwd: &str,
    cfg: &MCPServerConfig,
) -> Result<Transport, String> {
    let mut typ = cfg.typ.trim().to_lowercase();
    if typ == "sse" {
        return Err("sse transport not supported".to_string());
    }
    match typ.as_str() {
        "local" => typ = "stdio".to_string(),
        "remote" => typ = "http".to_string(),
        _ => {}
    }
    if typ.is_empty() {
        typ = if !cfg.url.is_empty() {
            "http".to_string()
        } else {
            "stdio".to_string()
        };
    }
    match typ.as_str() {
        "http" => {
            if cfg.url.is_empty() {
                return Err("missing url".to_string());
            }
            let mut header_map = reqwest::header::HeaderMap::new();
            for (k, v) in expand_env_map(&cfg.headers) {
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .map_err(|e| e.to_string())?;
                let value =
                    reqwest::header::HeaderValue::from_str(&v).map_err(|e| e.to_string())?;
                header_map.insert(name, value);
            }
            // Attach a cached OAuth bearer up front when one is stored;
            // the 401 retry below handles the interactive sign-in.
            if !header_map.contains_key(reqwest::header::AUTHORIZATION) {
                match crate::mcp_oauth::bearer_token(name, &cfg.url, &cfg.client_id, false).await {
                    Ok(Some(token)) => {
                        if let Ok(v) =
                            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                        {
                            header_map.insert(reqwest::header::AUTHORIZATION, v);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            let client = reqwest::Client::builder()
                .timeout(MCP_CALL_TIMEOUT)
                .default_headers(header_map)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Transport::Http(HttpSession {
                client,
                url: cfg.url.clone(),
                session_id: None,
                extra_headers: BTreeMap::new(),
                next_id: 0,
            }))
        }
        "stdio" => {
            if cfg.command.is_empty() {
                return Err("missing command".to_string());
            }
            let cwd = Path::new(cwd);
            if !cwd.is_absolute() {
                return Err("MCP stdio requires an absolute session cwd".to_string());
            }
            use tokio::io::AsyncBufReadExt;
            let mut cmd = tokio::process::Command::new(&cfg.command);
            cmd.args(&cfg.args)
                .current_dir(cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            for (k, v) in expand_env_map(&cfg.env) {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            let stdin = child.stdin.take().expect("piped stdin");
            let stdout = child.stdout.take().expect("piped stdout");
            let sess = StdioSession {
                child,
                stdin,
                stdout: tokio::io::BufReader::new(stdout).lines(),
                next_id: 0,
            };
            Ok(Transport::Stdio(Box::new(sess)))
        }
        other => Err(format!("unsupported type \"{other}\"")),
    }
}

async fn connect_and_list(
    name: &str,
    cwd: &str,
    cfg: &MCPServerConfig,
) -> Result<(Arc<tokio::sync::Mutex<Transport>>, Vec<McpTool>), String> {
    let mut transport =
        match tokio::time::timeout(MCP_CONNECT_TIMEOUT, connect_transport(name, cwd, cfg)).await {
            Ok(r) => r?,
            Err(_) => return Err("connect timed out".to_string()),
        };
    let tools = match initialize_and_list(&mut transport).await {
        Ok(t) => t,
        Err(e) if e.contains("HTTP 401") && transport.is_http() => {
            if cfg
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
                || !cfg.auth.eq_ignore_ascii_case("oauth")
                || cfg!(test)
            {
                return Err(e);
            }
            // Server demands authorization: run the OAuth flow (cached
            // token was already tried at connect). Opens a browser for
            // first-time sign-in; tokens persist and refresh afterwards.
            eprintln!("mcp: server \"{name}\" requires authorization, starting OAuth sign-in…");
            let token = crate::mcp_oauth::bearer_token(name, &cfg.url, &cfg.client_id, true)
                .await
                .map_err(|e| format!("oauth: {e}"))?
                .ok_or_else(|| "server requires authorization".to_string())?;
            if let Transport::Http(s) = &mut transport {
                s.set_bearer(&token);
            }
            initialize_and_list(&mut transport).await?
        }
        Err(e) => return Err(e),
    };
    Ok((Arc::new(tokio::sync::Mutex::new(transport)), tools))
}

async fn initialize_and_list(transport: &mut Transport) -> Result<Vec<McpTool>, String> {
    match tokio::time::timeout(MCP_CONNECT_TIMEOUT, transport.initialize()).await {
        Err(_) => return Err("initialize failed: timed out".to_string()),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(())) => {}
    }
    if let Transport::Stdio(s) = transport {
        let _ = s.notify_initialized().await;
    }
    match tokio::time::timeout(MCP_CONNECT_TIMEOUT, transport.list_tools()).await {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(format!("list tools: {e}")),
        Err(_) => Err("list tools timed out".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Hub.
// ---------------------------------------------------------------------------

struct McpConn {
    name: String,
    cwd: String,
    session: Option<Arc<tokio::sync::Mutex<Transport>>>,
    tools: Vec<ToolDef>,
    mapping: HashMap<String, McpToolRef>,
    /// All of this server's tools are hidden from the model until found
    /// via find_tool (expanded for the session).
    deferred: bool,
}

pub struct McpHub {
    #[allow(clippy::type_complexity)]
    conns: Mutex<HashMap<String, McpConn>>,
    /// Deferred tool names (sanitized) discovered via find_tool or called
    /// directly, per cwd: they stay in the model's tool list for the rest
    /// of the session, mirroring Anthropic's inline tool_reference model.
    expanded: Mutex<HashMap<String, std::collections::HashSet<String>>>,
}

static DEFAULT_HUB: Lazy<McpHub> = Lazy::new(McpHub::default);

pub fn mcp_default_hub() -> &'static McpHub {
    &DEFAULT_HUB
}

impl Default for McpHub {
    fn default() -> Self {
        McpHub {
            conns: Mutex::new(HashMap::new()),
            expanded: Mutex::new(HashMap::new()),
        }
    }
}

impl McpHub {
    fn used_names_locked(&self, cwd: &str) -> HashMap<String, bool> {
        let mut used = HashMap::new();
        if let Ok(conns) = self.conns.lock() {
            for c in conns.values() {
                if c.cwd != cwd {
                    continue;
                }
                for name in c.mapping.keys() {
                    used.insert(name.clone(), true);
                }
            }
        }
        used
    }

    /// True when a live (or failed-marker) connection exists for the key.
    pub(crate) fn is_cached(&self, name: &str, cwd: &str) -> bool {
        self.conns
            .lock()
            .map(|c| c.contains_key(&cache_key(name, cwd)))
            .unwrap_or(false)
    }

    pub(crate) fn is_failed(&self, name: &str, cwd: &str) -> bool {
        self.conns
            .lock()
            .map(|c| {
                c.get(&cache_key(name, cwd))
                    .map(|conn| conn.session.is_none())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    pub(crate) async fn ensure(&self, name: &str, cwd: &str, cfg: &MCPServerConfig) -> bool {
        if self.is_cached(name, cwd) {
            return !self.is_failed(name, cwd);
        }
        match connect_and_list(name, cwd, cfg).await {
            Err(e) => {
                eprintln!("mcp: skip server \"{name}\": {e}");
                if let Ok(mut conns) = self.conns.lock() {
                    let key = cache_key(name, cwd);
                    conns.entry(key).or_insert(McpConn {
                        name: name.to_string(),
                        cwd: cwd.to_string(),
                        session: None,
                        tools: Vec::new(),
                        mapping: HashMap::new(),
                        deferred: false,
                    });
                }
                false
            }
            Ok((session, tools)) => {
                let mut used = self.used_names_locked(cwd);
                let (td, mapping) = convert_mcp_tools(name, &tools, &mut used);
                let deferred = server_deferred(cfg, td.len());
                if let Ok(mut conns) = self.conns.lock() {
                    let key = cache_key(name, cwd);
                    if let Some(existing) = conns.get(&key) {
                        if existing.session.is_some() {
                            return true;
                        }
                    }
                    conns.insert(
                        key,
                        McpConn {
                            name: name.to_string(),
                            cwd: cwd.to_string(),
                            session: Some(session),
                            tools: td,
                            mapping,
                            deferred,
                        },
                    );
                }
                true
            }
        }
    }

    /// Tools the model sees this session: everything from non-deferred
    /// servers, plus deferred tools previously expanded via find_tool or
    /// direct call. Connection setup happens here too, so callers that
    /// only need exposure still warm the hub.
    pub async fn tools_for(&self, cwd: &str) -> Vec<ToolDef> {
        let cfgs = load_mcp_configs(cwd);
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        let expanded: std::collections::HashSet<String> = match self.expanded.lock() {
            Ok(ex) => ex.get(cwd).cloned().unwrap_or_default(),
            Err(_) => Default::default(),
        };
        for (name, cfg) in &cfgs {
            if self.ensure(name, cwd, cfg).await {
                seen.insert(name.clone());
                if let Ok(conns) = self.conns.lock() {
                    if let Some(c) = conns.get(&cache_key(name, cwd)) {
                        for t in &c.tools {
                            if !c.deferred || expanded.contains(&t.function.name) {
                                out.push(t.clone());
                            }
                        }
                    }
                }
            }
        }
        // Still-connected servers whose config vanished this round.
        if let Ok(conns) = self.conns.lock() {
            for c in conns.values() {
                if c.session.is_some()
                    && !c.tools.is_empty()
                    && c.cwd == cwd
                    && !seen.contains(&c.name)
                {
                    for t in &c.tools {
                        if !c.deferred || expanded.contains(&t.function.name) {
                            out.push(t.clone());
                        }
                    }
                }
            }
        }
        out
    }

    /// True when any server for this cwd defers tools, so the model
    /// should be given the find_tool entry point.
    pub async fn has_deferred(&self, cwd: &str) -> bool {
        let cfgs = load_mcp_configs(cwd);
        for (name, cfg) in &cfgs {
            if self.ensure(name, cwd, cfg).await {
                if let Ok(conns) = self.conns.lock() {
                    if let Some(c) = conns.get(&cache_key(name, cwd)) {
                        if c.deferred {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Search deferred tools for this cwd; matches are expanded for the
    /// rest of the session. Returns matched defs plus the total number
    /// of deferred tools available (for no-match hints), 0 when none.
    pub async fn find_deferred(&self, cwd: &str, query: &str) -> (Vec<ToolDef>, usize) {
        self.tools_for(cwd).await; // warm connections for all servers
        let mut scored: Vec<(i64, String, ToolDef)> = Vec::new();
        let mut total = 0usize;
        if let Ok(conns) = self.conns.lock() {
            for c in conns.values() {
                if c.cwd != cwd || !c.deferred {
                    continue;
                }
                total += c.tools.len();
                for t in &c.tools {
                    let s = score_tool(query, t);
                    if s > 0 {
                        scored.push((s, t.function.name.clone(), t.clone()));
                    }
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        scored.truncate(FIND_RESULT_LIMIT);
        if let Ok(mut ex) = self.expanded.lock() {
            let entry = ex.entry(cwd.to_string()).or_default();
            for (_, _, t) in &scored {
                entry.insert(t.function.name.clone());
            }
        }
        (scored.into_iter().map(|(_, _, t)| t).collect(), total)
    }

    /// mark_expanded pins a deferred tool as exposed after a direct call.
    pub fn mark_expanded(&self, cwd: &str, name: &str) {
        if let Ok(mut ex) = self.expanded.lock() {
            ex.entry(cwd.to_string()).or_default().insert(name.into());
        }
    }

    pub(crate) fn lookup(
        &self,
        sanitized: &str,
        cwd: &str,
    ) -> Option<(Arc<tokio::sync::Mutex<Transport>>, McpToolRef)> {
        let conns = self.conns.lock().ok()?;
        for c in conns.values() {
            if c.cwd != cwd {
                continue;
            }
            if let Some(ref_) = c.mapping.get(sanitized) {
                return c.session.clone().map(|s| (s, ref_.clone()));
            }
        }
        None
    }

    pub fn close_all(&self) {
        if let Ok(mut conns) = self.conns.lock() {
            conns.clear();
        }
    }
}

/// closeAllMCP drops every cached connection (stdio children are killed
/// via kill_on_drop).
pub fn close_all_mcp() {
    mcp_default_hub().close_all();
}

pub async fn mcp_tools_for(cwd: &Path) -> Vec<ToolDef> {
    mcp_default_hub()
        .tools_for(&cwd.display().to_string())
        .await
}

/// True when any server for this cwd defers tools behind find_tool.
pub async fn has_deferred_tools(cwd: &Path) -> bool {
    mcp_default_hub()
        .has_deferred(&cwd.display().to_string())
        .await
}

/// Max tools a single find_tool call loads into the session.
const FIND_RESULT_LIMIT: usize = 5;

/// tokenizeQuery lowercases and splits on non-alphanumeric characters,
/// dropping short tokens that add more noise than signal.
fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_string())
        .collect()
}

/// scoreTool ranks one deferred tool against the query. Name matches
/// count most (mcp_meta-ads_get_campaigns carries its own keywords),
/// description matches second, parameter names last. Zero means no
/// signal; caller drops those.
fn score_tool(query: &str, tool: &ToolDef) -> i64 {
    let name_lower = tool.function.name.to_lowercase();
    let name_words: Vec<String> = name_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_string)
        .collect();
    let desc_lower = tool.function.description.to_lowercase();
    let params_lower = tool.function.parameters.to_string().to_lowercase();
    let mut score = 0i64;
    for tok in query_tokens(query) {
        if name_words.contains(&tok) {
            score += 3;
        }
        if desc_lower.contains(&tok) {
            score += 1;
        }
        if params_lower.contains(&tok) {
            score += 1;
        }
    }
    score
}

/// find_tool executes the deferred-tool search for the model: renders
/// matches as full tool definitions and pins them for the session.
pub async fn execute_find_tool(args_json: &str, cwd: &Path) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        query: String,
    }
    let args: Args = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    let cwd_str = cwd.display().to_string();
    let (matches, total) = mcp_default_hub().find_deferred(&cwd_str, &args.query).await;
    if total == 0 {
        return "no deferred tools: all MCP servers expose their tools directly".into();
    }
    if matches.is_empty() {
        return format!(
            "no tools matched \"{}\". {total} deferred tools exist; retry with \
             different keywords (server name or capability, e.g. \"campaigns\", \
             \"github pull request\", \"linear issue\").",
            args.query
        );
    }
    let defs: Vec<serde_json::Value> = matches
        .iter()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null))
        .collect();
    let out = serde_json::json!({
        "matches": defs,
        "note": format!(
            "{} of {total} deferred tools matched. They are now loaded for this \
             session — call the chosen tool directly next.",
            defs.len()
        ),
    });
    out.to_string()
}

/// Used by execute_tool's default arm: does a bare (non-mcp_-prefixed)
/// name resolve to an MCP tool for this cwd?
pub async fn hub_lookup(sanitized: &str, cwd: &Path) -> bool {
    mcp_default_hub()
        .lookup(sanitized, &cwd.display().to_string())
        .is_some()
}

pub async fn execute_mcp_selection(
    server: &str,
    tool: &str,
    arguments: serde_json::Value,
    cwd: &Path,
    override_config: Option<MCPServerConfig>,
) -> String {
    let tool = tool.trim();
    if tool.is_empty() {
        return "error: selected MCP tool is empty".into();
    }
    let cwd_str = cwd.display().to_string();
    let cfg = match override_config {
        Some(config) => config,
        None => match load_mcp_configs(&cwd_str).remove(server) {
            Some(config) => config,
            None => return format!("error: unknown MCP server \"{server}\""),
        },
    };
    match tokio::time::timeout(MCP_CALL_TIMEOUT, async {
        let (session, tools) = connect_and_list(server, &cwd_str, &cfg).await?;
        if !tools.iter().any(|candidate| candidate.name == tool) {
            return Err(format!("server \"{server}\" has no tool \"{tool}\""));
        }
        let mut guard = session.lock().await;
        guard.call_tool(tool, arguments).await
    })
    .await
    {
        Err(_) => "error: tool call timed out".into(),
        Ok(Err(error)) => format!("error: {error}"),
        Ok(Ok(result)) => format_mcp_result(Some(&result)),
    }
}

pub async fn execute_mcp_tool(name: &str, arguments: &str, cwd: &Path) -> String {
    let cwd_str = cwd.display().to_string();
    mcp_default_hub().tools_for(&cwd_str).await;
    let Some((session, ref_)) = mcp_default_hub().lookup(name, &cwd_str) else {
        return format!("error: unknown MCP tool \"{name}\"");
    };
    // Directly calling a deferred tool expands it for this session so it
    // stays in the model's tool list on later rounds.
    mcp_default_hub().mark_expanded(&cwd_str, name);
    let args: serde_json::Value = if arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(arguments) {
            Ok(a) => a,
            Err(e) => return format!("error parsing arguments: {e}"),
        }
    };
    let call = tokio::time::timeout(MCP_CALL_TIMEOUT, async move {
        let mut guard = session.lock().await;
        guard.call_tool(&ref_.name, args).await
    })
    .await;
    match call {
        Err(_) => "error: tool call timed out".to_string(),
        Ok(Err(e)) => format!("error: {e}"),
        Ok(Ok(res)) => format_mcp_result(Some(&res)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_and_uniquify_names() {
        assert_eq!(
            sanitize_mcp_name("my.server", "get/foo"),
            "mcp_my_server_get_foo"
        );
        let mut used = HashMap::new();
        let a = unique_mcp_name(&sanitize_mcp_name("github", "issues"), &mut used);
        let b = unique_mcp_name(&sanitize_mcp_name("github", "issues"), &mut used);
        assert_eq!(a, "mcp_github_issues");
        assert_eq!(b, "mcp_github_issues_2");

        let tools = vec![
            McpTool {
                name: "search".into(),
                description: "s".into(),
                input_schema: None,
            },
            McpTool {
                name: "search".into(),
                description: "s2".into(),
                input_schema: None,
            },
        ];
        let (td, mapping) = convert_mcp_tools("svc", &tools, &mut HashMap::new());
        assert_eq!(td.len(), 2);
        assert_eq!(td[0].function.name, "mcp_svc_search");
        assert_eq!(td[1].function.name, "mcp_svc_search_2");
        assert_eq!(mapping["mcp_svc_search"].name, "search");
        assert_eq!(mapping["mcp_svc_search_2"].name, "search");
        assert!(td[0].function.description.starts_with("[mcp:svc] "));
    }

    #[test]
    fn sanitize_empty_falls_back_to_mcp_tool() {
        // The "mcp_" prefix survives trimming, matching Go byte-for-byte.
        assert_eq!(sanitize_mcp_name("##", "$$"), "mcp");
        assert_eq!(sanitize_mcp_name("", ""), "mcp");
    }

    #[test]
    fn params_json_defaults_to_object() {
        let schema = serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}});
        let got = mcp_params_json(Some(&schema));
        assert_eq!(got["properties"]["q"]["type"], "string");
        assert_eq!(mcp_params_json(None)["type"], "object");
        assert_eq!(
            mcp_params_json(Some(&serde_json::Value::Null))["type"],
            "object"
        );
    }

    #[test]
    fn expands_env_refs() {
        std::env::set_var("ASA_CLIENT_ID_TEST_TOOLS", "cid-1");
        assert_eq!(expand_env_refs("{env:ASA_CLIENT_ID_TEST_TOOLS}"), "cid-1");
        let mut m = BTreeMap::new();
        m.insert(
            "ASA_CLIENT_ID_TEST_TOOLS".to_string(),
            "{env:ASA_CLIENT_ID_TEST_TOOLS}".to_string(),
        );
        m.insert("plain".to_string(), "x".to_string());
        let m = expand_env_map(&m);
        assert_eq!(m["ASA_CLIENT_ID_TEST_TOOLS"], "cid-1");
        assert_eq!(m["plain"], "x");
        // Unclosed token passes through untouched (like Go).
        assert_eq!(expand_env_refs("{env:NOPE_X"), "{env:NOPE_X");
    }

    #[test]
    fn parses_and_merges_configs_with_override_order() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join("xdg");
        let cwd = home.path().join("proj");
        std::fs::create_dir_all(xdg.join(atom_core::build::dir_leaf())).unwrap();
        std::fs::create_dir_all(cwd.join(".cursor")).unwrap();

        std::fs::write(
            xdg.join(atom_core::build::dir_leaf()).join("mcp.json"),
            r#"{"mcpServers":{
                "github":{"command":"npx","args":["-y","@modelcontextprotocol/server-github"]},
                "gone":{"command":"echo","disabled":true},
                "remote":{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer x"}}
            }}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".cursor").join("mcp.json"),
            r#"{"mcpServers":{
                "github":{"command":"other","args":["ok"]},
                "gone":{"command":"still-disabled","disabled":true}
            }}"#,
        )
        .unwrap();

        let cfgs = load_mcp_configs_in(
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(home.path()),
        );
        assert!(
            !cfgs.contains_key("gone"),
            "disabled server should be skipped"
        );
        assert_eq!(cfgs["github"].command, "other", "project overrides user");
        assert_eq!(cfgs["remote"].url, "https://example.com/mcp");
    }

    #[test]
    fn environment_aliases_env_when_env_missing() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join("xdg");
        std::fs::create_dir_all(xdg.join(atom_core::build::dir_leaf())).unwrap();
        std::fs::write(
            xdg.join(atom_core::build::dir_leaf()).join("mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"run","environment":{"KEY":"v"}}}}"#,
        )
        .unwrap();
        let cfgs = load_mcp_configs_in(
            "/",
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(home.path()),
        );
        assert_eq!(cfgs["srv"].env["KEY"], "v");
    }

    #[test]
    fn unsupported_types_error_like_go() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            async fn expect_err(cfg: &MCPServerConfig, want: &str) {
                match connect_transport("x", "/", cfg).await {
                    Err(e) => assert_eq!(e, want),
                    Ok(_) => panic!("expected error {want}"),
                }
            }
            expect_err(
                &MCPServerConfig {
                    typ: "sse".into(),
                    ..Default::default()
                },
                "sse transport not supported",
            )
            .await;
            expect_err(
                &MCPServerConfig {
                    typ: "websocket".into(),
                    ..Default::default()
                },
                "unsupported type \"websocket\"",
            )
            .await;
            expect_err(
                &MCPServerConfig {
                    typ: "http".into(),
                    ..Default::default()
                },
                "missing url",
            )
            .await;
            expect_err(
                &MCPServerConfig {
                    typ: "stdio".into(),
                    ..Default::default()
                },
                "missing command",
            )
            .await;
        });
    }

    #[test]
    fn formats_results_like_go() {
        let boom = parse_call_result(&serde_json::json!({
            "content":[{"type":"text","text":"boom"}], "isError": true}));
        assert_eq!(format_mcp_result(Some(&boom)), "error: boom");
        let img = parse_call_result(&serde_json::json!({
            "content":[{"type":"image","data":"","mimeType":"image/png"}]}));
        assert_eq!(format_mcp_result(Some(&img)), "[image]");
        let silent_fail = parse_call_result(&serde_json::json!({"isError":true}));
        assert_eq!(format_mcp_result(Some(&silent_fail)), "error: tool failed");
        assert_eq!(format_mcp_result(None), "");
    }

    #[test]
    fn failed_connect_is_cached_without_leaking_tools() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let hub = McpHub::default();
            let cwd = tempfile::tempdir().unwrap().path().display().to_string();
            let cfg = MCPServerConfig {
                command: "atom-mcp-server-does-not-exist".into(),
                ..Default::default()
            };
            assert!(!hub.ensure("missing", &cwd, &cfg).await);
            assert!(hub.is_cached("missing", &cwd));
            assert!(hub.is_failed("missing", &cwd));
            assert!(hub.tools_for(&cwd).await.is_empty());
            // Second attempt short-circuits through the failed marker.
            assert!(!hub.ensure("missing", &cwd, &cfg).await);
        });
    }

    #[tokio::test]
    async fn execute_selection_validates_server_and_tool() {
        let cwd = tempfile::tempdir().unwrap();
        let unknown = execute_mcp_selection(
            "missing",
            "web_search",
            serde_json::json!({"query":"atom"}),
            cwd.path(),
            None,
        )
        .await;
        assert_eq!(unknown, "error: unknown MCP server \"missing\"");

        let empty =
            execute_mcp_selection("missing", " ", serde_json::json!({}), cwd.path(), None).await;
        assert_eq!(empty, "error: selected MCP tool is empty");
    }

    #[tokio::test]
    async fn stdio_roundtrip_against_fake_server() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-mcp.sh");
        std::fs::write(
            &script,
            "#!/bin/sh
while IFS= read -r line; do
  case \"$line\" in
    *initialize*)
      printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"}}}'
      ;;
    *tools/list*)
      printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"echo text\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}}}}]}}'
      ;;
    *tools/call*)
      printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"echo:hi\"}],\"isError\":false}}'
      ;;
  esac
done
",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Use the process default hub so execute_mcp_tool and
        // tool_definitions_with_mcp see the same connection; the unique
        // tempdir cwd isolates this test from others.
        let hub = mcp_default_hub();
        let cwd = dir.path().display().to_string();
        let cfg = MCPServerConfig {
            command: script.display().to_string(),
            ..Default::default()
        };
        assert!(hub.ensure("echo", &cwd, &cfg).await);

        let tools = hub.tools_for(&cwd).await;
        let echo_def = tools.iter().find(|t| t.function.name == "mcp_echo_echo");
        assert!(
            echo_def.is_some(),
            "tools = {:?}",
            tools.iter().map(|t| &t.function.name).collect::<Vec<_>>()
        );
        assert!(echo_def
            .unwrap()
            .function
            .description
            .contains("[mcp:echo]"));
        assert_eq!(echo_def.unwrap().function.parameters["type"], "object");

        // Builtins remain present alongside MCP tools.
        let all = crate::tool_definitions_with_mcp(Path::new(&cwd)).await;
        assert!(all.iter().any(|t| t.function.name == "skill"));
        assert!(all.iter().any(|t| t.function.name == "mcp_echo_echo"));

        let out = execute_mcp_tool("mcp_echo_echo", "{\"text\":\"hi\"}", Path::new(&cwd)).await;
        assert_eq!(out, "echo:hi");

        let direct = execute_mcp_selection(
            "echo",
            "echo",
            serde_json::json!({"text":"hi"}),
            Path::new(&cwd),
            Some(cfg.clone()),
        )
        .await;
        assert_eq!(direct, "echo:hi");

        let missing_tool = execute_mcp_selection(
            "echo",
            "missing",
            serde_json::json!({}),
            Path::new(&cwd),
            Some(cfg),
        )
        .await;
        assert_eq!(
            missing_tool,
            "error: server \"echo\" has no tool \"missing\""
        );

        let unknown = execute_mcp_tool("mcp_nope_nope", "{}", Path::new(&cwd)).await;
        assert!(unknown.starts_with("error:"), "{unknown}");

        hub.close_all();
    }

    #[test]
    fn defer_heuristic_and_overrides() {
        let cfg = MCPServerConfig::default();
        assert!(!server_deferred(&cfg, 5));
        assert!(server_deferred(&cfg, AUTO_DEFER_TOOLS + 1));
        // Explicit config beats the count heuristic.
        let on = MCPServerConfig {
            defer: Some(true),
            ..Default::default()
        };
        assert!(server_deferred(&on, 1));
        let off = MCPServerConfig {
            defer: Some(false),
            ..Default::default()
        };
        assert!(!server_deferred(&off, AUTO_DEFER_TOOLS + 50));
    }

    #[tokio::test]
    async fn deferred_server_hides_tools_until_find_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = Vec::new();
        for i in 1..=21 {
            tools.push(serde_json::json!({
                "name": format!("alpha{i}"),
                "description": format!("capability number {i}"),
            }));
        }
        let init_line = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"big","version":"1"}}}"#;
        let list_line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"tools": tools},
        })
        .to_string();
        let script = dir.path().join("big-mcp.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *initialize*)
      printf '%s\n' '{init_line}'
      ;;
    *tools/list*)
      printf '%s\n' '{list_line}'
      ;;
  esac
done
"#
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let hub = mcp_default_hub();
        let cwd = dir.path().display().to_string();
        // Register through a project config so has_deferred/find_deferred
        // (which scan load_mcp_configs) see the server like production.
        let cfg_dir = dir.path().join(".atom");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "bigserver": {"command": script.display().to_string()}
                }
            })
            .to_string(),
        )
        .unwrap();

        // 21 tools with no defer override: auto-defers above AUTO_DEFER_TOOLS.
        let visible = hub.tools_for(&cwd).await;
        assert!(
            visible
                .iter()
                .all(|t| !t.function.name.starts_with("mcp_bigserver_")),
            "visible = {:?}",
            visible.iter().map(|t| &t.function.name).collect::<Vec<_>>()
        );
        assert!(hub.has_deferred(&cwd).await);

        // find_tool surfaces the match and pins it for the session.
        let (matches, total) = hub.find_deferred(&cwd, "alpha7").await;
        assert_eq!(total, 21);
        assert_eq!(
            matches
                .iter()
                .map(|t| t.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["mcp_bigserver_alpha7"]
        );
        let after = hub.tools_for(&cwd).await;
        assert!(after
            .iter()
            .any(|t| t.function.name == "mcp_bigserver_alpha7"));
        // The other 20 stay hidden.
        assert!(after
            .iter()
            .all(|t| t.function.name == "mcp_bigserver_alpha7"
                || !t.function.name.starts_with("mcp_bigserver_")));

        hub.close_all();
    }
}
