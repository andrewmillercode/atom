//! The ACP turn driver: an alternative to run_session_turn for sessions
//! backed by an external ACP agent (provider "acp-agent"). The agent
//! owns the model loop and its tools; atom renders its updates, serves
//! its fs/permission requests through the existing approval flow, and
//! persists the resulting transcript. Everything is emitted as the same
//! NDJSON event vocabulary the model turn loop produces, so the TUI and
//! HTTP clients need no new event handling.

use crate::cancel::CancelToken;
use crate::state::AppState;
use crate::turn::TurnCtx;
use crate::turn::{done_event, emit, end_of_turn, last_tokens_per_sec, persist_session, EventOut};
use atom_core::session::store::Session;
use atom_core::types::Message;
use atom_sandbox::approvals::{ApprovalRequest, Approver, Decision};
use atom_tools::acp::{
    config_option_for_category, get_connection, parse_config_options, parse_permission_request,
    parse_update, probe_session_config, session_config_from_result, AcpClientHandler, AcpError,
    AgentProcess, PermissionRequest, Update,
};
use chrono::Utc;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// How many chained prompts (user interjections that arrived mid-turn)
/// one turn may run before stopping.
const MAX_PROMPT_CHAIN: usize = 50;

/// How long an agent may take to acknowledge session/cancel before the
/// turn stops waiting for its prompt response.
const CANCEL_TAIL: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything the agent → client callback handler needs to route back
/// to the atom session that started the prompt.
struct SessionLink {
    session_id: String,
    cwd: String,
    out: EventOut,
    cancel: CancelToken,
}

static LINKS: Lazy<Mutex<HashMap<String, Arc<SessionLink>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// The AcpClientHandler the connection is spawned with.
// ---------------------------------------------------------------------------

pub struct ServerAcpHandler {
    state: Arc<AppState>,
}

impl ServerAcpHandler {
    pub fn new(state: Arc<AppState>) -> Self {
        ServerAcpHandler { state }
    }
}

#[async_trait::async_trait]
impl AcpClientHandler for ServerAcpHandler {
    async fn on_request(&self, method: &str, params: &Value) -> Result<Value, AcpError> {
        match method {
            "fs/read_text_file" => {
                let path = jstr(params, "path");
                if !PathBuf::from(&path).is_absolute() {
                    return Err(AcpError::new(-32602, "path must be absolute"));
                }
                std::fs::read_to_string(&path)
                    .map(|content| json!({"content": content}))
                    .map_err(|e| AcpError::new(-32603, format!("read failed: {e}")))
            }
            "fs/write_text_file" => {
                let path = jstr(params, "path");
                if !PathBuf::from(&path).is_absolute() {
                    return Err(AcpError::new(-32602, "path must be absolute"));
                }
                let content = params.get("content").and_then(Value::as_str).unwrap_or("");
                std::fs::write(&path, content)
                    .map(|_| json!({}))
                    .map_err(|e| AcpError::new(-32603, format!("write failed: {e}")))
            }
            "session/request_permission" => {
                let req = parse_permission_request(params);
                let link = LINKS.lock().unwrap().get(&req.session_id).cloned();
                let Some(link) = link else {
                    // Nobody is driving this session right now — refuse
                    // rather than block the agent forever.
                    return Ok(permission_outcome(&req, Decision::DenyOnce));
                };
                let approver = crate::dispatch::ServerApprover::for_turn(
                    self.state.clone(),
                    link.session_id.clone(),
                    link.out.clone(),
                    Some(link.cancel.clone()),
                );
                let raw = req
                    .raw_input
                    .as_ref()
                    .map(|input| serde_json::to_string(input).unwrap_or_default())
                    .unwrap_or_default();
                let command = if raw.is_empty() {
                    req.title.clone()
                } else {
                    format!("{} ({})", req.title, raw)
                };
                let decision = approver
                    .decide(ApprovalRequest {
                        session_id: link.session_id.clone(),
                        command,
                        cwd: PathBuf::from(&link.cwd),
                        workspace_root: PathBuf::new(),
                        rule_id: String::new(),
                        reason: String::new(),
                        accept_all_preview: None,
                        flagged: false,
                    })
                    .await;
                Ok(permission_outcome(&req, decision))
            }
            _ => Err(AcpError::new(
                atom_tools::acp::ERR_METHOD_NOT_FOUND,
                format!("method {method} is not supported"),
            )),
        }
    }
}

fn jstr(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// acp_output_tokens reads the optional usage block the ACP spec puts
/// on the prompt response; agents that omit it (Kimi, Cursor, often
/// Claude Code) yield zero and the turn shows no tok/s.
fn acp_output_tokens(result: &Value) -> u64 {
    let usage = result.get("usage").unwrap_or(&Value::Null);
    for key in ["outputTokens", "output_tokens", "totalOutputTokens"] {
        if let Some(n) = usage.get(key).and_then(Value::as_u64) {
            return n;
        }
    }
    0
}

/// acp_response_usage reads the prompt response's per-turn token block.
fn acp_response_usage(result: &Value) -> Option<atom_core::types::StreamUsage> {
    let usage = result.get("usage").filter(|u| u.is_object())?;
    let num = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(Value::as_i64))
            .unwrap_or(0)
    };
    let cache_read = num(&["cachedReadTokens", "cached_read_tokens"]);
    let cache_write = num(&["cachedWriteTokens", "cached_write_tokens"]);
    let input = num(&["inputTokens", "input_tokens"]) + cache_read + cache_write;
    let output = num(&["outputTokens", "output_tokens", "totalOutputTokens"]);
    if input == 0 && output == 0 {
        return None;
    }
    Some(atom_core::types::StreamUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        reasoning_tokens: num(&["thoughtTokens", "thought_tokens"]),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ..Default::default()
    })
}

/// fold_turn_usage adds one prompt's tokens onto the session totals,
/// keeping the live context meter (`total_tokens`/`context_window`).
fn fold_turn_usage(sess: &mut Session, turn: &atom_core::types::StreamUsage) {
    let mut u = sess.usage.clone().unwrap_or_default();
    u.prompt_tokens += turn.prompt_tokens;
    u.completion_tokens += turn.completion_tokens;
    u.cache_read_tokens += turn.cache_read_tokens;
    u.cache_write_tokens += turn.cache_write_tokens;
    u.reasoning_tokens += turn.reasoning_tokens;
    if u.context_window == 0 {
        u.total_tokens = turn.prompt_tokens + turn.completion_tokens;
    }
    u.prompt_tokens_all = u.prompt_tokens;
    sess.usage = Some(u);
}

