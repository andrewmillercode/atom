//! Integration tests: a real server bound to a tempfile-scoped unix
//! socket, driven through the client module, against a local mock
//! OpenAI-compatible SSE provider (mirrors server_test.go's httptest
//! style).

use atom_core::session::store::SessionStore;
use atom_sandbox::approvals::{ApprovalRequest, Approver, Decision};
use atom_sandbox::policy::SandboxConfig;
use atom_server::client;
use atom_server::dispatch::{DispatchBridge, ServerApprover};
use atom_server::state::{AppState, ConnTracker};
use atom_tools::SubagentHandle;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// XDG_DATA_HOME is process-global: serialize every env-dependent test.
static SERIAL: Mutex<()> = Mutex::new(());

/// Isolates the atom data dir for one test (and clears provider creds so
/// background title/compaction requests never leave the machine).
struct TestEnv {
    prev_xdg: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl TestEnv {
    fn new(tag: &str) -> Self {
        let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().expect("tempdir");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", dir.path());
        // Keep background title generation off the network.
        for var in [
            "OLLAMA_API_KEY",
            "OPENCODE_GO_API_KEY",
            "OPENCODE_ZEN_API_KEY",
        ] {
            unsafe { std::env::set_var(var, "") };
        }
        let _ = tag;
        TestEnv {
            prev_xdg,
            _lock: lock,
            dir,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().to_path_buf()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match &self.prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        for var in [
            "OLLAMA_API_KEY",
            "OPENCODE_GO_API_KEY",
            "OPENCODE_ZEN_API_KEY",
        ] {
            unsafe { std::env::remove_var(var) };
        }
    }
}

fn off_cfg() -> SandboxConfig {
    SandboxConfig::default()
}

fn workspace_cfg() -> SandboxConfig {
    SandboxConfig::default()
}

/// Spawns the server stack on the isolated data dir's socket path.
async fn spawn_server(env: &TestEnv, cfg: SandboxConfig) -> Arc<AppState> {
    let sessions_dir = env.path().join("sessions");
    let store = Arc::new(SessionStore::open_in_dir(&sessions_dir).unwrap());
    let state = Arc::new(AppState::new(store, cfg, Arc::new(ConnTracker::new())));
    let listener = atom_server::http::listen_on_socket(&atom_core_socket())
        .unwrap()
        .expect("socket path free");
    tokio::spawn(atom_server::http::serve_listener(listener, state.clone()));
    // Wait for readiness by dialing like real clients do.
    for _ in 0..100 {
        if client::is_running().await {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not become ready");
}

fn atom_core_socket() -> std::path::PathBuf {
    atom_core::session::store::socket_path()
}

// ---------------------------------------------------------------------------
// Mock OpenAI-compatible SSE provider.
// ---------------------------------------------------------------------------

async fn spawn_mock_provider(fixtures: Vec<String>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let idx = Arc::new(AtomicUsize::new(0));
    let fixtures = Arc::new(fixtures);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let idx = idx.clone();
            let fixtures = fixtures.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    if let Some(p) = find(&buf, b"\r\n\r\n") {
                        break p;
                    }
                    match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => break usize::MAX,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                };
                if head_end != usize::MAX {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let clen: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let want = head_end + 4 + clen;
                    while buf.len() < want {
                        match stream.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                }
                let i = idx.fetch_add(1, Ordering::SeqCst);
                let resp = fixtures
                    .get(i)
                    .cloned()
                    .or_else(|| fixtures.last().cloned())
                    .unwrap_or_default();
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn session_create_and_model_patch_persist_provider() {
    let env = TestEnv::new("session-provider");
    let _state = spawn_server(&env, off_cfg()).await;

    let created = client::post(
        "/api/sessions",
        &json!({
            "provider": "openai",
            "model": "gpt-5",
            "cwd": env.path(),
        }),
    )
    .await
    .unwrap();
    let id = created["id"].as_str().unwrap();
    assert_eq!(created["provider"], "openai");

    client::patch(
        &format!("/api/sessions/{id}"),
        &json!({
            "provider": "ollama",
            "model": "deepseek-v4-flash:0731",
        }),
    )
    .await
    .unwrap();

    let loaded = client::get(&format!("/api/sessions/{id}")).await.unwrap();
    assert_eq!(loaded["provider"], "ollama");
    assert_eq!(loaded["model"], "deepseek-v4-flash:0731");
}

/// Collects NDJSON events from a /send or /events stream until `done`
/// arrives or the deadline lapses.
async fn collect_until_done(mut rx: tokio::sync::mpsc::Receiver<Value>) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let is_done = ev["type"] == "done";
                out.push(ev);
                if is_done {
                    return out;
                }
            }
            _ => return out,
        }
    }
}

async fn collect_until_closed(mut rx: tokio::sync::mpsc::Receiver<Value>) -> (Vec<Value>, bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => return (out, true),
            Err(_) => return (out, false),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// A plain content turn streams content deltas, a usage event, and done;
/// the normalized store retains the complete assistant response.
#[tokio::test(flavor = "multi_thread")]
async fn send_streams_content_and_persists_session() {
    let env = TestEnv::new("send-content");
    let base_url = spawn_mock_provider(vec![sse_response(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\",\"reasoning\":null},\"finish_reason\":null}]}\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n",
        "data: [DONE]\n",
    ))])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let sess = state.store.create("test-model", "/tmp", vec![]);
    let rx = client::stream_send(
        &sess.id,
        &json!({"message": "say hello", "base_url": base_url}),
    )
    .await
    .unwrap();
    let (events, closed) = collect_until_closed(rx).await;
    assert!(closed, "send stream did not close after done");

    let types: Vec<String> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        types,
        vec!["round_start", "content", "content", "usage", "done"],
        "{events:?}"
    );
    assert_eq!(events[1]["text"], "Hel");
    assert_eq!(events[2]["text"], "lo");
    // Usage numbers travel as strings, exactly like Go's map[string]string.
    assert_eq!(events[3]["prompt"], "3");
    assert_eq!(events[3]["completion"], "2");
    assert_eq!(events[3]["total"], "5");
    assert_eq!(events[4]["model"], "test-model");
    assert!(events[4]["duration_ms"].as_i64().is_some_and(|ms| ms > 0));

