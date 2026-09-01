//! HTTP server over a Unix socket, ported from server.go runServer and
//! its handlers. Routes are byte-for-byte the Go ones; /send and /events
//! stream NDJSON (one JSON object per line). A new POST /approval/:sid
//! route completes sandbox approval prompts.

use crate::cancel::CancelToken;
use crate::instructions::load_instructions_from;
use crate::state::{unlink_socket_if_no_listener, AppState, ConnGuard};
use crate::turn::{self, EventOut, TurnOpts};
use atom_core::session::compaction::{
    compact_session, compact_span, compaction_target, is_nothing_to_compact,
};
use atom_core::session::store::{
    data_dir, new_session_id, socket_path, DelegateStatus, Session, SessionStore,
};
use atom_core::types::{ImageData, MAX_IMAGE_BASE64_BYTES};
use atom_sandbox::approvals::Decision;
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

/// How long POST /pause waits for the paused turn to fully unwind before
/// returning, so a follow-up /send can't race end_turn.
const PAUSE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

type Resp = Response<BoxBody<Bytes, Infallible>>;

fn full_body(v: Value) -> Resp {
    // writeJSON appends a newline like Go's json.Encoder.
    let mut s = serde_json::to_string(&v).unwrap_or_default();
    s.push('\n');
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(s)).boxed())
        .unwrap()
}

fn error_resp(status: StatusCode, msg: &str) -> Resp {
    // http.Error writes "<msg>\n" as text/plain.
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .body(Full::new(Bytes::from(format!("{msg}\n"))).boxed())
        .unwrap()
}

fn no_content() -> Resp {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()).boxed())
        .unwrap()
}

/// Wraps a tokio mpsc channel into an HTTP body.
fn receiver_to_body(
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, Infallible>>,
) -> BoxBody<Bytes, Infallible> {
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    BoxBody::new(StreamBody::new(stream.map(|item| item.map(Frame::data))))
}

/// NDJSON streaming response paired with its sender.
fn ndjson_response() -> (Resp, tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(16);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(receiver_to_body(rx))
        .unwrap();
    (resp, tx)
}

// ---------------------------------------------------------------------------
// Request body decoding.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct CreateBody {
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    thinking: String,
}

#[derive(serde::Deserialize, Default)]
struct SendBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    key: String,
    #[serde(rename = "base_url", default)]
    base_url: String,
    #[serde(rename = "reasoning_field", default)]
    reasoning_field: String,
    #[serde(rename = "turn_id", default)]
    turn_id: String,
    #[serde(default)]
    images: Vec<ImageData>,
    #[serde(default)]
    compact: bool,
    #[serde(rename = "compact_instructions", default)]
    compact_instructions: String,
}

#[derive(serde::Deserialize, Default)]
struct PatchBody {
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    cwd: String,
}

#[derive(serde::Deserialize, Default)]
struct PauseBody {
    #[serde(rename = "turn_id", default)]
    turn_id: String,
}

#[derive(serde::Deserialize, Default)]
struct CompactBody {
    #[serde(default)]
    instructions: String,
}

#[derive(serde::Deserialize, Default)]
struct ForkBody {
    /// Position (0-based index) of the user message in the source
    /// session's `messages` array; the new session's transcript is
    /// truncated to `messages[..position]` (exclusive of position
    /// itself) and `messages[position].content` is returned as the
    /// draft. `None` means "fork from latest": copy the full transcript
    /// and return an empty draft.
    #[serde(default)]
    position: Option<i64>,
}

#[derive(serde::Deserialize, Default)]
struct ApprovalBody {
    #[serde(default)]
    id: String,
    #[serde(default)]
    decision: String,
}