/// Map the user's three-decision set onto the agent's offered options:
/// the spec only recognises {"outcome":"selected","optionId":...} (or a
/// bare "cancelled"); anything else reads as a denial to the agent.
fn permission_outcome(req: &PermissionRequest, decision: Decision) -> Value {
    let kind = match decision {
        Decision::AllowOnce => "allow_once",
        Decision::AllowSession => "allow_always",
        Decision::DenyOnce => "reject_once",
    };
    match req.options.iter().find(|o| o.kind == kind) {
        Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o.id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

// ---------------------------------------------------------------------------
// Update → NDJSON event mapping (pure, unit-testable).
// ---------------------------------------------------------------------------

/// Streaming state across one prompt: what has been emitted so far, for
/// the reasoning_end hand-off and per-tool-call completion tracking.
#[derive(Default)]
struct UpdateTracker {
    saw_reasoning: bool,
    saw_content: bool,
    /// Tool calls already seen a start event for (toolCallId).
    started: HashSet<String>,
    /// Tool calls that already emitted their result (toolCallId).
    completed: HashSet<String>,
    /// Latest kind/title/rawInput per call, held until the block can open.
    pending: HashMap<String, PendingTool>,
}

fn content_event(tracker: &mut UpdateTracker, text: &str) -> Vec<Value> {
    let mut evs = Vec::new();
    if tracker.saw_reasoning && !tracker.saw_content {
        evs.push(json!({"type": "reasoning_end"}));
    }
    tracker.saw_content = true;
    evs.push(json!({"type": "content", "text": text}));
    evs
}

fn reasoning_event(tracker: &mut UpdateTracker, text: &str) -> Vec<Value> {
    tracker.saw_reasoning = true;
    vec![json!({"type": "reasoning", "text": text})]
}

#[derive(Default)]
struct PendingTool {
    kind: String,
    title: String,
    raw_input: Value,
}

fn has_input(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => !m.is_empty(),
        _ => true,
    }
}

/// atom_tool_call renames an agent tool onto atom's own tool vocabulary
/// (bash/read_file/edit_file/…) so the TUI's per-tool rendering, syntax
/// highlighting, and diff blocks apply unchanged.
fn atom_tool_call(
    kind: &str,
    title: &str,
    raw: &Value,
    contents: &[atom_tools::acp::ToolCallContent],
) -> (String, String) {
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| raw.get(*k).and_then(Value::as_str))
            .filter(|s| !s.is_empty())
    };
    let diff_path = contents
        .iter()
        .find(|c| c.typ == "diff")
        .map(|c| c.path.as_str());
    let path = text(&["file_path", "path"]).or(diff_path);
    let generic = || {
        let name = if title.is_empty() { kind } else { title };
        let args = if has_input(raw) { raw.to_string() } else { String::new() };
        (name.to_string(), args)
    };
    match kind {
        "execute" => match text(&["command", "cmd"]) {
            Some(cmd) => ("bash".into(), json!({"command": cmd}).to_string()),
            None => generic(),
        },
        "read" => match path {
            Some(path) => {
                let mut args = json!({"path": path});
                for key in ["offset", "limit"] {
                    if let Some(n) = raw.get(key).and_then(Value::as_i64) {
                        args[key] = json!(n);
                    }
                }
                ("read_file".into(), args.to_string())
            }
            None => generic(),
        },
        "edit" => match path {
            Some(path) => {
                let write = raw.get("content").is_some() && raw.get("old_string").is_none();
                let name = if write { "write_file" } else { "edit_file" };
                (name.into(), json!({"path": path}).to_string())
            }
            None => generic(),
        },
        "search" => match text(&["pattern", "query"]) {
            Some(pattern) => {
                let lower = title.to_lowercase();
                let name = if lower.contains("glob") || lower.starts_with("find") {
                    "glob"
                } else {
                    "grep"
                };
                let mut args = json!({"pattern": pattern});
                if let Some(p) = text(&["path"]) {
                    args["path"] = json!(p);
                }
                (name.into(), args.to_string())
            }
            None => generic(),
        },
        "fetch" => match (text(&["url"]), text(&["query"])) {
            (Some(url), _) => ("webfetch".into(), json!({"url": url}).to_string()),
            (None, Some(q)) => ("web_search".into(), json!({"query": q}).to_string()),
            _ => generic(),
        },
        _ if title.contains("subagent") && raw.get("action").is_some() => {
            ("subagent".into(), raw.to_string())
        }
        _ => generic(),
    }
}

/// tool_events maps a tool_call / tool_call_update onto TUI events. The
/// block opens once the call has input (Claude Code announces calls
/// before their input streams in), so its header shows the real command.
#[allow(clippy::too_many_arguments)]
fn tool_events(
    tracker: &mut UpdateTracker,
    id: &str,
    title: &str,
    kind: &str,
    status: &str,
    contents: &[atom_tools::acp::ToolCallContent],
    raw_input: Option<&Value>,
) -> Vec<Value> {
    let mut evs = Vec::new();
    let done = status == "completed" || status == "failed";
    if !tracker.started.contains(id) {
        let p = tracker.pending.entry(id.to_string()).or_default();
        if !kind.is_empty() {
            p.kind = kind.to_string();
        }
        if !title.is_empty() {
            p.title = title.to_string();
        }
        if let Some(raw) = raw_input.filter(|v| has_input(v)) {
            p.raw_input = raw.clone();
        }
        if has_input(&p.raw_input) || done || !contents.is_empty() {
            let p = tracker.pending.remove(id).unwrap_or_default();
            let (name, arguments) = atom_tool_call(&p.kind, &p.title, &p.raw_input, contents);
            evs.push(json!({
                "type": "tool",
                "name": name,
                "arguments": arguments,
                "call_id": id,
            }));
            tracker.started.insert(id.to_string());
        }
    }
    if done && !tracker.completed.contains(id) {
        tracker.completed.insert(id.to_string());
        let mut text = String::new();
        let mut diffs = Vec::new();
        for c in contents {
            match c.typ.as_str() {
                "diff" => {
                    diffs.push(diff_text(c));
                    if text.is_empty() {
                        text = format!("{}: edited", c.path);
                    }
                }
                _ => {
                    if let Some(block) = &c.content {
                        let described = block.describe();
                        if !described.is_empty() {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&described);
                        }
                    }
                }
            }
        }
        if status == "failed" && text.is_empty() {
            text = "failed".into();
        }
        evs.push(json!({
            "type": "tool_result",
            "text": text,
            "tool_provider": "",
            "call_id": id,
        }));
        for diff in diffs {
            evs.push(json!({"type": "tool_diff", "diff": diff, "call_id": id}));
        }
    }
    evs
}

/// Unified-style diff text from an ACP diff content block: old lines
/// prefixed `-`, new lines `+`, matching what the TUI's diff block
/// renders for edit tools.
fn diff_text(c: &atom_tools::acp::ToolCallContent) -> String {
    let mut out = String::new();
    if !c.path.is_empty() {
        out.push_str(&format!("--- a/{}\n+++ b/{}\n", c.path, c.path));
    }
    for line in c.old_text.as_deref().unwrap_or_default().lines() {
        out.push_str(&format!("-{line}\n"));
    }
    for line in c.new_text.as_deref().unwrap_or_default().lines() {
        out.push_str(&format!("+{line}\n"));
    }
    out.trim_end_matches('\n').to_string()
}

fn plan_text(entries: &[atom_tools::acp::PlanEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let marker = match e.status.as_str() {
            "completed" => "x",
            "in_progress" => ">",
            _ => " ",
        };
        let priority = if e.priority.is_empty() {
            String::new()
        } else {
            format!(" [{}]", e.priority)
        };
        out.push_str(&format!("[{}] {}{}\n", marker, e.content, priority));
    }
    out.trim_end_matches('\n').to_string()
}