    let persisted = state.store.get(&sess.id).unwrap();
    let roles: Vec<&str> = persisted
        .messages
        .iter()
        .map(|message| message.role.as_str())
        .collect();
    assert_eq!(roles, vec!["user", "assistant"]);
    assert_eq!(persisted.messages[1].content, "Hello");
    assert_eq!(persisted.messages[1].model, "test-model");
    assert!(persisted.messages[1].duration_ms > 0);
}

/// A second client subscribed to the same session sees the user's
/// message (user_message broadcast) before the model finishes, not only
/// after the turn's done event. The event carries the message text so
/// viewers can append it without a full reload.
#[tokio::test(flavor = "multi_thread")]
async fn subscribed_client_sees_user_message_before_done() {
    let env = TestEnv::new("sub-sees-user");
    let base_url = spawn_mock_provider(vec![sse_response(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\",\"reasoning\":null},\"finish_reason\":null}]}\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n",
        "data: [DONE]\n",
    ))])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let sess = state.store.create("test-model", "/tmp", vec![]);
    let mut sub = state.subs.subscribe(&sess.id);

    let rx = client::stream_send(
        &sess.id,
        &json!({"message": "say hello", "base_url": base_url}),
    )
    .await
    .unwrap();
    let (_events, closed) = collect_until_closed(rx).await;
    assert!(closed);

    // The user message must be persisted immediately at turn start.
    let got = state.store.get(&sess.id).unwrap();
    assert_eq!(got.messages[0].role, "user");
    assert_eq!(got.messages[0].content, "say hello");

    // Subscriber receives the user_message event before the turn's done
    // event, not only after the model finishes.
    let mut sub_events = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(15), sub.rx.recv()).await {
        let is_done = ev["type"] == "done";
        sub_events.push(ev);
        if is_done {
            break;
        }
    }
    let user_msg = sub_events
        .iter()
        .position(|e| e["type"] == "user_message")
        .expect("subscriber got user_message event");
    assert_eq!(sub_events[user_msg]["text"], "say hello");
    let done_at = sub_events
        .iter()
        .position(|e| e["type"] == "done")
        .expect("subscriber got done event");
    assert!(
        user_msg < done_at,
        "user_message must precede done: {sub_events:?}"
    );
}