async fn decode<T: serde::de::DeserializeOwned>(req: &mut Request<Incoming>) -> anyhow::Result<T> {
    let bytes = BodyExt::collect(req.body_mut())
        .await
        .map_err(|e| anyhow::anyhow!("read body: {e}"))?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

// ---------------------------------------------------------------------------
// Router.
// ---------------------------------------------------------------------------

pub async fn handle(state: Arc<AppState>, mut req: Request<Incoming>) -> Result<Resp, Infallible> {
    // connTracker middleware: every request counts as an active
    // connection while in flight (streaming handlers hold the guard in
    // their worker task for the stream's lifetime).
    let guard = ConnGuard::take(&state.tracker);
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    Ok(route(&state, &method, &path, &mut req, guard).await)
}

async fn route(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    req: &mut Request<Incoming>,
    guard: ConnGuard,
) -> Resp {
    match path {
        "/api/sessions" => return sessions_index(state, method, req, guard).await,
        "/api/stats" => {
            if method != "GET" {
                return error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            }
            // days=0 or absent means all time.
            let days = req
                .uri()
                .query()
                .and_then(|q| {
                    q.split('&').find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        (k == "days").then(|| v.parse::<i64>().unwrap_or(0))
                    })
                })
                .unwrap_or(0);
            drop(guard);
            let report = state
                .store_call(move |store| atom_core::session::stats::aggregate_stats(store, days))
                .await;
            return full_body(serde_json::to_value(report).unwrap_or(Value::Null));
        }
        "/api/keepalive" => {
            if method != "GET" {
                return error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            }
            return keepalive(guard);
        }
        "/api/capabilities" => {
            if method != "GET" {
                return error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            }
            drop(guard);
            // Feature flags so a newer client can detect a stale
            // background server and restart it. `version` lets a client
            // also reject a server built from a different release, and
            // `build` extends that to rebuilds of the same version (a
            // dev server built before an edit, a re-install without a
            // version bump).
            return full_body(json!({
                "compact": true, "dispatch": true, "mcp": true,
                "skills": true, "keepalive": true,
                "version": env!("CARGO_PKG_VERSION"),
                "build": atom_core::build::build_id(),
            }));
        }
        _ => {}
    }

    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        let id = parts[0];
        if id.is_empty() {
            return error_resp(StatusCode::BAD_REQUEST, "missing session id");
        }
        if parts.len() == 2 {
            return match parts[1] {
                "send" => {
                    if method != "POST" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        handle_send(state, req, id, guard).await
                    }
                }
                "events" => {
                    if method != "GET" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        handle_events(state, id, guard)
                    }
                }
                "pause" => {
                    if method != "POST" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        let r = handle_pause(state, req, id).await;
                        drop(guard);
                        r
                    }
                }
                "compact" => {
                    if method != "POST" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        let r = handle_compact(state, req, id).await;
                        drop(guard);
                        r
                    }
                }
                "fork" => {
                    if method != "POST" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        let r = handle_fork(state, req, id).await;
                        drop(guard);
                        r
                    }
                }
                "children" => {
                    if method != "GET" {
                        error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    } else {
                        drop(guard);
                        handle_session_children(state, id)
                    }
                }
                _ => error_resp(StatusCode::NOT_FOUND, "not found"),
            };
        }
        let r = session_item(state, method, req, id).await;
        drop(guard);
        return r;
    }

    if let Some(sid) = path.strip_prefix("/approval/") {
        if method != "POST" {
            return error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let r = handle_approval(state, req, sid).await;
        drop(guard);
        return r;
    }

    drop(guard);
    error_resp(StatusCode::NOT_FOUND, "404 page not found")
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// POST /api/sessions — create; GET /api/sessions — list.
async fn sessions_index(
    state: &Arc<AppState>,
    method: &str,
    req: &mut Request<Incoming>,
    guard: ConnGuard,
) -> Resp {
    let r = match method {
        "POST" => {
            let body: CreateBody = decode(req).await.unwrap_or_default();
            let cwd = if body.cwd.is_empty() {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            } else {
                body.cwd.clone()
            };
            if !std::path::Path::new(&cwd).is_absolute() {
                return error_resp(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
            }
            let model = body.model.clone();
            let provider = body.provider.clone();
            let thinking = body.thinking.clone();
            let sess = state
                .store_call(move |store| {
                    let instructions = load_instructions_from(&cwd);
                    let mut sess = store.create(&model, &cwd, instructions);
                    if !provider.is_empty() {
                        store.update_provider(&sess.id, &provider);
                        sess.provider = provider;
                    }
                    if !thinking.is_empty() {
                        store.update_thinking(&sess.id, &thinking);
                        sess.thinking = thinking;
                    }
                    sess
                })
                .await;
            full_body(serde_json::to_value(sess.info()).unwrap_or(Value::Null))
        }
        "GET" => {
            // Skip unstarted sessions (created but never messaged) so the
            // session picker only lists conversations.
            let infos: Vec<Value> = state
                .store
                .list_info()
                .iter()
                .filter(|info| info.message_count > 0)
                .map(|info| serde_json::to_value(info).unwrap_or(Value::Null))
                .collect();
            full_body(Value::Array(infos))
        }
        _ => error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    };
    drop(guard);
    r
}

/// GET/PATCH/DELETE /api/sessions/{id}.
async fn session_item(
    state: &Arc<AppState>,
    method: &str,
    req: &mut Request<Incoming>,
    id: &str,
) -> Resp {
    if state.store.get_info(id).is_none() {
        return error_resp(StatusCode::NOT_FOUND, "session not found");
    }
    match method {
        "GET" => {
            let id = id.to_string();
            let Some(sess) = state.store_call(move |store| store.get(&id)).await else {
                return error_resp(StatusCode::NOT_FOUND, "session not found");
            };
            full_body(serde_json::to_value(&sess).unwrap_or(Value::Null))
        }
        "PATCH" => {
            // Change the model, thinking level, or working directory the
            // session uses. The conversation history stays; only future
            // turns change.
            let body: PatchBody = decode(req).await.unwrap_or_default();
            if body.model.is_empty()
                && body.provider.is_empty()
                && body.thinking.is_none()
                && body.cwd.is_empty()
            {
                return error_resp(
                    StatusCode::BAD_REQUEST,
                    "model, provider, thinking, or cwd is required",
                );
            }
            if !body.cwd.is_empty() && !std::path::Path::new(&body.cwd).is_absolute() {
                return error_resp(StatusCode::BAD_REQUEST, "cwd must be an absolute path");
            }
            let patch_id = id.to_string();
            state
                .store_call(move |store| {
                    if !body.model.is_empty() {
                        store.update_model(&patch_id, &body.model);
                    }
                    if !body.provider.is_empty() {
                        store.update_provider(&patch_id, &body.provider);
                    }
                    if let Some(t) = &body.thinking {
                        store.update_thinking(&patch_id, t);
                    }
                    if !body.cwd.is_empty() {
                        store.update_cwd(&patch_id, &body.cwd);
                    }
                })
                .await;
            // Tell other instances viewing this session to reload so they
            // show the new model too.
            state.subs.broadcast(id, &json!({"type": "saved"}));
            no_content()
        }
        "DELETE" => {
            let delete_id = id.to_string();
            state
                .store_call(move |store| store.delete(&delete_id))
                .await;
            state.remove_file_seen(id);
            no_content()
        }
        _ => error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

/// POST /api/sessions/{id}/pause.
async fn handle_pause(state: &Arc<AppState>, req: &mut Request<Incoming>, id: &str) -> Resp {
    let body: PauseBody = decode(req).await.unwrap_or_default();
    // External pause routes (the TUI's Esc) are user-initiated by
    // construction: dispatch pauses run in-process and never use them.
    // Mark live child turns so the turn loop records a user stop, not an
    // error or a silent "done". No active turn means the mark is withheld
    // so a stale flag cannot misclassify a later dispatch turn.
    if !state
        .store
        .get_info(id)
        .map(|info| info.parent_id.is_empty())
        .unwrap_or(true)
        && state.turns.session_has_active_turn(id)
    {
        state.mark_user_stop(id);
    }
    state.turns.pause_session(id, &body.turn_id);
    state.subs.broadcast(id, &json!({"type": "paused"}));
    if state.turns.wait_idle(id, PAUSE_WAIT_TIMEOUT).await {
        no_content()
    } else {
        error_resp(
            StatusCode::GATEWAY_TIMEOUT,
            "turn did not stop within the pause timeout",
        )
    }
}

/// POST /approval/{session_id} {id, decision} completes a pending
/// sandbox approval prompt.
async fn handle_approval(state: &Arc<AppState>, req: &mut Request<Incoming>, sid: &str) -> Resp {
    let body: ApprovalBody = match decode(req).await {
        Ok(b) => b,
        Err(e) => return error_resp(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")),
    };
    let decision = match body.decision.as_str() {
        "allow_once" => Decision::AllowOnce,
        "allow_session" => Decision::AllowOnce,
        "allow_global" | "allow_always" | "allow_all" => Decision::AllowAll,
        "deny" | "deny_once" => Decision::DenyOnce,
        "deny_always" | "deny_all" => Decision::DenyAll,
        _ => return error_resp(StatusCode::BAD_REQUEST, "invalid decision"),
    };
    if state.approvals.complete(sid, &body.id, decision) {
        no_content()
    } else {
        error_resp(StatusCode::NOT_FOUND, "approval not found")
    }
}

/// GET /api/sessions/{id}/children lists the parent's dispatched
/// subagents that have not been explicitly killed (dispatch action=cancel).
/// A subagent stays listed even after its turn finishes; only an explicit
/// kill drops it from the list. Missing parent yields 404; no children is
/// an empty array.
fn handle_session_children(state: &Arc<AppState>, id: &str) -> Resp {
    if state.store.get_info(id).is_none() {
        return error_resp(StatusCode::NOT_FOUND, "session not found");
    }
    let infos: Vec<Value> = state
        .store
        .children_info(id)
        .iter()
        .filter(|child| !child.cancelled)
        .map(|child| serde_json::to_value(child).unwrap_or(Value::Null))
        .collect();
    full_body(Value::Array(infos))
}

/// GET /api/keepalive holds the request until the client disconnects so
/// connTracker counts a live TUI as an active connection.
fn keepalive(guard: ConnGuard) -> Resp {
    let (resp, tx) = plain_hold_response();
    tokio::spawn(async move {
        let _guard = guard;
        let tx = tx;
        // Flush the headers with one empty frame (Go's explicit Flush),
        // then hold until the client goes away.
        if tx.send(Ok(Bytes::new())).await.is_err() {
            return;
        }
        let _ = tx.closed().await;
    });
    resp
}

fn plain_hold_response() -> (Resp, tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(4);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(receiver_to_body(rx))
        .unwrap();
    (resp, tx)
}

/// GET /api/sessions/{id}/events streams NDJSON events to a subscriber in
/// real time. The connection stays open until the client disconnects.
fn handle_events(state: &Arc<AppState>, id: &str, guard: ConnGuard) -> Resp {
    // If the session doesn't exist, no events will come; the client will
    // handle reconnection.
    let (resp, tx) = ndjson_response();
    let sub = state.subs.subscribe(id);
    let state = state.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        let _guard = guard;
        let mut rx = sub.rx;
        let tx = tx;

        // Send an initial "subscribed" event so the client knows it's
        // connected.
        let hello = json!({"type": "subscribed"});
        let mut line = serde_json::to_string(&hello).unwrap_or_default();
        line.push('\n');
        if tx.send(Ok(Bytes::from(line))).await.is_err() {
            state.subs.unsubscribe(&id, sub.id);
            return;
        }

        // Replay any sandbox approval prompt already waiting on this
        // session: broadcasts are not replayed, so a viewer that arrives
        // while a request is pending (e.g. navigating into a subagent
        // stuck on the approval gate) would otherwise see a frozen
        // transcript with no way to answer.
        for (approval_id, req) in state.approvals.pending(&id) {
            let ev = crate::dispatch::approval_request_event(&state, &id, &approval_id, &req);
            let mut line = serde_json::to_string(&ev).unwrap_or_default();
            line.push('\n');
            if tx.send(Ok(Bytes::from(line))).await.is_err() {
                state.subs.unsubscribe(&id, sub.id);
                return;
            }
        }
        // A parent view also replays its subagents' pending prompts, so
        // the user can answer a child's request from the parent even
        // when the TUI connected after the original broadcast.
        for child in state.store.children_info(&id) {
            for (approval_id, req) in state.approvals.pending(&child.id) {
                let ev =
                    crate::dispatch::approval_request_event(&state, &child.id, &approval_id, &req);
                let mut line = serde_json::to_string(&ev).unwrap_or_default();
                line.push('\n');
                if tx.send(Ok(Bytes::from(line))).await.is_err() {
                    state.subs.unsubscribe(&id, sub.id);
                    return;
                }
            }
        }

        // Stream events to the client until it disconnects.
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = tx.closed() => None,
            };
            let Some(ev) = ev else { break };
            let mut line = serde_json::to_string(&ev).unwrap_or_default();
            line.push('\n');
            if tx.send(Ok(Bytes::from(line))).await.is_err() {
                break;
            }
        }
        // When the last subscriber leaves, any active turn for the session
        // is cancelled so generation doesn't keep running with no client
        // listening. Detached dispatch children are exempt: their turns
        // are owned by the parent session and deliberately run without a
        // viewer (EventOut::Discard), so peeking at one and navigating
        // away must not kill it mid-approval or mid-turn.
        let is_dispatch_child = state
            .store
            .get_info(&id)
            .is_some_and(|session| !session.parent_id.is_empty());
        if !is_dispatch_child && state.subs.unsubscribe(&id, sub.id) {
            state.turns.cancel_session_turns(&id);
        }
    });
    resp
}

/// POST /api/sessions/{id}/send processes one chat turn, streaming NDJSON
/// events back while the model answers and tools run.
async fn handle_send(
    state: &Arc<AppState>,
    req: &mut Request<Incoming>,
    id: &str,
    guard: ConnGuard,
) -> Resp {
    let load_id = id.to_string();
    let Some(mut sess) = state.store_call(move |store| store.get(&load_id)).await else {
        return error_resp(StatusCode::NOT_FOUND, "session not found");
    };
    if !sess.parent_id.is_empty() {
        return error_resp(
            StatusCode::CONFLICT,
            "subagent sessions are managed by their parent",
        );
    }
    let body: SendBody = match decode(req).await {
        Ok(b) => b,
        Err(e) => return error_resp(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")),
    };

    // Sanity-check attached images before the turn starts: sizes are
    // capped so the request stays reasonable, and a MIME type is required
    // to build the provider's data URL.
    for (i, img) in body.images.iter().enumerate() {
        if img.mime.is_empty() {
            return error_resp(
                StatusCode::BAD_REQUEST,
                &format!("images[{i}]: missing mime type"),
            );
        }
        if img.data.len() > MAX_IMAGE_BASE64_BYTES {
            return error_resp(
                StatusCode::BAD_REQUEST,
                &format!(
                    "images[{i}]: larger than the {}-byte base64 limit",
                    MAX_IMAGE_BASE64_BYTES
                ),
            );
        }
    }

    // Resolve the API key: use what the client sent, then fall back to
    // the same sources the client uses.
    let mut key = body.key.clone();
    if key.is_empty() {
        key = std::env::var("OLLAMA_API_KEY").unwrap_or_default();
    }
    if key.is_empty() {
        key = atom_core::providers::auth::load_provider_key("ollama-cloud").await;
    }

    // Resolve the base URL: use what the client sent, else derive from
    // whether we have a key.
    let mut base_url = body.base_url.clone();
    if base_url.is_empty() {
        base_url = if key.is_empty() {
            "http://localhost:11434/v1".into()
        } else {
            "https://ollama.com/v1".into()
        };
    }
    let base_url = base_url.trim_end_matches('/').to_string();

    if !state.turns.try_prepare_session_turn(id) {
        // A turn is already active. The prompt must neither pause the
        // turn nor bounce with a 409: hand it to the live turn (queued
        // on its handle, current provider round cancelled so the model
        // sees it next round) and acknowledge with a tiny stream.
        let msg = atom_core::types::Message {
            role: "user".into(),
            content: body.message.clone(),
            images: body.images.clone(),
            created_at: Some(Utc::now()),
            ..Default::default()
        };
        if state.turns.inject_session_message(id, msg) {
            // Prompt acceleration: in-flight bash calls in this session
            // stop waiting for inline results and hand back their job
            // ids immediately. The registry is the coordination point —
            // no token is threaded through the turn handle.
            atom_sandbox::jobs::flush_wait(id);
            let (resp, tx) = ndjson_response();
            tokio::spawn(async move {
                // Drop tx at the end of the block so the body closes.
                let mut line =
                    serde_json::to_string(&json!({"type": "injected"})).unwrap_or_default();
                line.push('\n');
                let _ = tx.send(Ok(Bytes::from(line))).await;
            });
            return resp;
        }
        // The active turn ended between the check and the injection:
        // take the slow path and start a normal turn.
        if !state.turns.try_prepare_session_turn(id) {
            return error_resp(
                StatusCode::CONFLICT,
                "session already has an active turn; send again in a moment",
            );
        }
    }

    let (resp, tx) = ndjson_response();

    // The watcher needs a sender to detect receiver closure, but must drop
    // that sender when the turn finishes so the response body can end.
    let disconnect = CancelToken::new();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    {
        let tx_watch = tx.clone();
        let dc = disconnect.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tx_watch.closed() => dc.cancel(),
                _ = done_rx => {}
            }
        });
    }

    let state = state.clone();
    let id = id.to_string();
    let opts = TurnOpts {
        message: body.message,
        thinking: body.thinking,
        key,
        base_url,
        reasoning_field: body.reasoning_field,
        turn_id: body.turn_id,
        images: body.images,
        compact: body.compact,
        compact_instructions: body.compact_instructions,
        skip_append: false,
    };
    tokio::spawn(async move {
        let _guard = guard;
        let out = EventOut::Response(tx);
        turn::run_session_turn_guarded(&state, &mut sess, &id, opts, out, disconnect).await;
        let _ = done_tx.send(());
    });
    resp
}