/// update_events maps one parsed ACP update into zero or more NDJSON
/// events. `call_id`-carrying events (tool blocks) all use the agent's
/// toolCallId so repeated updates attach to the same block.
fn update_events(tracker: &mut UpdateTracker, update: &Update) -> Vec<Value> {
    match update {
        Update::AgentMessageChunk { content } => content
            .iter()
            .filter(|b| b.typ == "text" && !b.text.is_empty())
            .flat_map(|b| content_event(tracker, &b.text))
            .collect(),
        Update::AgentThoughtChunk { content } => content
            .iter()
            .filter(|b| b.typ == "text" && !b.text.is_empty())
            .flat_map(|b| reasoning_event(tracker, &b.text))
            .collect(),
        Update::UserMessageChunk { .. } => Vec::new(),
        Update::ToolCall {
            id,
            title,
            kind,
            status,
            contents,
            raw_input,
        } => tool_events(tracker, id, title, kind, status, contents, raw_input.as_ref()),
        Update::ToolCallUpdate {
            id,
            title,
            kind,
            status,
            contents,
            raw_input,
            ..
        } => tool_events(tracker, id, title, kind, status, contents, raw_input.as_ref()),
        Update::Plan { entries } => {
            let text = plan_text(entries);
            if text.is_empty() {
                return Vec::new();
            }
            vec![
                json!({"type": "tool", "name": "Plan", "arguments": "", "call_id": "plan"}),
                json!({"type": "tool_result", "text": text, "tool_provider": "", "call_id": "plan"}),
            ]
        }
        // Available commands and mode changes surface through the
        // agent's own UI affordances; atom ignores them for now
        // (config_option_update is handled by the turn loop directly).
        Update::AvailableCommands { .. }
        | Update::UsageUpdate { .. }
        | Update::CurrentMode { .. }
        | Update::ConfigOptionUpdate { .. } => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The turn.
// ---------------------------------------------------------------------------

/// run_acp_turn drives one prompt turn against an external ACP agent.
/// Assumes the user message was already appended and persisted by the
/// caller (run_session_turn's shared preamble).
pub async fn run_acp_turn(
    state: &Arc<AppState>,
    sess: &mut Session,
    id: &str,
    out: EventOut,
    ctx: &TurnCtx,
    started_at: Instant,
) {
    // Model-picker rows are `agent/value`; the bare agent name is
    // accepted too. Resolve to a configured agent before anything else.
    let configs = atom_tools::acp::load_acp_configs(&sess.cwd);
    let agent = match resolve_agent(&sess.model, &configs) {
        Some(agent) => agent,
        None => {
            return finish_with_error(
                state,
                sess,
                id,
                &out,
                ctx,
                started_at,
                format!(
                    "unknown ACP agent \"{}\"; configure it in ~/.config/atom/acp.json \
                     ({{\"acpAgents\": {{\"…\": {{\"command\": …}}}}}})",
                    sess.model
                ),
            )
            .await;
        }
    };
    let cfg = configs.get(&agent).cloned().unwrap_or_default();

    let conn = match get_connection(
        &agent,
        &cfg,
        PathBuf::from(&sess.cwd).as_path(),
        Some(Arc::new(ServerAcpHandler::new(state.clone()))),
    )
    .await
    {
        Ok(agent_process) => agent_process,
        Err(err) => {
            return finish_with_error(state, sess, id, &out, ctx, started_at, err).await;
        }
    };

    // Session setup: resume the agent-side session when one was saved
    // and the agent accepts it, else create a fresh one. session/load
    // errors fall through to session/new either way. Both responses —
    // and the config_option_update notifications that race them — may
    // carry the agent's session config options, models, and modes.
    let session_params = acp_session_params(sess, id);
    let load_probe = if !sess.acp_session_id.is_empty() {
        let mut params = session_params.clone();
        params["sessionId"] = json!(sess.acp_session_id);
        atom_tools::acp::call_collecting_options(&conn.conn, "session/load", params)
            .await
            .ok()
            .map(|(result, options)| {
                // session/load doesn't echo the id — the loaded session is
                // the one we asked for.
                session_config_from_result(sess.acp_session_id.clone(), result, Vec::new(), options)
            })
    } else {
        None
    };
    let probe = match load_probe {
        Some(probe) => probe,
        None => match probe_session_config(&conn.conn, session_params).await {
            Ok(probe) => probe,
            Err(err) => {
                return finish_with_error(
                    state,
                    sess,
                    id,
                    &out,
                    ctx,
                    started_at,
                    auth_aware_error(&conn, format!("acp agent {agent}: {err}")),
                )
                .await;
            }
        },
    };
    let acp_session_id = probe.session_id.clone();
    sess.acp_session_id = acp_session_id.clone();
    sess.acp_config_options = serde_json::to_value(&probe.config_options).unwrap_or(Value::Null);
    sess.acp_models = probe
        .models
        .as_ref()
        .map(serde_json::to_value)
        .unwrap_or(Ok(Value::Null))
        .unwrap_or(Value::Null);
    sess.acp_modes = if probe.modes.available.is_empty() {
        Value::Null
    } else {
        json!({
            "currentModeId": probe.modes.current_mode_id,
            "availableModes": probe
                .modes
                .available
                .iter()
                .map(|(mid, name)| json!({"id": mid, "name": if name.is_empty() { mid } else { name }}))
                .collect::<Vec<_>>(),
        })
    };

    // Route this turn's agent → client callbacks back to this session.
    LINKS.lock().unwrap().insert(
        acp_session_id.clone(),
        Arc::new(SessionLink {
            session_id: id.to_string(),
            cwd: sess.cwd.clone(),
            out: out.clone(),
            cancel: ctx.handle.cancel_token(),
        }),
    );

    // Apply the session's saved config selections (model, thought
    // level, …) before the first prompt; each response refreshes the
    // stored option state.
    match apply_acp_selections(&conn.conn, &acp_session_id, sess).await {
        Ok(warnings) => {
            for message in warnings {
                emit(
                    state,
                    &out,
                    id,
                    &json!({"type": "error", "message": message}),
                )
                .await;
            }
        }
        Err(err) => {
            return finish_with_error(
                state,
                sess,
                id,
                &out,
                ctx,
                started_at,
                format!("acp agent {agent}: {err}"),
            )
            .await;
        }
    }

    // Prompt chain: one session/prompt per user message, plus any that
    // were injected mid-turn.
    let mut prompts = 0;
    loop {
        prompts += 1;
        match prompt_once(state, sess, id, &out, ctx, &conn.conn, &acp_session_id).await {
            PromptOutcome::Done => {}
            PromptOutcome::Cancelled => break,
            PromptOutcome::Error(err) => {
                let ev = json!({"type": "error", "message": err});
                emit(state, &out, id, &ev).await;
                break;
            }
        }
        if prompts >= MAX_PROMPT_CHAIN {
            break;
        }
        let injected = ctx.handle.take_pending();
        if injected.is_empty() {
            break;
        }
        for m in injected {
            state
                .subs
                .broadcast(id, &json!({"type": "user_message", "text": m.content}));
            sess.messages.push(m);
        }
        persist_session(state, sess, id).await;
    }

    LINKS.lock().unwrap().remove(&acp_session_id);
    persist_session(state, sess, id).await;
    let done = done_event(turn_ms(started_at), &agent, last_tokens_per_sec(sess));
    emit(state, &out, id, &done).await;
    end_of_turn(state, sess, id, &ctx.handle, &sess.parent_id).await;
}

/// Name the agent sees atom's MCP server under; Claude Code exposes its
/// tools as `mcp__atom__<tool>`.
pub const ATOM_MCP_SERVER: &str = "atom";

const SUBAGENT_PROMPT_HINT: &str = "You are running inside atom. For parallel or long-running \
delegated work, prefer the `subagent` tool from the `atom` MCP server over your built-in \
Task/Agent tool: atom subagents run on the user's configured non-ACP model, appear in atom's \
subagent panel, and wake you with a follow-up turn when they all finish.";

/// acp_session_params builds the session/new (and session/load) payload.
/// Top-level sessions get atom's MCP bridge so the agent can dispatch
/// atom-native subagents; subagents cannot nest, so children get none.
fn acp_session_params(sess: &Session, id: &str) -> Value {
    if !sess.parent_id.is_empty() {
        return json!({"cwd": sess.cwd, "mcpServers": []});
    }
    let Ok(exe) = std::env::current_exe() else {
        return json!({"cwd": sess.cwd, "mcpServers": []});
    };
    json!({
        "cwd": sess.cwd,
        "mcpServers": [{
            "name": ATOM_MCP_SERVER,
            "command": exe.display().to_string(),
            "args": ["-mcp-bridge"],
            "env": [
                {"name": crate::mcp_bridge::SESSION_ENV, "value": id},
                {
                    "name": crate::mcp_bridge::SOCKET_ENV,
                    "value": atom_core::session::store::socket_path().display().to_string(),
                },
            ],
        }],
        "_meta": {"systemPrompt": {"append": SUBAGENT_PROMPT_HINT}},
    })
}

/// turn_cancel_for returns the cancel token of the live ACP turn for an
/// atom session, so subagent waits issued by the agent stop on Esc.
pub fn turn_cancel_for(session_id: &str) -> Option<CancelToken> {
    LINKS
        .lock()
        .unwrap()
        .values()
        .find(|link| link.session_id == session_id)
        .map(|link| link.cancel.clone())
}

/// resolve_agent maps a session's model field onto a configured agent:
/// an exact config key, or the `agent/value` form the model picker
/// writes (config names never contain '/').
fn resolve_agent(
    model: &str,
    configs: &BTreeMap<String, atom_tools::acp::AcpAgentConfig>,
) -> Option<String> {
    if configs.contains_key(model) {
        return Some(model.to_string());
    }
    let (agent, _) = model.split_once('/')?;
    configs.contains_key(agent).then(|| agent.to_string())
}

/// auth_aware_error decorates session setup failures: agents report
/// auth_required (code -32000 or an "auth" message) before any session
/// exists, which reads as a mystery failure unless the fix — the
/// agent's own login flow — is spelled out.
fn auth_aware_error(process: &AgentProcess, message: String) -> String {
    let authish = message.to_lowercase().contains("auth");
    if !authish {
        return message;
    }
    let method = process
        .capabilities
        .auth_methods
        .first()
        .map(|m| {
            let name = m.get("name").and_then(Value::as_str).unwrap_or("log in");
            let description = m.get("description").and_then(Value::as_str).unwrap_or("");
            if description.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({description})")
            }
        })
        .unwrap_or_else(|| "log in with the agent's own CLI".into());
    format!("{message} — authenticate first: {method}")
}

/// apply_acp_selections pushes the session's stored ACP config
/// selections to the agent before the first prompt. Mechanism per
/// selection: an id among the agent's configOptions rides
/// session/set_config_option; "model" and "mode" fall back to the
/// spec's session/set_model and session/set_mode when the agent only
/// advertises those objects (Claude Code's adapter). A rejected value
/// fails the turn — account-gated models (Devin returns "Invalid
/// value") must not silently fall back to the agent's default.
async fn apply_acp_selections(
    conn: &Arc<atom_tools::acp::AcpConnection>,
    acp_session_id: &str,
    sess: &mut Session,
) -> Result<Vec<String>, String> {
    let mut wanted: Vec<(String, Value)> = sess
        .acp_selected
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let mut warnings = Vec::new();
    let mut thinking_pending = !sess.thinking.is_empty();
    let mut idx = 0;
    while idx <= wanted.len() {
        if idx == wanted.len() {
            // Effort ladders depend on the model, so validate the level
            // against the options the model selection just returned.
            if !std::mem::take(&mut thinking_pending) {
                break;
            }
            let options = parse_config_options(&sess.acp_config_options);
            match config_option_for_category(&options, "thought_level") {
                Some(option) if option.options.iter().any(|v| v.value == sess.thinking) => {
                    wanted.push((option.id.clone(), json!(sess.thinking)));
                }
                Some(option) => warnings.push(format!(
                    "{} \"{}\" is not offered for this model; using the agent's default",
                    option.name, sess.thinking
                )),
                None => {}
            }
            if idx == wanted.len() {
                break;
            }
        }
        let (config_id, value) = wanted[idx].clone();
        idx += 1;
        let raw_options = parse_config_options(&sess.acp_config_options);
        let response = if raw_options.iter().any(|o| o.id == config_id) {
            conn.call(
                "session/set_config_option",
                json!({
                    "sessionId": acp_session_id,
                    "configId": config_id,
                    "value": value,
                }),
            )
            .await
        } else if config_id == "model" && sess.acp_models.is_object() {
            conn.call(
                "session/set_model",
                json!({"sessionId": acp_session_id, "modelId": value}),
            )
            .await
        } else if config_id == "mode" && sess.acp_modes.is_object() {
            conn.call(
                "session/set_mode",
                json!({"sessionId": acp_session_id, "modeId": value}),
            )
            .await
        } else {
            // Stale selection for a selector this agent no longer
            // offers; skip rather than fail the turn.
            continue;
        };
        match response {
            Ok(result) => {
                if let Some(options) = result.get("configOptions") {
                    let merged = atom_tools::acp::merge_config_options(
                        raw_options,
                        parse_config_options(options),
                    );
                    sess.acp_config_options = serde_json::to_value(merged).unwrap_or(Value::Null);
                }
            }
            Err(err) => return Err(format!("setting {config_id}={value} failed: {err}")),
        }
    }
    Ok(warnings)
}

/// finish_with_error ends the turn after a setup failure: error event,
/// persist, done, and the turn-table bookkeeping so the session is not
/// wedged in an "active turn" state.
async fn finish_with_error(
    state: &Arc<AppState>,
    sess: &mut Session,
    id: &str,
    out: &EventOut,
    ctx: &TurnCtx,
    started_at: Instant,
    message: String,
) {
    let agent = sess.model.clone();
    let ev = json!({"type": "error", "message": message});
    emit(state, out, id, &ev).await;
    persist_session(state, sess, id).await;
    let done = done_event(turn_ms(started_at), &agent, 0.0);
    emit(state, out, id, &done).await;
    end_of_turn(state, sess, id, &ctx.handle, &sess.parent_id).await;
}

fn turn_ms(started_at: Instant) -> i64 {
    started_at
        .elapsed()
        .as_millis()
        .max(1)
        .min(i64::MAX as u128) as i64
}

enum PromptOutcome {
    Done,
    Cancelled,
    Error(String),
}

async fn prompt_once(
    state: &Arc<AppState>,
    sess: &mut Session,
    id: &str,
    out: &EventOut,
    ctx: &TurnCtx,
    conn: &Arc<atom_tools::acp::AcpConnection>,
    acp_session_id: &str,
) -> PromptOutcome {
    let prompt_started = Instant::now();
    // Images ride along as ACP image blocks; the text block is first.
    let user = sess
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .cloned()
        .unwrap_or_default();
    let mut blocks = vec![json!({"type": "text", "text": user.content})];
    for img in &user.images {
        blocks.push(json!({
            "type": "image",
            "mimeType": img.mime,
            "data": img.data,
        }));
    }

    let mut notifs = conn.subscribe();
    let prompt_conn = conn.clone();
    let params = json!({"sessionId": acp_session_id, "prompt": blocks});
    let prompt_task = tokio::spawn(async move {
        prompt_conn
            .call_timeout("session/prompt", params, None)
            .await
    });
    tokio::pin!(prompt_task);

    let mut tracker = UpdateTracker::default();
    let mut reply = String::new();
    let mut reasoning = String::new();
    let mut prompt_tokens = 0u64;
    let mut turn_usage = None;
    // A cancel sends one session/cancel notification, then waits a
    // bounded tail for the agent's prompt response.
    let turn_cancel = ctx.handle.cancel_token();
    let parent_cancel = ctx.parent.clone();

    loop {
        tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => {
                let _ = conn
                    .notify("session/cancel", json!({"sessionId": acp_session_id}))
                    .await;
                // The agent now owes a prompt response; wait for it with
                // a bounded tail so an agent that ignores cancel cannot
                // wedge the turn (the pending call is dropped when the
                // process exits, so no protocol leak either way).
                match tokio::time::timeout(CANCEL_TAIL, &mut prompt_task).await {
                    Ok(Ok(Ok(_))) | Err(_) => return PromptOutcome::Cancelled,
                    Ok(Ok(Err(_))) | Ok(Err(_)) => return PromptOutcome::Cancelled,
                }
            }
            _ = parent_cancel.cancelled() => {
                let _ = conn
                    .notify("session/cancel", json!({"sessionId": acp_session_id}))
                    .await;
                match tokio::time::timeout(CANCEL_TAIL, &mut prompt_task).await {
                    Ok(Ok(Ok(_))) | Err(_) => return PromptOutcome::Cancelled,
                    Ok(Ok(Err(_))) | Ok(Err(_)) => return PromptOutcome::Cancelled,
                }
            }
            n = notifs.recv() => {
                match n {
                    Ok((_, params)) => {
                        if jstr(&params, "sessionId") == acp_session_id {
                            if let Some(Update::UsageUpdate { used, size, cost }) =
                                params.get("update").and_then(parse_update)
                            {
                                let mut u = sess.usage.clone().unwrap_or_default();
                                u.total_tokens = used;
                                if size > 0 {
                                    u.context_window = size;
                                }
                                if cost > 0.0 {
                                    u.cost = cost;
                                }
                                sess.usage = Some(u.clone());
                                emit(state, out, id, &crate::turn::usage_event(&u)).await;
                            }
                        }
                        handle_notification(
                            state, out, id, &mut tracker, &mut reply, &mut reasoning,
                            acp_session_id, &params,
                        )
                        .await;
                        if params.get("update").and_then(|u| u.get("sessionUpdate")).and_then(Value::as_str)
                            == Some("config_option_update")
                        {
                            if let Some(options) = params
                                .get("update")
                                .and_then(|u| u.get("configOptions"))
                            {
                                sess.acp_config_options = options.clone();
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("atoms: acp turn {id} dropped {n} agent notifications");
                    }
                    Err(_) => {
                        // Channel closed: the agent process is gone.
                        return PromptOutcome::Error("agent process exited".into());
                    }
                }
            }
            res = &mut prompt_task => {
                let result = match res {
                    Ok(Ok(result)) => result,
                    Ok(Err(err)) => {
                        return PromptOutcome::Error(err.message);
                    }
                    Err(join_err) => {
                        return PromptOutcome::Error(format!("prompt task died: {join_err}"));
                    }
                };
                let _stop = jstr(&result, "stopReason");
                prompt_tokens = acp_output_tokens(&result);
                turn_usage = acp_response_usage(&result);
                #[cfg(debug_assertions)]
                eprintln!("atoms: acp prompt response {}: {result}", conn.name());
                break;
            }
        }
    }

    // Record the assistant reply (text + reasoning; no tool calls — the
    // agent owns those) so navigation and stats see the turn.
    let duration_ms = turn_ms(prompt_started);
    let tokens_per_sec = if prompt_tokens > 0 {
        prompt_tokens as f64 / (duration_ms as f64 / 1000.0)
    } else {
        0.0
    };
    if let Some(turn) = &turn_usage {
        fold_turn_usage(sess, turn);
        if let Some(u) = &sess.usage {
            emit(state, out, id, &crate::turn::usage_event(u)).await;
        }
    }
    sess.messages.push(Message {
        usage: turn_usage,
        role: "assistant".into(),
        content: reply.clone(),
        reasoning: reasoning.clone(),
        provider: atom_tools::acp::ACP_PROVIDER_NAME.into(),
        model: conn.name().to_string(),
        created_at: Some(Utc::now()),
        duration_ms,
        tokens_per_sec,
        ..Default::default()
    });
    persist_session(state, sess, id).await;
    PromptOutcome::Done
}

#[allow(clippy::too_many_arguments)]
async fn handle_notification(
    state: &Arc<AppState>,
    out: &EventOut,
    id: &str,
    tracker: &mut UpdateTracker,
    reply: &mut String,
    reasoning: &mut String,
    acp_session_id: &str,
    params: &Value,
) {
    let session_id = jstr(params, "sessionId");
    if session_id != acp_session_id {
        // Another atom session sharing this agent process.
        return;
    }
    let Some(update) = params.get("update").and_then(parse_update) else {
        return;
    };
    for ev in update_events(tracker, &update) {
        match (ev.get("type").and_then(Value::as_str), ev.get("text")) {
            (Some("content"), Some(Value::String(t))) => reply.push_str(t),
            (Some("reasoning"), Some(Value::String(t))) => reasoning.push_str(t),
            _ => {}
        }
        emit(state, out, id, &ev).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atom_tools::acp::{ContentBlock, PlanEntry, ToolCallContent};

    fn chunk(text: &str) -> Update {
        Update::AgentMessageChunk {
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn message_chunks_stream_as_content_events() {
        let mut t = UpdateTracker::default();
        let evs = update_events(&mut t, &chunk("hel"));
        assert_eq!(evs, vec![json!({"type": "content", "text": "hel"})]);
        let evs = update_events(&mut t, &chunk("lo"));
        assert_eq!(evs, vec![json!({"type": "content", "text": "lo"})]);
    }

    #[test]
    fn thought_chunk_opens_reasoning_and_content_closes_it() {
        let mut t = UpdateTracker::default();
        assert_eq!(
            update_events(
                &mut t,
                &Update::AgentThoughtChunk {
                    content: vec![ContentBlock::text("thinking…")],
                }
            ),
            vec![json!({"type": "reasoning", "text": "thinking…"})]
        );
        let evs = update_events(&mut t, &chunk("answer"));
        assert_eq!(
            evs,
            vec![
                json!({"type": "reasoning_end"}),
                json!({"type": "content", "text": "answer"}),
            ]
        );
        // Only the first hand-off emits the close.
        let evs = update_events(&mut t, &chunk("more"));
        assert_eq!(evs, vec![json!({"type": "content", "text": "more"})]);
    }

    #[test]
    fn tool_call_start_and_completion_produce_block_and_result() {
        let mut t = UpdateTracker::default();
        let evs = update_events(
            &mut t,
            &Update::ToolCall {
                id: "t1".into(),
                title: "Read src/main.rs".into(),
                kind: "read".into(),
                status: "in_progress".into(),
                contents: vec![],
                raw_input: Some(json!({"file_path": "src/main.rs"})),
            },
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], json!("tool"));
        assert_eq!(evs[0]["name"], json!("read_file"));
        assert_eq!(evs[0]["arguments"], json!(r#"{"path":"src/main.rs"}"#));
        assert_eq!(evs[0]["call_id"], json!("t1"));

        let evs = update_events(
            &mut t,
            &Update::ToolCallUpdate {
                id: "t1".into(),
                title: "Read src/main.rs".into(),
                kind: "read".into(),
                status: "completed".into(),
                contents: vec![ToolCallContent {
                    typ: "content".into(),
                    content: Some(ContentBlock::text("file contents here")),
                    ..Default::default()
                }],
                raw_input: None,
                raw_output: None,
            },
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], json!("tool_result"));
        assert_eq!(evs[0]["text"], json!("file contents here"));
        assert_eq!(evs[0]["call_id"], json!("t1"));

        // A second completed update for the same call is suppressed.
        let evs = update_events(
            &mut t,
            &Update::ToolCallUpdate {
                id: "t1".into(),
                title: String::new(),
                kind: String::new(),
                status: "completed".into(),
                contents: vec![],
                raw_input: None,
                raw_output: None,
            },
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn execute_call_opens_once_input_arrives_as_bash() {
        let mut t = UpdateTracker::default();
        let announce = Update::ToolCall {
            id: "t3".into(),
            title: "Terminal".into(),
            kind: "execute".into(),
            status: "pending".into(),
            contents: vec![],
            raw_input: Some(json!({})),
        };
        assert!(update_events(&mut t, &announce).is_empty());
        let evs = update_events(
            &mut t,
            &Update::ToolCallUpdate {
                id: "t3".into(),
                title: String::new(),
                kind: String::new(),
                status: "in_progress".into(),
                contents: vec![],
                raw_input: Some(json!({"command": "cargo check"})),
                raw_output: None,
            },
        );
        assert_eq!(evs[0]["name"], json!("bash"));
        assert_eq!(evs[0]["arguments"], json!(r#"{"command":"cargo check"}"#));
    }

    #[test]
    fn usage_update_parses_context_size() {
        let update = parse_update(&json!({
            "sessionUpdate": "usage_update", "used": 1200, "size": 200000
        }));
        assert!(matches!(
            update,
            Some(Update::UsageUpdate { used: 1200, size: 200000, .. })
        ));
    }

    #[test]
    fn diffs_become_tool_diff_events() {
        let mut t = UpdateTracker::default();
        let evs = update_events(
            &mut t,
            &Update::ToolCall {
                id: "t2".into(),
                title: "Edit src/main.rs".into(),
                kind: "edit".into(),
                status: "completed".into(),
                contents: vec![ToolCallContent {
                    typ: "diff".into(),
                    path: "src/main.rs".into(),
                    old_text: Some("old".into()),
                    new_text: Some("new".into()),
                    content: None,
                }],
                raw_input: None,
            },
        );
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0]["type"], json!("tool"));
        assert_eq!(evs[1]["type"], json!("tool_result"));
        assert_eq!(evs[1]["text"], json!("src/main.rs: edited"));
        assert_eq!(evs[2]["type"], json!("tool_diff"));
        assert_eq!(
            evs[2]["diff"],
            json!("--- a/src/main.rs\n+++ b/src/main.rs\n-old\n+new")
        );
    }

    #[test]
    fn plans_render_as_a_live_block() {
        let mut t = UpdateTracker::default();
        let evs = update_events(
            &mut t,
            &Update::Plan {
                entries: vec![PlanEntry {
                    content: "Write the parser".into(),
                    priority: "high".into(),
                    status: "in_progress".into(),
                }],
            },
        );
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["name"], json!("Plan"));
        assert_eq!(evs[1]["text"], json!("[>] Write the parser [high]"));
    }

    #[test]
    fn permission_outcome_matches_option_ids() {
        let req = PermissionRequest {
            session_id: "s".into(),
            tool_call_id: "t".into(),
            title: "cmd".into(),
            kind: String::new(),
            raw_input: None,
            options: vec![atom_tools::acp::PermissionOption {
                id: "opt-allow".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            }],
        };
        let out = permission_outcome(&req, Decision::AllowOnce);
        assert_eq!(
            out,
            json!({"outcome": {"outcome": "selected", "optionId": "opt-allow"}})
        );
        // Deny maps to reject_once; when the agent offered no reject
        // option the reply is a bare cancel, which the spec permits.
        let out = permission_outcome(&req, Decision::DenyOnce);
        assert_eq!(out, json!({"outcome": {"outcome": "cancelled"}}));
    }

    const FAKE_AGENT_SCRIPT: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"session/new"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"acs1","models":{"currentModelId":"default","availableModels":[{"modelId":"default","name":"Default"},{"modelId":"opus","name":"Opus"}]},"modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]}}}'
      ;;
    *'"session/set_model"'*)
      model=$(printf '%s' "$line" | sed -n 's/.*"modelId":"\([^"]*\)".*/\1/p')
      echo '{"jsonrpc":"2.0","id":77,"method":"fs/write_text_file","params":{"path":"__FS_WRITE_TARGET__","content":"model:'"$model"'"}}'
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
    *'"session/set_mode"'*)
      mode=$(printf '%s' "$line" | sed -n 's/.*"modeId":"\([^"]*\)".*/\1/p')
      echo '{"jsonrpc":"2.0","id":78,"method":"fs/write_text_file","params":{"path":"__FS_WRITE_TARGET__","content":"mode:'"$mode"'"}}'
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
    *'"session/prompt"'*)
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acs1","update":{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Run go test","kind":"execute","status":"in_progress","rawInput":{"command":"go test"}}}}'
      echo '{"jsonrpc":"2.0","id":55,"method":"session/request_permission","params":{"sessionId":"acs1","options":[{"optionId":"opt-a","name":"Allow","kind":"allow_once"}],"toolCall":{"toolCallId":"tc1","title":"Run go test","kind":"execute","rawInput":{"command":"go test"}}}}'
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acs1","update":{"sessionUpdate":"tool_call_update","toolCallId":"tc1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"all tests pass"}}]}}}'
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acs1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"tests green"}}}}'
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}'
      ;;
    *)
      resp=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/{"jsonrpc":"2.0","id":\1,"result":{}}/p')
      [ -n "$resp" ] && echo "$resp"
      ;;
  esac