/// A tool-call turn executes the tool (sandbox Off), feeds the result
/// back, and finishes when the model stops calling tools.
#[tokio::test(flavor = "multi_thread")]
async fn send_runs_tool_rounds() {
    let env = TestEnv::new("send-tool");
    let base_url = spawn_mock_provider(vec![
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"echo hi\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n",
            "data: [DONE]\n",
        )),
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":1,\"total_tokens\":10}}\n",
            "data: [DONE]\n",
        )),
    ])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let ws = env.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sess = state
        .store
        .create("test-model", &ws.display().to_string(), vec![]);

    let rx = client::stream_send(
        &sess.id,
        &json!({"message": "run it", "base_url": base_url}),
    )
    .await
    .unwrap();
    let events = collect_until_done(rx).await;
    let types: Vec<String> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        types,
        vec![
            "round_start",
            "tool_pending",
            "usage",
            "tool",
            "tool_result",
            "round_start",
            "content",
            "usage",
            "done",
        ],
        "{events:?}"
    );
    assert_eq!(events[3]["name"], "bash");
    assert_eq!(events[4]["text"], "hi");

    let got = state.store.get(&sess.id).unwrap();
    let roles: Vec<&str> = got.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
    assert_eq!(got.messages[2].tool_call_id, "call_a");
    assert_eq!(got.messages[2].content, "hi");
    assert_eq!(got.messages[3].content, "all done");
}