/// POST /api/sessions/{id}/compact folds the session on demand. Unlike
/// the auto path it ignores the token threshold. Optional JSON field
/// "instructions" is forwarded to the summarizer as extra focus.
async fn handle_compact(state: &Arc<AppState>, req: &mut Request<Incoming>, id: &str) -> Resp {
    let load_id = id.to_string();
    let Some(mut sess) = state.store_call(move |store| store.get(&load_id)).await else {
        return error_resp(StatusCode::NOT_FOUND, "session not found");
    };
    if !sess.parent_id.is_empty() {
        return error_resp(
            StatusCode::CONFLICT,
            "subagent sessions are managed by their parent",
        );
    }
    let body: CompactBody = decode(req).await.unwrap_or_default();

    // Mid-turn: interrupt the current model request so handleSend can
    // fold and resume. The TUI watches compaction events on /send.
    if state.turns.request_session_compact(id, &body.instructions) {
        return no_content();
    }

    if compact_span(&sess).is_none() {
        return error_resp(StatusCode::BAD_REQUEST, "nothing to compact");
    }

    let target = compaction_target().await;
    if let Err(err) = compact_session(
        &mut sess,
        None,
        &target.base_url,
        &target.key,
        &target.model,
        &body.instructions,
    )
    .await
    {
        if is_nothing_to_compact(&err) {
            return error_resp(StatusCode::BAD_REQUEST, "nothing to compact");
        }
        return error_resp(StatusCode::BAD_GATEWAY, &err.to_string());
    }
    turn::persist_session(state, &sess, id).await;
    full_body(json!({
        "summary": sess.compaction_summary,
        "compacted_through": sess.compacted_through,
    }))
}

