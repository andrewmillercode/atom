//! events.rs defines the runtime message flow: crossterm input, server
//! stream events (the Rust shape of Go's streamMsg), async results
//! arriving as AppMsg, and the Effects handlers ask the loop to run
//! (Bubble Tea Cmd analog).

use std::time::Duration;

use serde_json::Value;

use atom_core::providers::auth::AuthEntry;
use atom_core::providers::providers::ModelEntry;
use atom_core::session::context_breakdown::ContextRow;
use atom_core::session::stats::StatsReport;
use atom_core::session::store::Session;
use atom_core::session::store::SessionInfo;
use atom_core::types::StreamUsage;

/// streamMsg carries one NDJSON event from the server's /send or
/// /events endpoint (parseStreamLine port).
#[derive(Debug, Clone, Default)]
pub struct StreamEvent {
    pub event_type: String,
    pub text: String,
    pub name: String,
    pub arguments: String,
    pub message: String,
    pub diff: String,
    /// Model that completed a turn, set on `done` events.
    pub model: String,
    /// Provider-reported reasoning duration or server-measured turn duration.
    pub duration: Option<Duration>,
    /// set for "usage" events when total > 0
    pub usage: Option<StreamUsage>,
    // approval_request fields
    pub id: String,
    pub session_id: String,
    pub command: String,
    pub cwd: String,
    pub rule_id: String,
    pub reason: String,
    /// Non-empty when the request comes from a dispatched subagent.
    pub child_title: String,
    pub from_subagent: bool,
    /// v2 origin tag: "self" when the prompt is for the current
    /// session, "child" when it surfaced via a dispatched subagent.
    /// Mirrors `from_subagent` but is the wire-level name the server
    /// uses (defaults to "self" when missing for back-compat with v1).
    pub origin: String,
    /// Prefix-rule preview for `[a] accept-all` (e.g. `"cargo test *"`).
    /// Pre-computed by the server; the TUI just renders it.
    pub accept_all_preview: Option<String>,
}