/// The pending-bash UX: the model calls a long bash command, the turn
/// parks (no further rounds), a mid-turn prompt is answered against a
/// placeholder result without stopping the command, and the real result
/// is recorded as THE result of the original tool call — inserted right
/// after its assistant tool_calls message, before the interjected
/// prompt — with a tool_result event that fills the original block.
#[tokio::test(flavor = "multi_thread")]
async fn pending_bash_answers_prompt_and_records_real_result() {
    let env = TestEnv::new("pending-bash");
    let base_url = spawn_mock_provider(vec![
        // Round 1: a long command; the turn parks on it.
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"sleep 1 && echo all green\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n",
            "data: [DONE]\n",
        )),
        // Round 2 (placeholder round after the mid-turn prompt): the
        // model answers the prompt without stopping the command.
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"started the test suite, should take a few seconds\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2,\"total_tokens\":13}}\n",
            "data: [DONE]\n",
        )),
        // Round 3: the command has exited; the model reports the result.
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"tests finished\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":1,\"total_tokens\":18}}\n",
            "data: [DONE]\n",
        )),
    ])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let ws = env.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sess = state
        .store
        .create("test-model", &ws.display().to_string(), vec![]);

    let rx = client::stream_send(
        &sess.id,
        &json!({"message": "run the tests", "base_url": base_url}),
    )
    .await
    .unwrap();

    // Land the prompt while the command is parked (~200ms into the 1s
    // sleep). The injected send is answered with the tiny injected
    // stream; the original stream keeps painting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut inject_rx = client::stream_send(&sess.id, &json!({"message": "hows it going"}))
        .await
        .expect("mid-turn prompt must be accepted, not conflict");
    let ack = tokio::time::timeout(Duration::from_secs(5), inject_rx.recv())
        .await
        .unwrap()
        .expect("injected stream open");
    assert_eq!(ack["type"], "injected");
    assert!(inject_rx.recv().await.is_none(), "injected stream closes");

    let events = collect_until_done(rx).await;
    let types: Vec<String> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    // tool_result (the real sleep result) arrives after the placeholder
    // round's reply, and the final answer follows it.
    assert_eq!(
        types,
        vec![
            "round_start",
            "tool_pending",
            "usage",
            "tool",
            "round_start",
            "content",
            "usage",
            "tool_result",
            "round_start",
            "content",
            "usage",
            "done",
        ],
        "{events:?}"
    );

    // History: the real tool result sits at its recorded position —
    // immediately after the assistant tool_calls message, BEFORE the
    // interjected prompt.
    let got = state.store.get(&sess.id).unwrap();
    let roles: Vec<&str> = got.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec![
            "user",
            "assistant",
            "tool",
            "user",
            "assistant",
            "assistant"
        ]
    );
    assert_eq!(got.messages[2].tool_call_id, "call_a");
    assert_eq!(
        got.messages[2].content, "all green",
        "tool result must be the real one, not the placeholder"
    );
    assert!(!got.messages[2].content.contains("still running"));
    assert_eq!(got.messages[3].content, "hows it going");
    assert_eq!(
        got.messages[4].content,
        "started the test suite, should take a few seconds"
    );
    assert_eq!(got.messages[5].content, "tests finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn quiet_parked_command_ends_turn_without_report_round() {
    let env = TestEnv::new("quiet-park");
    let base_url = spawn_mock_provider(vec![
        // Round 1: a long command with no output; the turn parks on it.
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"sleep 1\\\"}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n",
            "data: [DONE]\n",
        )),
        // Round 2 (placeholder round after the mid-turn prompt): the
        // model answers the prompt. The mock repeats this fixture for
        // any further request, so a report round after the quiet exit
        // would surface as a third round_start/content below.
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"it is running in the background\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2,\"total_tokens\":13}}\n",
            "data: [DONE]\n",
        )),
    ])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let ws = env.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sess = state
        .store
        .create("test-model", &ws.display().to_string(), vec![]);

    let rx = client::stream_send(
        &sess.id,
        &json!({"message": "start the long job", "base_url": base_url}),
    )
    .await
    .unwrap();

    // Land the prompt while the command is parked (~200ms into the 1s
    // sleep). The injected send is answered with the placeholder round.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut inject_rx = client::stream_send(&sess.id, &json!({"message": "hows it going"}))
        .await
        .expect("mid-turn prompt must be accepted, not conflict");
    let ack = tokio::time::timeout(Duration::from_secs(5), inject_rx.recv())
        .await
        .unwrap()
        .expect("injected stream open");
    assert_eq!(ack["type"], "injected");
    assert!(inject_rx.recv().await.is_none(), "injected stream closes");

    let events = collect_until_done(rx).await;
    let types: Vec<String> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    // The quiet exit (sleep 1, empty output) records its result and
    // ends the turn — no report round after the spoken answer.
    assert_eq!(
        types,
        vec![
            "round_start",
            "tool_pending",
            "usage",
            "tool",
            "round_start",
            "content",
            "usage",
            "tool_result",
            "done",
        ],
        "{events:?}"
    );

    let got = state.store.get(&sess.id).unwrap();
    let roles: Vec<&str> = got.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "user", "assistant"]);
    assert_eq!(
        got.messages[2].content, "",
        "quiet exit records an empty result"
    );
    assert_eq!(got.messages[3].content, "hows it going");
    assert_eq!(got.messages[4].content, "it is running in the background");
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_file_write_surfaces_approval_and_finishes_turn() {
    let env = TestEnv::new("write-approval");
    let ws = env.path().join("ws");
    let outside = env.path().join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("test.txt");
    let arguments = serde_json::to_string(&json!({
        "path": target.display().to_string(),
        "content": "blocked",
    }))
    .unwrap();
    let tool_chunk = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_write",
                    "type": "function",
                    "function": {"name": "write_file", "arguments": arguments},
                }]
            }
        }]
    });
    let base_url = spawn_mock_provider(vec![
        sse_response(&format!(
            "data: {tool_chunk}\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\ndata: [DONE]\n"
        )),
        sse_response(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"write denied\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
        )),
    ])
    .await;
    let state = spawn_server(&env, off_cfg()).await;
    let sess = state
        .store
        .create("test-model", &ws.display().to_string(), vec![]);

    let mut rx = client::stream_send(
        &sess.id,
        &json!({"message": "write outside", "base_url": base_url}),
    )
    .await
    .unwrap();
    let mut events = Vec::new();
    let approval = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("approval request timed out")
            .expect("send stream closed before approval");
        events.push(ev.clone());
        if ev["type"] == "approval_request" {
            break ev;
        }
    };
    client::respond_approval(&sess.id, approval["id"].as_str().unwrap(), "deny")
        .await
        .unwrap();
    events.extend(collect_until_done(rx).await);

    assert!(events.iter().any(|ev| {
        ev["type"] == "tool_result"
            && ev["text"]
                .as_str()
                .unwrap_or_default()
                .contains("write outside the workspace was not approved")
    }));
    assert!(events.iter().any(|ev| ev["type"] == "done"));
    assert!(!target.exists());
}