/// POST /api/sessions/{id}/fork — create a child session whose
/// transcript is the source session up to (and excluding) the chosen
/// user message, plus a draft pre-filled with that message's content.
/// `body.position` is the index in `source.messages`; passing `None`
/// forks from latest (full transcript, empty draft). Subagent sessions
/// cannot be forked.
async fn handle_fork(state: &Arc<AppState>, req: &mut Request<Incoming>, id: &str) -> Resp {
    let load_id = id.to_string();
    let Some(source) = state.store_call(move |store| store.get(&load_id)).await else {
        return error_resp(StatusCode::NOT_FOUND, "session not found");
    };
    if !source.parent_id.is_empty() {
        return error_resp(
            StatusCode::CONFLICT,
            "subagent sessions are managed by their parent",
        );
    }
    let body: ForkBody = decode(req).await.unwrap_or_default();

    // Truncate the transcript up to (but excluding) the picked message.
    // `position = n` means: copy messages[0..n] into the child, and use
    // messages[n] as the draft. `position = None` is "fork from latest"
    // (no draft, copy everything).
    let (truncated, draft) = match body.position {
        None => (source.messages.clone(), String::new()),
        Some(pos) => {
            if pos < 0 || (pos as usize) >= source.messages.len() {
                return error_resp(
                    StatusCode::BAD_REQUEST,
                    "position is out of range for the source session",
                );
            }
            let picked = &source.messages[pos as usize];
            if picked.role != "user" {
                return error_resp(
                    StatusCode::BAD_REQUEST,
                    "position must point to a user message",
                );
            }
            let draft = picked.content.clone();
            let truncated = source.messages[..pos as usize].to_vec();
            (truncated, draft)
        }
    };

    // Build the child session. Same model/provider/cwd/thinking as the
    // source. parent_id stays empty: forks are independent root sessions,
    // not subagents. The codebase overloads parent_id to mean "dispatched
    // subagent" (turn.rs broadcasts "children" events to it, the TUI hides
    // the prompt via read_only_view(), /fork and /compact gate on
    // parent_id.is_empty()), so a fork child with parent_id set would be
    // treated as a subagent and the user couldn't type into it.
    // Lineage is conveyed by the `(fork #N)` title suffix instead.
    let now = Utc::now();
    let mut child = Session {
        id: new_session_id(),
        title: format!("{} (fork #1)", source.title),
        title_generated: source.title_generated,
        messages: truncated,
        model: source.model.clone(),
        provider: source.provider.clone(),
        cwd: source.cwd.clone(),
        instructions: source.instructions.clone(),
        usage: None,
        compaction_summary: source.compaction_summary.clone(),
        compacted_through: source.compacted_through,
        parent_id: String::new(),
        thinking: source.thinking.clone(),
        cancelled: false,
        status: DelegateStatus::Done,
        batch_id: String::new(),
        batch_index: 0,
        created_at: now,
        updated_at: now,
    };
    // Stamp every newly-copied message so /fork timestamps in the
    // *child* show when it was forked, not the original write time.
    for message in &mut child.messages {
        message.created_at = Some(now);
    }
    // Persist + index.
    let child_id = child.id.clone();
    state
        .store_call(move |store| {
            store.save_with_index(&child);
        })
        .await;

    full_body(json!({
        "info": serde_json::to_value(state.store.get_info(&child_id)).unwrap_or(Value::Null),
        "draft": draft,
    }))
}