done"#;

    /// A config-options agent (Devin style): session/new advertises a
    /// model option with an EMPTY value list plus a thought_level
    /// ladder; set_config_option records the selection by fs write and
    /// rejects the "gated" value the way account-gated models fail.
    const FAKE_CONFIG_OPTION_SCRIPT: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"session/new"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acs1","update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":"code","options":[{"value":"code","name":"Code"}]},{"id":"model","name":"Model","category":"model","type":"select","currentValue":"m-fast","options":[]}]}}}'
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"acs1","configOptions":[{"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":"code","options":[{"value":"code","name":"Code"}]},{"id":"model","name":"Model","category":"model","type":"select","currentValue":"m-fast","options":[]},{"id":"thought_level","name":"Thought Level","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}]}}'
      ;;
    *'"session/set_config_option"'*)
      value=$(printf '%s' "$line" | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')
      configid=$(printf '%s' "$line" | sed -n 's/.*"configId":"\([^"]*\)".*/\1/p')
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      if [ "$value" = "gated" ]; then
        echo '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32602,"message":"Invalid value '"$value"' for config option '"$configid"'"}}'
      else
        [ "$configid" = model ] && lastmodel=$value
        echo '{"jsonrpc":"2.0","id":79,"method":"fs/write_text_file","params":{"path":"__FS_WRITE_TARGET__","content":"config:'"$configid"'='"$value"'"}}'
        echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"'"$lastmodel"'","options":[{"value":"'"$lastmodel"'","name":"Chosen"}]},{"id":"thought_level","name":"Thought Level","category":"thought_level","type":"select","currentValue":"high","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}]}}'
      fi
      ;;
    *'"session/prompt"'*)
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acs1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"tests green"}}}}'
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}'
      ;;
    *)
      resp=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/{"jsonrpc":"2.0","id":\1,"result":{}}/p')
      [ -n "$resp" ] && echo "$resp"
      ;;
  esac