/// The sandbox approval bridge broadcasts approval_request on the session
/// channel and completes via POST /approval/:sid; unanswered prompts deny.
#[tokio::test(flavor = "multi_thread")]
async fn approval_flow_round_trip_and_timeout() {
    let env = TestEnv::new("approval");
    let state = spawn_server(&env, workspace_cfg()).await;

    let sess = state.store.create("m", "/tmp", vec![]);
    let sid = sess.id.clone();
    let mut sub = state.subs.subscribe(&sid);

    // Answered prompt: allow_once flows back through the route.
    let req = ApprovalRequest {
        session_id: sid.clone(),
        command: "curl https://example.com".into(),
        cwd: std::path::PathBuf::from("/tmp"),
        rule_id: "curl".into(),
        reason: "network tool".into(),
        accept_all_preview: None,
    };
    let waiter = {
        let approver = ServerApprover::new(state.clone(), sid.clone());
        tokio::spawn(async move { approver.decide(req).await })
    };
    let ev = tokio::time::timeout(Duration::from_secs(5), sub.rx.recv())
        .await
        .expect("approval_request event")
        .expect("channel open");
    assert_eq!(ev["type"], "approval_request");
    assert_eq!(ev["command"], "curl https://example.com");
    assert_eq!(ev["rule_id"], "curl");
    let approval_id = ev["id"].as_str().unwrap().to_string();

    client::respond_approval(&sid, &approval_id, "allow_once")
        .await
        .expect("route answers");
    let decision = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decision, Decision::AllowOnce);

    // Unanswered prompt with a short timeout denies.
    let short =
        ServerApprover::with_timeout(state.clone(), sid.clone(), Duration::from_millis(120));
    let denied = short
        .decide(ApprovalRequest {
            session_id: sid.clone(),
            command: "wget x".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            rule_id: "wget".into(),
            reason: "r".into(),
            accept_all_preview: None,
        })
        .await;
    assert_eq!(denied, Decision::DenyOnce);

    // Unknown approval ids 404.
    assert!(client::respond_approval(&sid, "doesnotexist0000", "deny")
        .await
        .is_err());
}

/// Dispatch spawn creates an owned child whose detached turn runs against
/// the mock provider; continue posts follow-ups; cancel pauses.
#[tokio::test(flavor = "multi_thread")]
async fn dispatch_spawn_continue_cancel() {
    let env = TestEnv::new("dispatch");
    let base_url = spawn_mock_provider(vec![sse_response(concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n",
        "data: [DONE]\n",
    ))])
    .await;
    let state = spawn_server(&env, off_cfg()).await;

    let ws = env.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let parent = state
        .store
        .create("test-model", &ws.display().to_string(), vec![]);
    let bridge = DispatchBridge::new(
        state.clone(),
        parent.id.clone(),
        atom_server::cancel::CancelToken::new(),
        String::new(),
        base_url.clone(),
        String::new(),
    );

    // Spawn.
    let res = bridge
        .spawn(atom_tools_dispatch_plan(
            "",
            "high",
            "investigate the flaky test",
        ))
        .await;
    let child_id = parse_id(&res);
    let spawned: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(spawned["status"], "queued");
    assert_eq!(spawned["provider"], "ollama-local");
    assert_eq!(state.store.get(&child_id).unwrap().provider, "ollama-local");

    // The detached child turn runs: user prompt + assistant reply.
    wait_for(|| async {
        state
            .store
            .get(&child_id)
            .map(|c| c.messages.len())
            .unwrap_or(0)
            >= 2
    })
    .await;

    // Busy guard while idle=false is timing-dependent; post-completion
    // continue appends another exchange.
    let cont = bridge
        .cont(atom_tools_dispatch_plan_with_sid(
            &child_id,
            "",
            "keep digging",
        ))
        .await;
    assert!(
        cont.starts_with("ok: continued\nid: "),
        "unexpected cont result: {cont}"
    );
    assert!(cont.contains("message_count: 3"), "{cont}");
    wait_for(|| async {
        state
            .store
            .get(&child_id)
            .map(|c| c.messages.len())
            .unwrap_or(0)
            >= 4
    })
    .await;

    // Cancel reports success.
    let cancelled = bridge.cancel(&child_id).await;
    assert_eq!(cancelled, format!("ok: cancelled\nid: {child_id}"));

    // Ownership: a sibling root session does not own the child.
    let outsider = state.store.create("m", "/tmp", vec![]);
    let other_bridge = DispatchBridge::new(
        state.clone(),
        outsider.id.clone(),
        atom_server::cancel::CancelToken::new(),
        String::new(),
        base_url.clone(),
        String::new(),
    );
    assert_eq!(
        other_bridge.cancel(&child_id).await,
        "error: not your subagent"
    );

    // Malformed id shape.
    assert_eq!(
        bridge.cancel("zzzz").await,
        "error: session_id must be a 16-character hex session id"
    );

    // Nested spawn is refused.
    let child_sess = state.store.get(&child_id).unwrap();
    let inner_bridge = DispatchBridge::new(
        state.clone(),
        child_sess.id.clone(),
        atom_server::cancel::CancelToken::new(),
        String::new(),
        base_url.clone(),
        String::new(),
    );
    assert_eq!(
        inner_bridge
            .spawn(atom_tools_dispatch_plan("", "low", "nested"))
            .await,
        "error: subagents cannot dispatch nested subagents"
    );
}