// ---------------------------------------------------------------------------
// Listener + server bootstrap.
// ---------------------------------------------------------------------------

/// listenOnSocket creates the Unix socket listener. Returns None when
/// another live server is already listening on the path, so the caller
/// exits cleanly. A stale socket (from a crashed server) is removed and
/// the bind retried once. Binding before removing anything makes it
/// impossible for a starter to unlink a live server's socket: the bind
/// only succeeds when the path is actually free, and when it fails the
/// live server is detected by dialing the path.
pub fn listen_on_socket(path: &std::path::Path) -> std::io::Result<Option<UnixListener>> {
    use std::os::unix::net::UnixStream as StdUnixStream;
    let mut attempt = 0;
    loop {
        match UnixListener::bind(path) {
            Ok(l) => return Ok(Some(l)),
            Err(err) => {
                // The bind failed. If a live server answers on the path,
                // defer to it instead of disturbing its socket.
                if StdUnixStream::connect(path).is_ok() {
                    return Ok(None);
                }
                // Stale socket from a crashed server: remove it and retry once.
                if attempt > 0 {
                    return Err(err);
                }
                let _ = std::fs::remove_file(path);
            }
        }
        attempt += 1;
    }
}

/// Serves one accepted HTTP/1 connection over the local Unix socket.
async fn serve_conn(state: Arc<AppState>, stream: UnixStream) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let service = service_fn(move |req| {
        let state = state.clone();
        async move { handle(state, req).await }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await;
}

/// Accept loop shared by run_server and tests.
pub async fn serve_listener(listener: UnixListener, state: Arc<AppState>) {
    while let Ok((stream, _)) = listener.accept().await {
        let state = state.clone();
        tokio::spawn(serve_conn(state, stream));
    }
}

/// Idle monitor: exits the server once it has had zero connections for
/// the full idle window. Sessions are persisted to disk on every update,
/// so exiting is safe; the next client run starts a fresh server. The
/// socket file is only removed when no other server answers on the path:
/// a racing server (from a previous bug) may own it, and unlinking it
/// would strand that server's listeners.
pub async fn idle_monitor(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if !state.tracker.idle_expired() {
            continue;
        }
        eprintln!(
            "atom server idle for {:?} with no connections, shutting down",
            state.tracker.idle_after
        );
        unlink_socket_if_no_listener(&socket_path());
        let _ = std::fs::remove_file(data_dir().join("server.pid"));
        atom_tools::close_all_mcp();
        std::process::exit(0);
    }
}