done"#;

    fn write_project_config(cwd: &std::path::Path) {
        write_agent_config(cwd, "fake", FAKE_AGENT_SCRIPT);
    }

    fn write_agent_config(cwd: &std::path::Path, name: &str, script: &str) {
        std::fs::create_dir_all(cwd.join(".atom")).unwrap();
        std::fs::write(
            cwd.join(".atom/acp.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "acpAgents": {
                    name: {"command": "/bin/sh", "args": ["-c", script]}
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn test_state(dir: &std::path::Path) -> Arc<AppState> {
        let store = Arc::new(atom_core::session::store::SessionStore::open_in_dir(dir).unwrap());
        Arc::new(AppState::new(
            store,
            atom_sandbox::policy::SandboxConfig::default(),
            Arc::new(crate::state::ConnTracker::default()),
        ))
    }

    /// E2E: a shell-based fake agent is spawned through the real hub,
    /// the turn runs session/new → session/prompt, the agent streams a
    /// tool call, asks for permission (answered via the approval hub),
    /// streams a reply, and the transcript is persisted.
    #[tokio::test]
    async fn end_to_end_turn_against_fake_agent() {
        use crate::turn::EventOut;
        use bytes::Bytes;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let cwd = dir.path().join("proj");
        write_project_config(&cwd);
        let mut sess = state
            .store
            .create("fake", cwd.to_str().unwrap(), vec![])
            .clone();
        sess.provider = atom_tools::acp::ACP_PROVIDER_NAME.into();
        sess.messages.push(Message {
            role: "user".into(),
            content: "run the tests".into(),
            ..Default::default()
        });

        // Auto-answer the permission prompt once it appears.
        let approval_state = state.clone();
        let approval_sid = sess.id.clone();
        tokio::spawn(async move {
            for _ in 0..250 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let pending = approval_state.approvals.pending(&approval_sid);
                if let Some((aid, _)) = pending.first() {
                    approval_state.approvals.complete(
                        &approval_sid,
                        aid,
                        atom_sandbox::approvals::Decision::AllowOnce,
                    );
                    return;
                }
            }
        });

        let handle = state.turns.start_turn(&sess.id, "t1");
        let ctx = TurnCtx {
            handle,
            parent: crate::cancel::CancelToken::new(),
        };
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ev_sink = events.clone();
        tokio::spawn(async move {
            while let Some(Ok(chunk)) = rx.recv().await {
                let text = String::from_utf8_lossy(&chunk).to_string();
                for line in text.lines() {
                    ev_sink.lock().unwrap().push(line.to_string());
                }
            }
        });
        let out = EventOut::Response(tx);
        let session_id = sess.id.clone();
        run_acp_turn(&state, &mut sess, &session_id, out, &ctx, Instant::now()).await;
        for line in events.lock().unwrap().iter() {
            eprintln!("EVENT: {line}");
        }

        let stored = state.store.get(&sess.id).unwrap();
        let assistant = stored
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .expect("assistant message recorded");
        assert_eq!(assistant.content, "tests green");
        assert_eq!(assistant.provider, "acp-agent");
        assert_eq!(stored.acp_session_id, "acs1");
        // No active turn left behind.
        assert!(!state.turns.session_has_active_turn(&sess.id));
    }

    /// Model picker rows carry `agent/value`; the turn must resolve the
    /// agent and push the value through session/set_model for agents
    /// that advertise the spec's models object (Claude Code's adapter).
    #[tokio::test]
    async fn model_selection_rides_session_set_model() {
        use crate::turn::EventOut;
        use bytes::Bytes;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let cwd = dir.path().join("proj");
        let target = dir.path().join("setmodel.txt");
        write_agent_config(
            &cwd,
            "fake",
            &FAKE_AGENT_SCRIPT.replace("__FS_WRITE_TARGET__", &target.display().to_string()),
        );
        let mut sess = state
            .store
            .create("fake/opus", cwd.to_str().unwrap(), vec![])
            .clone();
        sess.provider = atom_tools::acp::ACP_PROVIDER_NAME.into();
        sess.acp_selected = json!({"model": "opus"});
        sess.messages.push(Message {
            role: "user".into(),
            content: "run the tests".into(),
            ..Default::default()
        });

        // The fake's prompt flow asks for tool permission; auto-allow it.
        let approval_state = state.clone();
        let approval_sid = sess.id.clone();
        tokio::spawn(async move {
            for _ in 0..250 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let pending = approval_state.approvals.pending(&approval_sid);
                if let Some((aid, _)) = pending.first() {
                    approval_state.approvals.complete(
                        &approval_sid,
                        aid,
                        atom_sandbox::approvals::Decision::AllowOnce,
                    );
                    return;
                }
            }
        });

        let handle = state.turns.start_turn(&sess.id, "t1");
        let ctx = TurnCtx {
            handle,
            parent: crate::cancel::CancelToken::new(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);
        let session_id = sess.id.clone();
        run_acp_turn(
            &state,
            &mut sess,
            &session_id,
            EventOut::Response(tx),
            &ctx,
            Instant::now(),
        )
        .await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "model:opus",
            "the picked model reached the agent via session/set_model"
        );
        let stored = state.store.get(&sess.id).unwrap();
        assert_eq!(stored.acp_session_id, "acs1");
        assert!(
            stored.acp_models.get("availableModels").is_some(),
            "the agent's models object is persisted for later turns: {:?}",
            stored.acp_models
        );
        assert_eq!(stored.acp_models["currentModelId"], json!("default"));
    }

    /// Devin-style agents: the model rides session/set_config_option,
    /// including the priming round-trip their empty options list needs,
    /// and the thought_level ladder picks up the session's thinking.
    #[tokio::test]
    async fn model_and_thinking_ride_session_set_config_option() {
        use crate::turn::EventOut;
        use bytes::Bytes;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let cwd = dir.path().join("proj");
        let target = dir.path().join("setconfig.txt");
        write_agent_config(
            &cwd,
            "fake",
            &FAKE_CONFIG_OPTION_SCRIPT
                .replace("__FS_WRITE_TARGET__", &target.display().to_string()),
        );
        let mut sess = state
            .store
            .create("fake/swe-1-6-slow", cwd.to_str().unwrap(), vec![])
            .clone();
        sess.provider = atom_tools::acp::ACP_PROVIDER_NAME.into();
        sess.acp_selected = json!({"model": "swe-1-6-slow"});
        sess.thinking = "low".into();
        sess.messages.push(Message {
            role: "user".into(),
            content: "run the tests".into(),
            ..Default::default()
        });

        let handle = state.turns.start_turn(&sess.id, "t1");
        let ctx = TurnCtx {
            handle,
            parent: crate::cancel::CancelToken::new(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);
        let session_id = sess.id.clone();
        run_acp_turn(
            &state,
            &mut sess,
            &session_id,
            EventOut::Response(tx),
            &ctx,
            Instant::now(),
        )
        .await;

        // Last write wins: the model selection overwrote the priming
        // write, then the thinking level overwrote the model.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "config:thought_level=low"
        );
        let stored = state.store.get(&sess.id).unwrap();
        let options = atom_tools::acp::parse_config_options(&stored.acp_config_options);
        let model = atom_tools::acp::config_option_for_category(&options, "model").unwrap();
        assert_eq!(model.current_value, json!("swe-1-6-slow"));
        // The notification's mode list (missing from the result) won.
        let mode = atom_tools::acp::config_option_for_category(&options, "mode").unwrap();
        assert!(mode.options.iter().any(|v| v.value == "code"));
    }

    /// An account-gated model (agent rejects the value) fails the turn
    /// with a visible error instead of silently running the default.
    #[tokio::test]
    async fn rejected_model_selection_fails_the_turn_loudly() {
        use crate::turn::EventOut;
        use bytes::Bytes;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let cwd = dir.path().join("proj");
        write_agent_config(
            &cwd,
            "fake",
            &FAKE_CONFIG_OPTION_SCRIPT.replace(
                "__FS_WRITE_TARGET__",
                &dir.path().join("never.txt").display().to_string(),
            ),
        );
        let mut sess = state
            .store
            .create("fake/gated", cwd.to_str().unwrap(), vec![])
            .clone();
        sess.provider = atom_tools::acp::ACP_PROVIDER_NAME.into();
        sess.acp_selected = json!({"model": "gated"});
        sess.messages.push(Message {
            role: "user".into(),
            content: "run the tests".into(),
            ..Default::default()
        });

        let handle = state.turns.start_turn(&sess.id, "t1");
        let ctx = TurnCtx {
            handle,
            parent: crate::cancel::CancelToken::new(),
        };
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ev_sink = events.clone();
        tokio::spawn(async move {
            while let Some(Ok(chunk)) = rx.recv().await {
                for line in String::from_utf8_lossy(&chunk).lines() {
                    ev_sink.lock().unwrap().push(line.to_string());
                }
            }
        });
        let session_id = sess.id.clone();
        run_acp_turn(
            &state,
            &mut sess,
            &session_id,
            EventOut::Response(tx),
            &ctx,
            Instant::now(),
        )
        .await;

        let lines = events.lock().unwrap().join("\n");
        assert!(
            lines.contains("gated") && lines.contains("\"error\""),
            "turn surfaces the rejected selection: {lines}"
        );
        let stored = state.store.get(&sess.id).unwrap();
        assert!(
            !stored.messages.iter().any(|m| m.role == "assistant"),
            "no assistant reply when the selection was rejected"
        );
        assert!(!state.turns.session_has_active_turn(&sess.id));
    }

    /// The run_session_turn branch: a session with provider "acp-agent"
    /// sent through the real turn entry (what POST /send does) runs the
    /// ACP driver, not the model loop.
    #[tokio::test]
    async fn send_path_routes_acp_sessions_through_the_acp_driver() {
        use crate::turn::{EventOut, TurnOpts};
        use bytes::Bytes;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let cwd = dir.path().join("proj");
        write_project_config(&cwd);
        let mut sess = state
            .store
            .create("fake", cwd.to_str().unwrap(), vec![])
            .clone();
        sess.provider = atom_tools::acp::ACP_PROVIDER_NAME.into();
        state.store.save(&sess);

        let approval_state = state.clone();
        let approval_sid = sess.id.clone();
        tokio::spawn(async move {
            for _ in 0..250 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let pending = approval_state.approvals.pending(&approval_sid);
                if let Some((aid, _)) = pending.first() {
                    approval_state.approvals.complete(
                        &approval_sid,
                        aid,
                        atom_sandbox::approvals::Decision::AllowOnce,
                    );
                    return;
                }
            }
        });

        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);
        let session_id = sess.id.clone();
        crate::turn::run_session_turn_guarded(
            &state,
            &mut sess,
            &session_id,
            TurnOpts {
                message: "run the tests".into(),
                turn_id: "t1".into(),
                ..empty_turn_opts()
            },
            EventOut::Response(tx),
            crate::cancel::CancelToken::new(),
        )
        .await;

        let stored = state.store.get(&session_id).unwrap();
        assert!(
            stored
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content == "tests green"),
            "assistant reply missing: {:?}",
            stored
                .messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(stored.acp_session_id, "acs1");
    }

    fn empty_turn_opts() -> crate::turn::TurnOpts {
        crate::turn::TurnOpts {
            message: String::new(),
            thinking: String::new(),
            key: String::new(),
            base_url: String::new(),
            reasoning_field: String::new(),
            turn_id: String::new(),
            images: Vec::new(),
            compact: false,
            compact_instructions: String::new(),
            skip_append: false,
        }
    }
}