fn atom_tools_dispatch_plan(model: &str, thinking: &str, prompt: &str) -> atom_tools::DispatchPlan {
    atom_tools::DispatchPlan {
        model: model.into(),
        thinking: thinking.into(),
        prompt: prompt.into(),
        ..Default::default()
    }
}

fn atom_tools_dispatch_plan_with_sid(
    sid: &str,
    thinking: &str,
    prompt: &str,
) -> atom_tools::DispatchPlan {
    atom_tools::DispatchPlan {
        thinking: thinking.into(),
        prompt: prompt.into(),
        session_id: sid.into(),
        ..Default::default()
    }
}

fn parse_id(result: &str) -> String {
    atom_tools::parse_dispatch_session_id(result)
}

async fn wait_for<F, Fut>(mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if pred().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not reached before deadline");
}

/// Subscribing to /events yields the subscribed sentinel; closing the
/// subscription as the last viewer cancels the session's active turns.
#[tokio::test(flavor = "multi_thread")]
async fn events_stream_and_last_viewer_cancels_turns() {
    let env = TestEnv::new("events");
    let state = spawn_server(&env, off_cfg()).await;
    let sess = state.store.create("m", "/tmp", vec![]);
    let sid = sess.id.clone();

    let rx = client::stream_events(&sid).await.unwrap();
    let mut rx = rx;
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["type"], "subscribed");

    // Broadcast reaches the subscriber.
    state.subs.broadcast(&sid, &json!({"type": "saved"}));
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ev["type"], "saved");

    // A live turn is cancelled once the last subscriber disconnects.
    let handle = state.turns.start_turn(&sid, "t1");
    drop(rx);
    let mut cancelled = false;
    for _ in 0..100 {
        if handle.is_cancelled() {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cancelled, "last subscriber leaving must cancel the turn");
}

/// A mid-turn prompt is injected into the live turn instead of
/// conflicting: /send is accepted, answers with a tiny injected stream,
/// cancels only the provider round, and queues the prompt on the turn
/// handle. The pause route (Esc) remains the hard stop, and a send to
/// an idle session starts a normal turn.
#[tokio::test(flavor = "multi_thread")]
async fn send_mid_turn_injects_into_live_turn() {
    let env = TestEnv::new("send-inject");
    let state = spawn_server(&env, off_cfg()).await;
    let sess = state.store.create("m", "/tmp", vec![]);
    let sid = sess.id.clone();

    // A live turn with a registered provider round. /send must hand the
    // prompt to it: no pause, no 409, and only the round is cancelled.
    let handle = state.turns.start_turn(&sid, "live");
    let round = atom_server::cancel::CancelToken::new();
    handle.set_round_cancel(Some(round.clone()));
    {
        let handle = handle.clone();
        let state = state.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            handle.cancel_token().cancelled().await;
            state.turns.end_turn(&sid, &handle);
        });
    }

    // The send is accepted: a tiny injected stream, then close.
    let mut rx = client::stream_send(&sid, &json!({"message": "hello", "turn_id": "t2"}))
        .await
        .expect("mid-turn send must be accepted, not conflict");
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .expect("injected stream open");
    assert_eq!(first["type"], "injected");
    assert!(rx.recv().await.is_none(), "injected stream closes");

    // Only the provider round was cancelled; the turn lives on and the
    // prompt is queued for the next round.
    assert!(round.is_cancelled());
    assert!(!handle.is_cancelled());
    let pending = handle.take_pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content, "hello");

    // Esc (the pause route) remains the hard stop; once the turn is
    // gone, a send starts a normal turn again.
    client::post(
        &format!("/api/sessions/{sid}/pause"),
        &json!({"turn_id": ""}),
    )
    .await
    .expect("pause must succeed");
    assert!(handle.is_cancelled());
    assert!(!state.turns.session_has_active_turn(&sid));

    let mut rx = client::stream_send(&sid, &json!({"message": "again", "turn_id": "t3"}))
        .await
        .expect("send must be accepted once the session is idle");
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .expect("stream open");
    assert_eq!(first["type"], "round_start");
}