fn jstr(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// parseStreamEvent decodes one NDJSON JSON object into a StreamEvent.
pub fn parse_stream_event(v: &Value) -> StreamEvent {
    let mut ev = StreamEvent {
        event_type: jstr(v, "type"),
        text: jstr(v, "text"),
        name: jstr(v, "name"),
        arguments: jstr(v, "arguments"),
        message: jstr(v, "message"),
        diff: jstr(v, "diff"),
        model: jstr(v, "model"),
        id: jstr(v, "id"),
        session_id: jstr(v, "session_id"),
        command: jstr(v, "command"),
        cwd: jstr(v, "cwd"),
        rule_id: jstr(v, "rule_id"),
        reason: jstr(v, "reason"),
        child_title: jstr(v, "child_title"),
        from_subagent: v
            .get("from_subagent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // v2 server emits an explicit "origin" tag ("self" | "child").
        // Older payloads only carry from_subagent; fall back to a
        // best-effort derived value so the renderer doesn't have to
        // special-case missing keys.
        origin: {
            let raw = jstr(v, "origin");
            if !raw.is_empty() {
                raw
            } else if v
                .get("from_subagent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "child".to_string()
            } else {
                "self".to_string()
            }
        },
        accept_all_preview: v
            .get("accept_all_preview")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        ..Default::default()
    };
    let ms = jstr(v, "duration_ms").parse::<f64>().ok();
    if let Some(ms) = ms.filter(|m| *m > 0.0) {
        ev.duration = Some(Duration::from_secs_f64(ms / 1000.0));
    }
    // usage events carry session Input/Output totals (prompt/
    // completion) and the latest-round context size (total).
    let total_raw = jstr(v, "total");
    if !total_raw.is_empty() {
        let num = |k: &str| jstr(v, k).parse::<i64>().unwrap_or(0);
        let total = num("total");
        if total > 0 {
            ev.usage = Some(StreamUsage {
                prompt_tokens: num("prompt"),
                completion_tokens: num("completion"),
                total_tokens: total,
                cache_read_tokens: num("cache_read"),
                cache_write_tokens: num("cache_write"),
                prompt_tokens_all: num("prompt_all"),
                ..Default::default()
            });
        }
    }
    ev
}

/// One body for POST /api/sessions/{id}/send.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    pub session_id: String,
    pub turn_id: String,
    pub message: String,
    pub thinking: String,
    pub images: Vec<atom_core::types::ImageData>,
    pub key: String,
    pub base_url: String,
    pub reasoning_field: String,
    pub compact: bool,
    pub compact_instructions: String,
}

impl SendRequest {
    pub fn to_body(&self) -> Value {
        serde_json::json!({
            "message": self.message,
            "thinking": self.thinking,
            "key": self.key,
            "base_url": self.base_url,
            "reasoning_field": self.reasoning_field,
            "turn_id": self.turn_id,
            "images": self.images,
            "compact": self.compact,
            "compact_instructions": self.compact_instructions,
        })
    }
}

/// Effects are async actions a handler asks the loop to perform
/// (tea.Cmd analog). Results come back as AppMsgs.
#[derive(Debug)]
pub enum Effect {
    Quit,
    EnsureCatalog,
    FetchModels,
    FetchSessions,
    FetchStats {
        days: i64,
    },
    /// /profile: spawn a one-shot `ps` against the client and server
    /// pids so the overlay can show live resource utilization. The
    /// snapshot is delivered back as AppMsg::ProfileLoaded.
    FetchProfile {
        client_pid: i32,
        server_pid: Option<i32>,
    },
    LoadSession {
        id: String,
    },
    ListChildren {
        id: String,
    },
    FetchContext {
        id: String,
    },
    /// /fork: fetch the source session's messages so we can build the
    /// picker rows. The result comes back as AppMsg::ForkSourceLoaded.
    LoadForkSource {
        id: String,
    },
    /// /fork confirm: POST /api/sessions/{source_id}/fork with the
    /// optional position of the row the user picked. The server
    /// returns `{ info, draft }`; the App then subscribes to the new
    /// session and pre-fills the prompt.
    ForkSession {
        source_id: String,
        position: Option<i64>,
    },
    Subscribe {
        id: String,
    },
    SubscribeAfter {
        id: String,
        delay_ms: u64,
    },
    CreateSession {
        provider: String,
        model: String,
        cwd: String,
        thinking: String,
    },
    PatchSessionModel {
        provider: String,
        model: String,
        thinking: String,
    },
    PatchSessionThinking,
    DeleteSession {
        id: String,
    },
    PauseTurn,
    /// Mid-turn submit: the prompt is handed to the running turn
    /// (POST /send queues it on the live turn; the server answers with
    /// a tiny {"type":"injected"} stream and closes). Nothing is
    /// paused and the already-open event stream keeps painting; the
    /// App never stores a copy of the submitted prompt.
    InjectTurn {
        req: Box<SendRequest>,
    },
    Compact {
        instructions: String,
    },
    SendTurn(Box<SendRequest>),
    RespondApproval {
        /// Session that owns the pending approval (POST /approval target).
        /// Differs from the viewer's session when answering for a subagent.
        sid: String,
        id: String,
        decision: String,
    },
    StartOpenAIOAuth,
    /// Run OAuth browser sign-in for an MCP server that has
    /// `"auth": "oauth"` and is not yet authenticated. The runtime
    /// re-uses atom_tools::mcp_oauth::bearer_token to discover the
    /// metadata, walk PKCE, and store the resulting access/refresh
    /// tokens under the per-server auth key.
    StartMcpOAuth {
        server: String,
        url: String,
        client_id: String,
        client_secret: String,
        token_endpoint_auth_method: Option<String>,
    },
    ReloadProviders,
    ReadClipboard,
    CopyToClipboard {
        text: String,
    },
    /// Open a clicked OSC 8 hyperlink (http(s) or file URL) with the
    /// platform opener.
    OpenLink {
        uri: String,
    },
    /// Shell mode: run a user-typed command from the session cwd. The
    /// result comes back as AppMsg::ShellDone.
    RunShell {
        cmd: String,
        cwd: String,
    },
    /// Shell mode `cd`: persist the new working directory on the session
    /// so the agent's tools follow the shell.
    PatchSessionCwd {
        id: String,
        cwd: String,
    },
    PaintPreviews,
    /// Normalize, encode and base64 a freshly pasted image on the blocking
    /// pool. The marker is in the prompt already; this just fills the
    /// pending slot with the heavy bytes later via `AppMsg::PendingImageReady`.
    PreparePendingImage {
        num: usize,
        name: String,
        data: Vec<u8>,
    },
}

/// AppMsg is everything the main select! loop feeds into the App.
pub enum AppMsg {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize(u16, u16),
    Paste(String),
    ModelsLoaded(Vec<ModelEntry>),
    SessionsLoaded(Vec<SessionInfo>),
    ChildrenLoaded {
        id: String,
        agents: Vec<SessionInfo>,
    },
    SessionLoaded(Box<Session>),
    /// /fork: the source session loaded. The App filters the user
    /// messages into picker rows and clears the loading spinner.
    ForkSourceLoaded {
        id: String,
        sess: Box<Session>,
    },
    /// /fork: server created the child session. `info` is the new
    /// session summary; `draft` is the pre-filled prompt text (empty
    /// when the user picked the SessionLatest row).
    ForkedSession {
        info: Box<SessionInfo>,
        draft: String,
    },
    CreatedSession(Box<SessionInfo>),
    ProvidersRebuilt(Vec<atom_core::providers::providers::Provider>),
    ContextLoaded(Vec<ContextRow>),
    StatsLoaded(Result<Box<StatsReport>, String>),
    /// /profile: snapshot of CPU/RSS/VSZ/etime for both processes,
    /// delivered after Effect::FetchProfile finishes. The Box is a
    /// future-proofing pun — the report itself stays small.
    ProfileLoaded(Result<Box<crate::profile::ProfileReport>, String>),
    ClipboardText(String),
    /// Image bytes read from the OS clipboard via Ctrl/Cmd+V (mirrors the
    /// `data` half of Go's clipboardPasteMsg; text arrives separately as
    /// ClipboardText). Only sent when the clipboard holds an image.
    ClipboardImage {
        name: String,
        data: Vec<u8>,
    },
    /// Background image normalization completed. The App swaps the
    /// PreparedImage into the matching pending slot (or drops the slot on
    /// Err) and repaints the previews.
    PendingImageReady {
        num: usize,
        result: Result<crate::preview::PreparedImage, String>,
    },
    Errored(String),
    CompactDone(Result<(), String>),
    ModelsDevReady,
    /// Internal: re-arms a subscription after a reconnect delay.
    SubscribeNow(String),
    SendStarted {
        sid: String,
    },
    /// Internal: a spawned SendTurn dial completed with the NDJSON
    /// channel in hand. The loop stores it before surfacing SendStarted.
    SendReady {
        sid: String,
        rx: tokio::sync::mpsc::Receiver<Value>,
    },
    SendEvent(Value),
    SendClosed,
    SubStarted {
        sid: String,
    },
    /// Internal: a spawned Subscribe dial completed with the NDJSON
    /// channel in hand. The loop stores it before surfacing SubStarted.
    SubReady {
        sid: String,
        rx: tokio::sync::mpsc::Receiver<Value>,
    },
    SubEvent(Value),
    SubEnded {
        sid: String,
    },
    TickSpinner,
    TickSplash(f64),
    TestSceneTick,
    /// Periodic safety-net: re-arms all select! wakeup sources so a lost
    /// crossterm wakeup self-heals within one heartbeat interval.
    Heartbeat,
    /// Internal: the terminal view regained focus and needs a full repaint.
    Redraw,
    /// Internal: the math engine finished rendering one or more display
    /// formulas; the viewport must rescan blocks for newly ready
    /// placeholder rows. Handled by the loop, not the state machine.
    MathWake,
    OAuthDone(Result<AuthEntry, String>),
    /// Result of Effect::StartMcpOAuth. The String is the server name
    /// so the App can refresh the slash catalog / picker once a fresh
    /// token is persisted.
    McpOAuthDone {
        server: String,
        result: Result<(), String>,
    },
    HotRebuilt(Result<crate::hot::HotBuild, String>),
    ThemeReloaded(Result<std::time::Duration, String>),
    /// Internal: the spawned shell command armed its kill switch. The App
    /// stores the sender so Ctrl+C can abort the running command.
    ShellKillArmed(tokio::sync::oneshot::Sender<()>),
    /// Shell mode command finished. `code` is None when the command was
    /// killed; `new_cwd` is the shell's $PWD afterwards ("" when the
    /// platform wrapper can't report it), letting `cd` move the app.
    ShellDone {
        cmd: String,
        cwd: String,
        output: String,
        code: Option<i32>,
        new_cwd: String,
    },
}

impl std::fmt::Debug for AppMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppMsg::Key(k) => write!(f, "Key({k:?})"),
            AppMsg::Mouse(m) => write!(f, "Mouse({:?})", m.kind),
            AppMsg::Resize(w, h) => write!(f, "Resize({w},{h})"),
            AppMsg::Paste(_) => write!(f, "Paste(..)"),
            AppMsg::ModelsLoaded(n) => write!(f, "ModelsLoaded({n})", n = n.len()),
            AppMsg::SessionsLoaded(n) => write!(f, "SessionsLoaded({n})", n = n.len()),
            AppMsg::ChildrenLoaded { id, agents } => {
                write!(f, "ChildrenLoaded({id}, {n})", n = agents.len())
            }
            AppMsg::SessionLoaded(s) => write!(f, "SessionLoaded({})", s.id),
            AppMsg::ForkSourceLoaded { id, sess } => {
                write!(f, "ForkSourceLoaded({id}, {} msgs)", sess.messages.len())
            }
            AppMsg::ForkedSession { info, draft } => write!(
                f,
                "ForkedSession({}, draft {} chars)",
                info.id,
                draft.chars().count()
            ),
            AppMsg::CreatedSession(s) => write!(f, "CreatedSession({})", s.id),
            AppMsg::ProvidersRebuilt(n) => write!(f, "ProvidersRebuilt({n})", n = n.len()),
            AppMsg::ContextLoaded(n) => write!(f, "ContextLoaded({n})", n = n.len()),
            AppMsg::StatsLoaded(r) => write!(f, "StatsLoaded({r:?})"),
            AppMsg::ProfileLoaded(r) => write!(f, "ProfileLoaded({:?})", r.as_ref().map(|_| ())),
            AppMsg::ClipboardText(_) => write!(f, "ClipboardText(..)"),
            AppMsg::ClipboardImage { name, data } => {
                write!(f, "ClipboardImage({name}, {} bytes)", data.len())
            }
            AppMsg::PendingImageReady { num, result } => {
                write!(
                    f,
                    "PendingImageReady({num}, {})",
                    if result.is_ok() { "ok" } else { "err" }
                )
            }
            AppMsg::Errored(e) => write!(f, "Errored({e})"),
            AppMsg::CompactDone(r) => write!(f, "CompactDone({r:?})"),
            AppMsg::ModelsDevReady => write!(f, "ModelsDevReady"),
            AppMsg::SubscribeNow(id) => write!(f, "SubscribeNow({id})"),
            AppMsg::SendStarted { sid } => write!(f, "SendStarted({sid})"),
            AppMsg::SendReady { sid, .. } => write!(f, "SendReady({sid})"),
            AppMsg::SendEvent(_) => write!(f, "SendEvent"),
            AppMsg::SendClosed => write!(f, "SendClosed"),
            AppMsg::SubStarted { sid } => write!(f, "SubStarted({sid})"),
            AppMsg::SubReady { sid, .. } => write!(f, "SubReady({sid})"),
            AppMsg::SubEvent(_) => write!(f, "SubEvent"),
            AppMsg::SubEnded { sid } => write!(f, "SubEnded({sid})"),
            AppMsg::TickSpinner => write!(f, "TickSpinner"),
            AppMsg::TickSplash(t) => write!(f, "TickSplash({t})"),
            AppMsg::TestSceneTick => write!(f, "TestSceneTick"),
            AppMsg::Heartbeat => write!(f, "Heartbeat"),
            AppMsg::Redraw => write!(f, "Redraw"),
            AppMsg::MathWake => write!(f, "MathWake"),
            AppMsg::OAuthDone(r) => write!(f, "OAuthDone({r:?})"),
            AppMsg::McpOAuthDone { server, result } => {
                write!(f, "McpOAuthDone({server}, {result:?})")
            }
            AppMsg::HotRebuilt(r) => write!(f, "HotRebuilt({r:?})"),
            AppMsg::ThemeReloaded(r) => write!(f, "ThemeReloaded({r:?})"),
            AppMsg::ShellKillArmed(_) => write!(f, "ShellKillArmed"),
            AppMsg::ShellDone { cmd, code, .. } => {
                write!(f, "ShellDone({cmd:?}, code={code:?})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_usage_event_with_stringified_numbers() {
        let ev = parse_stream_event(&json!({
            "type":"usage","prompt":"12345","completion":"678",
            "total":"13023","cache_read":"9000","cache_write":"2100",
            "prompt_all":"120000"
        }));
        assert_eq!(ev.event_type, "usage");
        let u = ev.usage.expect("usage set");
        assert_eq!(u.prompt_tokens, 12345);
        assert_eq!(u.completion_tokens, 678);
        assert_eq!(u.total_tokens, 13023);
        assert_eq!(u.cache_read_tokens, 9000);
        assert_eq!(u.cache_write_tokens, 2100);
        assert_eq!(u.prompt_tokens_all, 120000);
    }

    #[test]
    fn no_usage_without_total() {
        let ev = parse_stream_event(&json!({"type":"content","text":"hi"}));
        assert!(ev.usage.is_none());
        assert_eq!(ev.text, "hi");
        let ev = parse_stream_event(&json!({"type":"usage","total":"0"}));
        assert!(ev.usage.is_none());
    }

    #[test]
    fn duration_ms_parsed() {
        let ev = parse_stream_event(&json!({"type":"reasoning_end","duration_ms":"3200"}));
        assert_eq!(ev.duration, Some(Duration::from_millis(3200)));
        let ev = parse_stream_event(&json!({"type":"reasoning_end","duration_ms":"0"}));
        assert!(ev.duration.is_none());
    }

    #[test]
    fn send_body_matches_go_shape() {
        let req = SendRequest {
            session_id: "s".into(),
            turn_id: "42".into(),
            message: "hello".into(),
            thinking: "high".into(),
            ..Default::default()
        };
        let b = req.to_body();
        assert_eq!(b["message"], "hello");
        assert_eq!(b["turn_id"], "42");
        assert_eq!(b["compact"], false);
        assert_eq!(b["images"], serde_json::json!([]));
    }
}