/// Writes the pid file so the server can be found and managed.
pub fn write_pid_file() {
    let _ = std::fs::write(
        data_dir().join("server.pid"),
        format!("{}", std::process::id()),
    );
}

/// Graceful shutdown on SIGTERM or SIGINT: remove the socket and pid
/// file, close MCP connections, exit.
async fn signal_shutdown(socket: PathBuf) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(data_dir().join("server.pid"));
    atom_tools::close_all_mcp();
    std::process::exit(0);
}

/// runServer starts the atom session server. It returns when another
/// live server is detected (clean exit) or on a listener error.
pub async fn run_server() -> anyhow::Result<()> {
    // If a server is already running, exit cleanly. This handles the race
    // where two clients try to start a server at the same time.
    if UnixStream::connect(socket_path()).await.is_ok() {
        return Ok(());
    }

    let store = SessionStore::open().map_err(|e| anyhow::anyhow!("session store: {e}"))?;
    let state = Arc::new(AppState::new(
        Arc::new(store),
        atom_sandbox::policy::SandboxConfig::load(),
        Arc::new(crate::state::ConnTracker::default()),
    ));

    let listener =
        match listen_on_socket(&socket_path()).map_err(|e| anyhow::anyhow!("listen: {e}"))? {
            Some(l) => l,
            None => return Ok(()), // another server is already running
        };
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    let _ = std::fs::set_permissions(socket_path(), perms);

    write_pid_file();

    // Start the idle countdown now; the first connection pauses it.
    state.tracker.start_idle_clock();
    tokio::spawn(idle_monitor(state.clone()));
    tokio::spawn(signal_shutdown(socket_path()));

    eprintln!("atom server listening on {}", socket_path().display());
    serve_listener(listener, state).await;
    atom_tools::close_all_mcp();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// listenOnSocket must detect a live server on the path and defer to
    /// it without touching its socket file.
    #[tokio::test]
    async fn listen_live_defers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atom.sock");
        let l = listen_on_socket(&path).unwrap().expect("first bind");
        let l2 = listen_on_socket(&path).unwrap();
        assert!(l2.is_none(), "want None when a live server owns the path");
        assert!(path.exists(), "live server's socket file was removed");
        drop(l);
    }

    /// listenOnSocket must recover from a stale socket file left by a
    /// crashed server: remove it, retry the bind, and accept connections.
    #[tokio::test]
    async fn listen_stale_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atom.sock");
        // Simulate the leftover path entry of a crashed server (Go unlinks
        // real unix sockets on Close, so a regular file stands in for the
        // stale inode that blocks the bind).
        std::fs::write(&path, b"stale").unwrap();
        let l = listen_on_socket(&path)
            .unwrap()
            .expect("stale socket should be replaced");
        std::os::unix::net::UnixStream::connect(&path).expect("fresh socket not accepting");
        drop(l);
    }

    /// /children must keep listing subagents after their turn finishes,
    /// dropping only the ones the parent explicitly killed.
    #[tokio::test]
    async fn session_children_lists_alive_subagents_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::open_in_dir(dir.path()).unwrap());
        let state = Arc::new(AppState::new(
            store.clone(),
            atom_sandbox::policy::SandboxConfig::default(),
            Arc::new(crate::state::ConnTracker::default()),
        ));
        let parent = store.create("m", "/tmp", vec![]);
        let alive = store.create_child(&parent.id, "m", "/tmp", "low", "alive", vec![]);
        let idle = store.create_child(&parent.id, "m", "/tmp", "low", "idle", vec![]);
        let killed = store.create_child(&parent.id, "m", "/tmp", "low", "killed", vec![]);
        store.set_cancelled(&killed.id, true);

        let resp = handle_session_children(&state, &parent.id);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        let listed: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["id"].as_str().map(|s| s.to_string()))
            .collect();
        assert_eq!(listed.len(), 2, "only non-killed subagents are listed");
        assert!(
            listed.contains(&alive.id),
            "alive subagent must stay listed"
        );
        assert!(listed.contains(&idle.id), "idle subagent must stay listed");
        assert!(
            !listed.contains(&killed.id),
            "explicitly killed subagent must be hidden"
        );
    }
}
