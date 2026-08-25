//! The chat turn loop, ported from server.go runSessionTurn and its
//! helpers: toolCallAccumulator (both the OpenAI index-delta shape and
//! the Ollama complete-object-index-0 shape), streamModelToClient
//! relaying SSE chunks as NDJSON events, usage/title/compaction
//! plumbing, pause handling, and persistence.

use crate::cancel::CancelToken;
use crate::state::{AppState, TurnHandle};
use atom_core::providers::codex::{
    do_openai_codex_round, marshal_openai_codex_request, openai_codex_auth_for_key,
};
use atom_core::providers::{
    bedrock_style_for_url, context_window_tokens, provider_name_for_url, reasoning_field_for_url,
    stream_bedrock, stream_chat,
};
use atom_core::session::compaction::{
    compact_session, compact_span, compaction_prompt_text, compaction_target, compaction_threshold,
    llm_messages, should_compact_with_threshold, COMPACTION_MODEL_ID,
};
use atom_core::session::store::{usage_for_display, Session};
use atom_core::session::title::{generate_and_store_title, title_provider};
use atom_core::types::{
    ChatRequest, Message, StreamOptions, StreamResult, StreamToolCallDelta, ToolCall,
};
use atom_tools::defs::without_tool;
use chrono::Local;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// maxToolRounds is a runaway-loop guard, not a task-length budget.
/// Stopping at 30 left real sessions mid-edit: the model still had tool
/// calls queued, the turn ended with no done event, and a later "?" got
/// "I'm mid-implementation".
pub const MAX_TOOL_ROUNDS: usize = 500;
pub const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

fn cap_tool_output(text: &mut String) {
    if text.len() <= MAX_TOOL_OUTPUT_BYTES {
        return;
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... truncated at tool output byte limit");
}

// ---------------------------------------------------------------------------
// NDJSON output sink.
// ---------------------------------------------------------------------------

/// Where writeNDJSON goes: the HTTP response body channel of /send, or a
/// discard stand-in for detached dispatch turns (Go's discardWriter).
#[derive(Clone)]
pub enum EventOut {
    Response(tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::convert::Infallible>>),
    Discard,
}

impl EventOut {
    async fn write_line(&self, line: &str) {
        if let EventOut::Response(tx) = self {
            let _ = tx.send(Ok(bytes::Bytes::from(line.to_string()))).await;
        }
    }
}

/// writeNDJSON + broadcastSession: one JSON object followed by a newline
/// to the HTTP response, then the same event to every subscriber.
pub async fn emit(state: &AppState, out: &EventOut, session_id: &str, event: &Value) {
    let mut line = serde_json::to_string(event).unwrap_or_default();
    line.push('\n');
    out.write_line(&line).await;
    state.subs.broadcast(session_id, event);
}

fn event(map: Vec<(&str, Value)>) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in map {
        obj.insert(k.to_string(), v);
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// toolCallAccumulator.
// ---------------------------------------------------------------------------

fn json_valid(s: &str) -> bool {
    // Go json.Valid: the whole string must be one JSON value.
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// toolCallAccumulator rebuilds complete tool calls from a streamed
/// response's deltas. Two provider shapes are handled:
///
///   - OpenAI-style streams fragment one call's fields across many deltas
///     and use a unique Index per call; fragments are concatenated.
///   - Some routers (Ollama) stream each parallel call as a complete
///     arguments object that reuses index 0; a delta whose arguments
///     would corrupt the accumulated string starts a new call instead.
///
/// Calls are kept in arrival order so the final list matches the order
/// the model emitted them.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: Vec<ToolCall>,
    by_index: HashMap<i64, usize>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// add merges one delta into the accumulator, opening a new call when
    /// the delta clearly belongs to a different call than the one at its
    /// index: a different call ID, or a complete JSON arguments object
    /// that can't be appended to the accumulated string.
    pub fn add(&mut self, d: &StreamToolCallDelta) {
        let mut existing = self.by_index.get(&d.index).copied();
        if let Some(i) = existing {
            let e = &self.calls[i];
            // A delta naming a different call ID than the one at this index
            // starts a new call (routers reuse index 0 for every call).
            if !d.id.is_empty() && !e.id.is_empty() && d.id != e.id {
                existing = None;
            } else if !d.function.arguments.is_empty()
                && !e.function.arguments.is_empty()
                && json_valid(&e.function.arguments)
                && json_valid(&d.function.arguments)
                && !json_valid(&format!("{}{}", e.function.arguments, d.function.arguments))
            {
                // Two complete JSON objects can't form one argument string:
                // the delta is a second call reusing this index.
                existing = None;
            }
        }
        let Some(i) = existing else {
            self.calls.push(ToolCall {
                id: d.id.clone(),
                call_type: d.call_type.clone(),
                function: atom_core::types::FunctionCall {
                    name: d.function.name.clone(),
                    arguments: d.function.arguments.clone(),
                },
            });
            self.by_index.insert(d.index, self.calls.len() - 1);
            return;
        };
        // Fill in fields that arrive in later deltas.
        let e = &mut self.calls[i];
        if e.id.is_empty() && !d.id.is_empty() {
            e.id = d.id.clone();
        }
        if e.call_type.is_empty() && !d.call_type.is_empty() {
            e.call_type = d.call_type.clone();
        }
        if e.function.name.is_empty() && !d.function.name.is_empty() {
            e.function.name = d.function.name.clone();
        }
        e.function.arguments += &d.function.arguments;
    }

    /// list returns the accumulated tool calls in the order their first
    /// deltas arrived.
    pub fn list(&self) -> Vec<ToolCall> {
        self.calls.clone()
    }
}

// ---------------------------------------------------------------------------
// sanitizeMessages re-export + usageEvent + assistantMessage.
// ---------------------------------------------------------------------------

pub use atom_core::session::compaction::sanitize_messages;

/// usageEvent is the NDJSON payload the TUI uses to refresh the status
/// bar meter. prompt/completion are session totals (Input/Output); total
/// is the latest-round context size. cache fields and prompt_all are
/// session totals so hit rate is a weighted average. Cache fields are
/// omitted when zero so the viewer can tell "provider didn't report
/// cache" from a real split.
pub fn usage_event(u: &atom_core::types::StreamUsage) -> Value {
    let mut ev = serde_json::Map::new();
    ev.insert("type".into(), json!("usage"));
    ev.insert("prompt".into(), json!(u.prompt_tokens.to_string()));
    ev.insert("completion".into(), json!(u.completion_tokens.to_string()));
    ev.insert("total".into(), json!(u.total_tokens.to_string()));
    if u.cache_read_tokens > 0 {
        ev.insert("cache_read".into(), json!(u.cache_read_tokens.to_string()));
    }
    if u.cache_write_tokens > 0 {
        ev.insert(
            "cache_write".into(),
            json!(u.cache_write_tokens.to_string()),
        );
    }
    if u.prompt_tokens_all > 0 {
        ev.insert("prompt_all".into(), json!(u.prompt_tokens_all.to_string()));
    }
    Value::Object(ev)
}

/// assistantMessage builds the persisted record for one model reply: the
/// reply text, any tool calls, the provider and model that answered, and
/// the request's token usage so the stats report can attribute it. The
/// per-message usage record is what makes per-model stats exact even
/// when a session switches models mid-conversation.
pub fn assistant_message(
    model: &str,
    base_url: &str,
    result: &StreamResult,
    tool_calls: Option<&[ToolCall]>,
) -> Message {
    Message {
        role: "assistant".into(),
        content: result.content.clone(),
        reasoning: result.reasoning.clone(),
        reasoning_ms: result.reasoning_ms,
        tool_calls: tool_calls.map(|t| t.to_vec()).unwrap_or_default(),
        provider: provider_name_for_url(base_url),
        model: model.into(),
        usage: result.usage.clone(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// streamModelToClient.
// ---------------------------------------------------------------------------

/// streamModelToClient reads the provider's chunk stream and relays each
/// delta to the client as NDJSON events. It returns the full assistant
/// reply and any tool calls, exactly like the client-side stream() did
/// before the server split. When either cancellation token fires (the
/// turn was paused), it stops reading and returns the partial reply so
/// far.
pub async fn stream_model_to_client<S>(
    state: &AppState,
    out: &EventOut,
    session_id: &str,
    chunks: S,
    turn_cancel: &CancelToken,
    parent_cancel: &CancelToken,
    reasoning_field: &str,
) -> anyhow::Result<StreamResult>
where
    S: Stream<Item = anyhow::Result<atom_core::types::StreamChunk>>,
{
    let mut reply = String::new();
    let mut reasoning = String::new();
    let mut accumulator = ToolCallAccumulator::default();
    let mut usage = None;
    let mut finish_reason = String::new();
    let mut saw_reasoning = false;
    let mut saw_tool_call = false;
    let mut chunks = std::pin::pin!(chunks);
    loop {
        // Stop reading when the turn was paused. The cancelled request
        // also closes the body, so this is just a fast path.
        if turn_cancel.is_cancelled() || parent_cancel.is_cancelled() {
            break;
        }
        let item = tokio::select! {
            _ = turn_cancel.cancelled() => None,
            _ = parent_cancel.cancelled() => None,
            item = chunks.next() => item,
        };
        let chunk = match item {
            None => break,
            Some(Err(err)) => return Err(err),
            Some(Ok(c)) => c,
        };
        // The final chunk (with stream_options.include_usage) carries the
        // request's token counts.
        if let Some(u) = &chunk.usage {
            if u.total_tokens > 0 {
                usage = Some(u.clone());
            }
        }
        for choice in chunk.choices {
            if !choice.finish_reason.is_empty() {
                finish_reason = choice.finish_reason.clone();
            }
            // Pick the reasoning delta for this provider; fall back to the
            // other field when the configured one is empty (defensive
            // against router changes).
            let mut rt = choice.delta.reasoning.clone();
            if reasoning_field == "reasoning_content" {
                if !choice.delta.reasoning_content.is_empty() {
                    rt = choice.delta.reasoning_content.clone();
                }
            } else if rt.is_empty() && !choice.delta.reasoning_content.is_empty() {
                rt = choice.delta.reasoning_content.clone();
            }
            if !rt.is_empty() {
                let ev = event(vec![("type", json!("reasoning")), ("text", json!(rt))]);
                emit(state, out, session_id, &ev).await;
                reasoning.push_str(&rt);
                saw_reasoning = true;
            }
            if !choice.delta.content.is_empty() {
                if saw_reasoning {
                    let ev = event(vec![("type", json!("reasoning_end"))]);
                    emit(state, out, session_id, &ev).await;
                    saw_reasoning = false;
                }
                let ev = event(vec![
                    ("type", json!("content")),
                    ("text", json!(choice.delta.content)),
                ]);
                emit(state, out, session_id, &ev).await;
                reply.push_str(&choice.delta.content);
            }
            // Accumulate tool calls, splitting deltas that reuse an index
            // for a different call.
            if !saw_tool_call && !choice.delta.tool_calls.is_empty() {
                let ev = event(vec![("type", json!("tool_pending"))]);
                emit(state, out, session_id, &ev).await;
                saw_tool_call = true;
            }
            for tc in &choice.delta.tool_calls {
                accumulator.add(tc);
            }
        }
    }
    if saw_reasoning {
        let ev = event(vec![("type", json!("reasoning_end"))]);
        emit(state, out, session_id, &ev).await;
    }
    Ok(StreamResult {
        content: reply,
        reasoning,
        reasoning_ms: 0,
        tool_calls: accumulator.list(),
        usage,
        finish_reason,
    })
}

fn empty_response(result: &StreamResult) -> bool {
    result.content.is_empty() && result.reasoning.is_empty() && result.tool_calls.is_empty()
}

// ---------------------------------------------------------------------------
// Persistence helpers.
// ---------------------------------------------------------------------------

/// Propagates the fields Go mutated through the shared *Session pointer
/// onto the stored session so the next save persists them (the Rust
/// store hands out clones).
fn sync_back(store: &atom_core::session::store::SessionStore, id: &str, sess: &Session) {
    store.modify(id, |s| {
        s.usage = sess.usage.clone();
        s.compaction_summary = sess.compaction_summary.clone();
        s.compacted_through = sess.compacted_through;
        s.thinking = sess.thinking.clone();
    });
}

/// persistSession saves a session's messages. A generated LLM title is
/// kept; otherwise the first user message (truncated) is used so the
/// picker has a name immediately. Title generation runs in the background.
pub async fn persist_session(state: &Arc<AppState>, sess: &Session, id: &str) {
    let mut title = String::new();
    if !sess.title_generated {
        for m in &sess.messages {
            if m.role == "user" {
                title = truncate_bytes_ellipsis(&m.content, 60);
                break;
            }
        }
    }
    sync_back(&state.store, id, sess);
    state.store.update(id, sess.messages.clone(), &title);
    state.subs.broadcast(id, &json!({"type": "saved"}));
    if !sess.title_generated {
        kickoff_title_generation(state, id.to_string());
    }
}

/// Go slices the title bytes raw (`title[:60]`); truncate at the last
/// char boundary at or below the limit so Rust stays panic-free on
/// multi-byte content.
fn truncate_bytes_ellipsis(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}

/// kickoffTitleGeneration names the session in the background. Failures
/// leave the fallback title in place; the chat turn is not blocked.
pub fn kickoff_title_generation(state: &Arc<AppState>, id: String) {
    let state = state.clone();
    tokio::spawn(async move {
        let target = title_provider().await;
        if let Some(title) = generate_and_store_title(
            &state.store,
            &id,
            &target.base_url,
            &target.key,
            &target.model,
        )
        .await
        {
            state
                .subs
                .broadcast(&id, &json!({"type": "title", "text": title}));
        }
    });
}

/// finishPausedTurn ends a paused turn: it tells the client the stream
/// stopped, persists the partial conversation, and notifies other viewers.
async fn finish_paused_turn(state: &Arc<AppState>, sess: &Session, out: &EventOut, id: &str) {
    let ev = json!({"type": "paused"});
    emit(state, out, id, &ev).await;
    persist_session(state, sess, id).await;
}

/// foldSession summarizes older turns and notifies the client with the
/// same start/end events the TUI uses for thinking: compaction then
/// compaction_end. Both events name the compaction model so the TUI can
/// show it (and the fold duration) like a regular model output. The
/// transcript on sess.messages is left intact.
pub async fn fold_session(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    extra: &str,
) -> anyhow::Result<()> {
    let target = compaction_target().await;
    let model = if target.model.trim().is_empty() {
        COMPACTION_MODEL_ID.to_string()
    } else {
        target.model.trim().to_string()
    };
    let start = json!({"type": "compaction", "model": model});
    emit(state, out, id, &start).await;
    let res = compact_session(sess, None, &target.base_url, &target.key, &model, extra).await;
    let mut end = json!({"type": "compaction_end", "model": model});
    if res.is_ok() {
        end["text"] = json!(compaction_prompt_text(&sess.compaction_summary));
    }
    emit(state, out, id, &end).await;
    res?;
    if let Some(u) = &sess.usage {
        let shown = usage_for_display(Some(u), Some(sess), None).unwrap_or_else(|| u.clone());
        let ev = usage_event(&shown);
        emit(state, out, id, &ev).await;
    }
    persist_session(state, sess, id).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// The turn loop.
// ---------------------------------------------------------------------------

pub struct TurnOpts {
    pub message: String,
    pub thinking: String,
    pub key: String,
    pub base_url: String,
    pub reasoning_field: String,
    pub turn_id: String,
    pub images: Vec<atom_core::types::ImageData>,
    pub compact: bool,
    pub compact_instructions: String,
    pub skip_append: bool,
}

/// Combined cancellation view of a turn handle plus its parent context
/// (the HTTP request lifetime; never cancelled for detached dispatch
/// turns, mirroring context.Background()). No awaitable merge is built —
/// every wait selects over both tokens so nothing leaks.
#[derive(Clone)]
pub struct TurnCtx {
    pub handle: Arc<TurnHandle>,
    pub parent: CancelToken,
}

impl TurnCtx {
    /// turnCtx.Err() != nil
    pub fn err(&self) -> bool {
        self.handle.is_cancelled() || self.parent.is_cancelled()
    }
}

fn fold_error_event(err: &anyhow::Error) -> Value {
    event(vec![
        ("type", json!("error")),
        ("message", json!(format!("compaction failed: {err}"))),
    ])
}

fn turn_duration_ms(started_at: Instant) -> i64 {
    started_at
        .elapsed()
        .as_millis()
        .max(1)
        .min(i64::MAX as u128) as i64
}

fn done_event(duration_ms: i64, model: &str) -> Value {
    json!({
        "type": "done",
        "duration_ms": duration_ms,
        "model": model,
    })
}

/// runSessionTurn processes one chat turn: it appends the user's
/// message, calls the model API, streams content/reasoning/tool events
/// back to the client as NDJSON, executes any tool calls, and loops
/// until the model gives a final answer. The session is persisted at the
/// end. The turn can be paused at any point: when the client disconnects,
/// when the last subscriber leaves, or when a pause request arrives.
pub async fn run_session_turn(
    state: &Arc<AppState>,
    sess: &mut Session,
    id: &str,
    opts: TurnOpts,
    out: EventOut,
    parent: CancelToken,
) {
    let started_at = Instant::now();
    let mut key = opts.key.clone();
    let base_url = opts.base_url.clone();

    let compact_only = opts.compact && opts.message.is_empty() && opts.images.is_empty();
    if !opts.thinking.is_empty() {
        sess.thinking = opts.thinking.clone();
    }
    if !compact_only && !opts.skip_append {
        sess.messages.push(Message {
            role: "user".into(),
            content: opts.message.clone(),
            images: opts.images.clone(),
            ..Default::default()
        });
    }

    let handle = state.turns.start_turn(id, &opts.turn_id);
    let ctx = TurnCtx { handle, parent };
    let parent_id = sess.parent_id.clone();
    if !parent_id.is_empty() {
        state
            .store
            .update_delegate_status(id, atom_core::session::store::DelegateStatus::Working);
        state
            .subs
            .broadcast(&parent_id, &json!({"type": "children"}));
    }

    let cwd = PathBuf::from(sess.cwd.clone());
    let mut tools = atom_tools::tool_definitions_with_mcp(&cwd).await;
    if !parent_id.is_empty() {
        tools = without_tool(&tools, "dispatch");
    }

    // One failed auto-compact must not be retried on every tool round:
    // the summarizer uses a 10-minute HTTP timeout, so a down Ollama
    // would stall the turn. A manual /compact still forces a fold.
    let mut compact_failed = false;

    if compact_only {
        if let Err(err) = fold_session(state, sess, &out, id, &opts.compact_instructions).await {
            emit(state, &out, id, &fold_error_event(&err)).await;
        }
        let done = done_event(turn_duration_ms(started_at), "");
        emit(state, &out, id, &done).await;
        persist_session(state, sess, id).await;
        end_of_turn(state, id, &ctx.handle, &parent_id);
        return;
    }

    let mut finished = false;
    let mut nudge_attempt: usize = 0;
    let mut empty_response_attempt: usize = 0;
    'rounds: for _round in 0..MAX_TOOL_ROUNDS {
        // Stop immediately when the turn was paused.
        if ctx.err() {
            finish_paused_turn(state, sess, &out, id).await;
            end_of_turn(state, id, &ctx.handle, &parent_id);
            return;
        }

        let forced = ctx.handle.take_compact();
        // Auto-compact threshold: the smaller of the fixed threshold and
        // the session model's window minus headroom, so small-context
        // models fold before the provider starts rejecting requests.
        let provider = provider_name_for_url(&base_url);
        let auto_threshold = compaction_threshold(context_window_tokens(&provider, &sess.model));
        let mut fold_now = forced.is_some()
            || (!compact_failed && should_compact_with_threshold(sess, auto_threshold));
        if fold_now && forced.is_none() {
            fold_now = compact_span(sess).is_some();
        }
        if fold_now {
            let instructions = forced.clone().unwrap_or_default();
            if let Err(err) = fold_session(state, sess, &out, id, &instructions).await {
                if forced.is_none() {
                    compact_failed = true;
                }
                emit(state, &out, id, &fold_error_event(&err)).await;
            }
        }

        // Tell viewers a model round is starting (first token is still
        // seconds away: history rebuild, compaction, context upload).
        // The TUI shows its live "Thinking" indicator from this event.
        let ev = event(vec![("type", json!("round_start"))]);
        emit(state, &out, id, &ev).await;

        // Build the request: instructions, an optional compaction brief,
        // and the unsummarized tail. Tool calls whose arguments aren't
        // valid JSON are dropped (plus the tool results that answered
        // them); the API rejects requests containing them with 400
        // "invalid tool call arguments".
        let mut msgs = llm_messages(sess);
        msgs.insert(
            sess.instructions.len().min(msgs.len()),
            Message {
                role: "system".into(),
                content: Local::now().format("%D,%H:%M").to_string(),
                ..Default::default()
            },
        );
        let reasoning_field = if !opts.reasoning_field.is_empty() {
            opts.reasoning_field.clone()
        } else {
            reasoning_field_for_url(&base_url)
        };
        let round_cancel = CancelToken::new();
        ctx.handle.set_round_cancel(Some(round_cancel));

        let result: StreamResult = if bedrock_style_for_url(&base_url) {
            // Bedrock Converse: bearer-token auth, per-request regional
            // endpoint, binary eventstream response. Chunks come back in
            // the shared OpenAI-delta shape, so the relay below is
            // unchanged.
            let chunks = match stream_bedrock(&base_url, &key, &sess.model, &msgs, &tools).await {
                Ok(c) => c,
                Err(err) => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(state, sess, &out, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                        continue 'rounds;
                    }
                    let msg = provider_error_message(&err, &base_url);
                    let ev = event(vec![
                        ("type", json!("error")),
                        ("message", json!(msg.clone())),
                    ]);
                    emit(state, &out, id, &ev).await;
                    sess.messages.push(Message {
                        role: "error".into(),
                        content: msg,
                        ..Default::default()
                    });
                    persist_session(state, sess, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
            };
            let r = stream_model_to_client(
                state,
                &out,
                id,
                chunks,
                &ctx.handle.cancel_token(),
                &ctx.parent,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(state, sess, &out, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                    let msg = provider_error_message(&err, &base_url);
                    let ev = event(vec![
                        ("type", json!("error")),
                        ("message", json!(msg.clone())),
                    ]);
                    emit(state, &out, id, &ev).await;
                    sess.messages.push(Message {
                        role: "error".into(),
                        content: msg,
                        ..Default::default()
                    });
                    persist_session(state, sess, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
            }
        } else if openai_codex_auth_for_key(&key).is_some() {
            let codex_body =
                match marshal_openai_codex_request(&sess.model, &msgs, &tools, &opts.thinking) {
                    Ok(b) => b,
                    Err(err) => {
                        ctx.handle.set_round_cancel(None);
                        let msg = err.to_string();
                        let ev = event(vec![
                            ("type", json!("error")),
                            ("message", json!(msg.clone())),
                        ]);
                        emit(state, &out, id, &ev).await;
                        sess.messages.push(Message {
                            role: "error".into(),
                            content: msg,
                            ..Default::default()
                        });
                        persist_session(state, sess, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                };
            match do_openai_codex_round(&key, &codex_body).await {
                Ok(outcome) => {
                    key = outcome.key;
                    for ev in &outcome.events {
                        emit(state, &out, id, ev).await;
                    }
                    outcome.result
                }
                Err(round_err) => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(state, sess, &out, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                        continue 'rounds;
                    }
                    let msg = round_err.to_string();
                    let ev = event(vec![
                        ("type", json!("error")),
                        ("message", json!(msg.clone())),
                    ]);
                    emit(state, &out, id, &ev).await;
                    sess.messages.push(Message {
                        role: "error".into(),
                        content: msg,
                        ..Default::default()
                    });
                    persist_session(state, sess, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
            }
        } else {
            let req_body = ChatRequest {
                model: sess.model.clone(),
                messages: msgs,
                stream: true,
                tools: tools.clone(),
                reasoning_effort: opts.thinking.clone(),
                stream_options: Some(StreamOptions {
                    include_usage: true,
                }),
            };

            let chunks = match stream_chat(&base_url, &key, req_body).await {
                Ok(c) => c,
                Err(err) => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(state, sess, &out, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                        continue 'rounds;
                    }
                    let msg = provider_error_message(&err, &base_url);
                    let ev = event(vec![
                        ("type", json!("error")),
                        ("message", json!(msg.clone())),
                    ]);
                    emit(state, &out, id, &ev).await;
                    sess.messages.push(Message {
                        role: "error".into(),
                        content: msg,
                        ..Default::default()
                    });
                    persist_session(state, sess, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
            };
            // Stream the model's SSE response, relaying each delta to the
            // client as an NDJSON event. The reasoning delta field differs
            // per provider ("reasoning" vs "reasoning_content").
            let r = stream_model_to_client(
                state,
                &out,
                id,
                chunks,
                &ctx.handle.cancel_token(),
                &ctx.parent,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(state, sess, &out, id).await;
                        end_of_turn(state, id, &ctx.handle, &parent_id);
                        return;
                    }
                    let msg = provider_error_message(&err, &base_url);
                    let ev = event(vec![
                        ("type", json!("error")),
                        ("message", json!(msg.clone())),
                    ]);
                    emit(state, &out, id, &ev).await;
                    sess.messages.push(Message {
                        role: "error".into(),
                        content: msg,
                        ..Default::default()
                    });
                    persist_session(state, sess, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
            }
        };

        if empty_response(&result) {
            if empty_response_attempt < atom_core::providers::MAX_EMPTY_RESPONSE_RETRIES {
                let delays = atom_core::providers::retry::provider_retry_delays();
                let delay = delays
                    .get(empty_response_attempt)
                    .copied()
                    .unwrap_or(Duration::from_secs(15));
                empty_response_attempt += 1;
                let retry_ev = event(vec![
                    ("type", json!("retry")),
                    ("message", json!("empty provider response; retrying")),
                    ("attempt", json!(empty_response_attempt)),
                ]);
                emit(state, &out, id, &retry_ev).await;
                if sleep_ctx(ctx.handle.cancel_token(), &ctx.parent, delay).await {
                    finish_paused_turn(state, sess, &out, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
                continue 'rounds;
            }
            let msg = format!(
                "provider returned an empty response after {} attempts",
                empty_response_attempt + 1
            );
            let ev = event(vec![
                ("type", json!("error")),
                ("message", json!(msg.clone())),
            ]);
            emit(state, &out, id, &ev).await;
            sess.messages.push(Message {
                role: "error".into(),
                content: msg,
                ..Default::default()
            });
            persist_session(state, sess, id).await;
            end_of_turn(state, id, &ctx.handle, &parent_id);
            return;
        }
        empty_response_attempt = 0;

        // The provider's token count for this request is the current
        // context snapshot (history, instructions, and tool results).
        // Remember it on the session and tell every viewer, so the
        // status bar indicator updates without waiting for a reload.
        if let Some(u) = &result.usage {
            sess.usage = Some(u.clone());
            let shown =
                usage_for_display(Some(u), Some(sess), Some(u)).unwrap_or_else(|| u.clone());
            let ev = usage_event(&shown);
            emit(state, &out, id, &ev).await;
        }

        // Paused mid-stream: keep the partial reply so nothing is lost.
        if ctx.err() {
            if !result.content.is_empty() || !result.reasoning.is_empty() {
                sess.messages
                    .push(assistant_message(&sess.model, &base_url, &result, None));
            }
            finish_paused_turn(state, sess, &out, id).await;
            end_of_turn(state, id, &ctx.handle, &parent_id);
            return;
        }

        // /compact (or auto-fold) interrupted this round. Keep any
        // partial text, fold, and start a fresh model request.
        if let Some(extra) = ctx.handle.take_compact() {
            if !result.content.is_empty() || !result.reasoning.is_empty() {
                sess.messages
                    .push(assistant_message(&sess.model, &base_url, &result, None));
            }
            if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                emit(state, &out, id, &fold_error_event(&ferr)).await;
            }
            continue 'rounds;
        }

        // No tool calls: the model gave its final answer for this turn.
        if result.tool_calls.is_empty() {
            if atom_core::providers::should_nudge_incomplete_reasoning(&result, nudge_attempt) {
                sess.messages
                    .push(assistant_message(&sess.model, &base_url, &result, None));
                sess.messages.push(Message {
                    role: "nudge".into(),
                    content: atom_core::providers::REASONING_NUDGE_TEXT.into(),
                    ..Default::default()
                });
                let nudge_ev = event(vec![
                    ("type", json!("nudge")),
                    ("message", json!("truncated reasoning; continuing")),
                ]);
                emit(state, &out, id, &nudge_ev).await;
                let delays = atom_core::providers::retry::provider_retry_delays();
                let delay = delays
                    .get(nudge_attempt)
                    .copied()
                    .unwrap_or(Duration::from_secs(15));
                if sleep_ctx(ctx.handle.cancel_token(), &ctx.parent, delay).await {
                    finish_paused_turn(state, sess, &out, id).await;
                    end_of_turn(state, id, &ctx.handle, &parent_id);
                    return;
                }
                nudge_attempt += 1;
                continue 'rounds;
            }
            let duration_ms = turn_duration_ms(started_at);
            let mut message = assistant_message(&sess.model, &base_url, &result, None);
            message.duration_ms = duration_ms;
            sess.messages.push(message);
            if let Some(extra) = ctx.handle.take_compact() {
                if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                    emit(state, &out, id, &fold_error_event(&ferr)).await;
                }
            }
            let done = done_event(duration_ms, &sess.model);
            emit(state, &out, id, &done).await;
            finished = true;
            break;
        }
        nudge_attempt = 0;

        // Record the assistant's tool-call message before executing tools.
        // Dispatch get_result(wait:true), approvals, and other tools can block;
        // persisting here keeps navigation/reload from reverting to the
        // pre-turn transcript while execution is still in progress.
        sess.messages.push(assistant_message(
            &sess.model,
            &base_url,
            &result,
            Some(&result.tool_calls),
        ));
        persist_session(state, sess, id).await;

        // Execute each tool and feed the result back to the model.
        for (tool_index, tc) in result.tool_calls.iter().enumerate() {
            let ev = event(vec![
                ("type", json!("tool")),
                ("name", json!(tc.function.name)),
                ("arguments", json!(tc.function.arguments)),
            ]);
            emit(state, &out, id, &ev).await;

            let approver = crate::dispatch::ServerApprover::for_turn(
                state.clone(),
                id.to_string(),
                out.clone(),
            );
            let bridge = crate::dispatch::DispatchBridge::new(
                state.clone(),
                id.to_string(),
                ctx.handle.cancel_token(),
                key.clone(),
                base_url.clone(),
                opts.reasoning_field.clone(),
            );
            let seen = state.file_seen_for(id);
            let tool_ctx = atom_tools::ToolCtx {
                cwd: cwd.clone(),
                session_id: id.to_string(),
                api_key: key.clone(),
                base_url: base_url.clone(),
                reasoning_field: reasoning_field.clone(),
                sandbox_cfg: state.cfg.clone(),
                approver: &approver,
                spawner: Some(&bridge),
                file_seen: Some(seen.as_ref()),
            };
            let mut outcome =
                atom_tools::execute_tool(&tool_ctx, &tc.function.name, &tc.function.arguments)
                    .await;
            cap_tool_output(&mut outcome.text);

            sess.messages.push(Message {
                role: "tool".into(),
                tool_call_id: tc.id.clone(),
                content: outcome.text.clone(),
                images: outcome.images.clone(),
                diff: outcome.diff.clone(),
                ..Default::default()
            });
            // Save each tool result immediately. Besides crash safety, this
            // makes an in-progress blocking dispatch visible after navigation.
            persist_session(state, sess, id).await;
            let result_ev = event(vec![
                ("type", json!("tool_result")),
                ("text", json!(outcome.text)),
            ]);
            emit(state, &out, id, &result_ev).await;
            // Send any file diff as its own event so the client can attach
            // it to the tool block it already rendered.
            if !outcome.diff.is_empty() {
                let diff_ev = event(vec![
                    ("type", json!("tool_diff")),
                    ("diff", json!(outcome.diff)),
                ]);
                emit(state, &out, id, &diff_ev).await;
            }
            // Stop between tools when the turn was paused.
            if ctx.err() {
                for skipped in &result.tool_calls[tool_index + 1..] {
                    sess.messages.push(Message {
                        role: "tool".into(),
                        tool_call_id: skipped.id.clone(),
                        content: "error: tool execution cancelled before it started".into(),
                        ..Default::default()
                    });
                }
                persist_session(state, sess, id).await;
                finish_paused_turn(state, sess, &out, id).await;
                end_of_turn(state, id, &ctx.handle, &parent_id);
                return;
            }
        }
        // Loop again: the model now sees the tool results.
    }

    if !finished {
        // Exhausted the runaway guard while the model was still calling
        // tools. Persist the partial turn and tell the client, instead of
        // going silent with unfinished work.
        let ev = event(vec![
            ("type", json!("error")),
            (
                "message",
                json!(format!(
                    "stopped after {} tool rounds; send a message to continue",
                    MAX_TOOL_ROUNDS
                )),
            ),
        ]);
        emit(state, &out, id, &ev).await;
        let done = done_event(turn_duration_ms(started_at), "");
        emit(state, &out, id, &done).await;
    }

    persist_session(state, sess, id).await;
    end_of_turn(state, id, &ctx.handle, &parent_id);
}

/// Shared tail of run_session_turn: deregister the turn and notify the
/// parent session's viewers (Go's deferred endTurn/children broadcast).
fn end_of_turn(state: &Arc<AppState>, id: &str, handle: &Arc<TurnHandle>, parent_id: &str) {
    state.turns.end_turn(id, handle);
    if !parent_id.is_empty() {
        if let Some(child) = state.store.get(id) {
            let status = if child.cancelled {
                atom_core::session::store::DelegateStatus::Cancelled
            } else if child
                .messages
                .iter()
                .rev()
                .find(|message| matches!(message.role.as_str(), "assistant" | "error"))
                .is_some_and(|message| message.role == "error")
            {
                atom_core::session::store::DelegateStatus::Error
            } else {
                atom_core::session::store::DelegateStatus::Done
            };
            state.store.update_delegate_status(id, status);
        }
        state
            .subs
            .broadcast(parent_id, &json!({"type": "children"}));
    }
}

fn provider_error_message(err: &anyhow::Error, base_url: &str) -> String {
    let msg = err.to_string();
    // stream_chat reports HTTP failures as "<status>: chat completions
    // request failed"; those mirror Go's ProviderHTTPError and get no hint.
    if msg.len() >= 4
        && msg.as_bytes()[0].is_ascii_digit()
        && msg.as_bytes()[1].is_ascii_digit()
        && msg.as_bytes()[2].is_ascii_digit()
        && msg.as_bytes()[3] == b':'
    {
        return msg;
    }
    if base_url.contains("localhost") {
        return format!(
            "{msg} (is Ollama running? or set OLLAMA_API_KEY to talk to ollama.com directly)"
        );
    }
    msg
}

/// sleepCtx sleeps or reports cancellation (true = cancelled).
async fn sleep_ctx(turn: CancelToken, parent: &CancelToken, delay: Duration) -> bool {
    tokio::select! {
        _ = turn.cancelled() => true,
        _ = parent.cancelled() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_event_includes_elapsed_milliseconds() {
        let event = done_event(25, "test-model");

        assert_eq!(event["type"], "done");
        assert_eq!(event["duration_ms"], 25);
        assert_eq!(event["model"], "test-model");
    }

    fn delta(index: i64, id: &str, name: &str, args: &str) -> StreamToolCallDelta {
        StreamToolCallDelta {
            index,
            id: id.into(),
            call_type: "function".into(),
            function: atom_core::types::FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    /// OpenAI-style streams fragment one call's arguments across many deltas
    /// and use a unique index per call. Fragments must be concatenated.
    #[test]
    fn accumulator_fragments() {
        let mut acc = ToolCallAccumulator::new();
        acc.add(&delta(0, "call_a", "bash", r#"{"command":"git lo"#));
        acc.add(&delta(0, "", "", r#"g --oneline -20"}"#));
        acc.add(&delta(1, "call_b", "bash", r#"{"command":"ls"}"#));
        let calls = acc.list();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(
            calls[0].function.arguments,
            r#"{"command":"git log --oneline -20"}"#
        );
        assert_eq!(calls[1].function.arguments, r#"{"command":"ls"}"#);
    }

    /// Some routers (Ollama) stream each parallel call as a complete
    /// arguments object that reuses index 0. The second object must open a
    /// new call instead of being appended to the first.
    #[test]
    fn accumulator_reused_index() {
        let mut acc = ToolCallAccumulator::new();
        acc.add(&delta(
            0,
            "",
            "bash",
            r#"{"command":"git log --oneline -20"}"#,
        ));
        acc.add(&delta(0, "", "bash", r#"{"command":"ls"}"#));
        let calls = acc.list();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].function.arguments,
            r#"{"command":"git log --oneline -20"}"#
        );
        assert_eq!(calls[1].function.arguments, r#"{"command":"ls"}"#);
    }

    /// The same reuse of index 0 with distinct call IDs must also split,
    /// and later deltas of the second call must keep merging into it.
    #[test]
    fn accumulator_reused_index_with_ids() {
        let mut acc = ToolCallAccumulator::new();
        acc.add(&delta(0, "call_a", "bash", r#"{"command":"git log"}"#));
        acc.add(&delta(0, "call_b", "bash", r#"{"command":"ls"#));
        acc.add(&delta(0, "", "", r#""}"#));
        let calls = acc.list();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.arguments, r#"{"command":"git log"}"#);
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].function.arguments, r#"{"command":"ls"}"#);
    }

    /// Fields that arrive in later deltas (name, id) must be filled in.
    #[test]
    fn accumulator_late_fields() {
        let mut acc = ToolCallAccumulator::new();
        acc.add(&delta(0, "", "", r#"{"query":"x"#));
        acc.add(&delta(0, "call_a", "web_search", r#""}"#));
        let calls = acc.list();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "web_search");
        assert_eq!(calls[0].function.arguments, r#"{"query":"x"}"#);
    }

    /// Wire chunks that omit `name` or `arguments` inside function (the
    /// vision-exp router streams both as separate fragments, exactly like
    /// OpenAI) must decode and accumulate. Go's json.Unmarshal zero-fills
    /// missing fields; the strict serde port once dropped every fragment
    /// chunk, leaving tool-call arguments permanently empty.
    #[test]
    fn stream_chunk_parses_field_fragments() {
        let chunks: Vec<&str> = vec![
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"vector_search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"query\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}"#,
        ];
        let mut acc = ToolCallAccumulator::new();
        for c in chunks {
            let chunk: atom_core::types::StreamChunk =
                serde_json::from_str(c).expect("chunk should decode");
            for choice in chunk.choices {
                for tc in &choice.delta.tool_calls {
                    acc.add(tc);
                }
            }
        }
        let calls = acc.list();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "vector_search");
        assert_eq!(calls[0].function.arguments, r#"{"query"}"#);
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn mk_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: atom_core::types::FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    /// sanitizeMessages must drop malformed tool calls and the tool results
    /// that answered them, while keeping valid turns intact.
    #[test]
    fn sanitize_drops_malformed_calls_and_orphans() {
        use atom_core::types::{FunctionCall, ToolCall as TC};
        let _ = mk_tool_call;
        let msgs = vec![
            msg("user", "hello"),
            Message {
                role: "assistant".into(),
                tool_calls: vec![
                    TC {
                        id: "a".into(),
                        call_type: String::new(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"ls"}{"command":"pwd"}"#.into(), // invalid JSON
                        },
                    },
                    TC {
                        id: "b".into(),
                        call_type: String::new(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"pwd"}"#.into(), // valid
                        },
                    },
                ],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "a".into(),
                content: "error parsing arguments".into(),
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "b".into(),
                content: "/Users/andrewmiller".into(),
                ..Default::default()
            },
            msg("assistant", "done"),
        ];
        let out = sanitize_messages(&msgs);
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[1].tool_calls.len(), 1);
        assert_eq!(out[1].tool_calls[0].id, "b");
        assert_eq!(out[2].role, "tool");
        assert_eq!(out[2].tool_call_id, "b");
        // The input must not be mutated.
        assert_eq!(msgs[1].tool_calls.len(), 2);
    }

    /// An assistant message whose every tool call is invalid disappears
    /// entirely, along with its orphaned tool results.
    #[test]
    fn sanitize_drops_empty_tool_call_turn() {
        let msgs = vec![
            msg("user", "hello"),
            Message {
                role: "assistant".into(),
                tool_calls: vec![mk_tool_call("a", "bash", "not json")],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "a".into(),
                content: "error".into(),
                ..Default::default()
            },
            msg("assistant", "final"),
        ];
        let got = sanitize_messages(&msgs);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].role, "user");
        assert_eq!(got[1].role, "assistant");
        assert_eq!(got[1].content, "final");
    }

    #[test]
    fn usage_event_fields() {
        use atom_core::types::StreamUsage;
        let ev = usage_event(&StreamUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            ..Default::default()
        });
        assert_eq!(ev["type"], "usage");
        assert_eq!(ev["prompt"], "10");
        assert_eq!(ev["total"], "12");
        assert!(ev.get("cache_read").is_none());
        assert!(ev.get("cache_write").is_none());

        let ev = usage_event(&StreamUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            cache_read_tokens: 8,
            cache_write_tokens: 1,
            ..Default::default()
        });
        assert_eq!(ev["cache_read"], "8");
        assert_eq!(ev["cache_write"], "1");

        let ev = usage_event(&StreamUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            cache_read_tokens: 18,
            prompt_tokens_all: 22,
            ..Default::default()
        });
        assert_eq!(ev["prompt"], "10");
        assert_eq!(ev["prompt_all"], "22");
        assert_eq!(ev["cache_read"], "18");
    }

    #[test]
    fn tool_output_is_byte_bounded_at_utf8_boundary() {
        let mut text = "é".repeat(MAX_TOOL_OUTPUT_BYTES);
        cap_tool_output(&mut text);
        assert!(text.len() < MAX_TOOL_OUTPUT_BYTES + 100);
        assert!(text.contains("truncated at tool output byte limit"));
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn empty_provider_result_is_not_a_successful_reply() {
        assert!(empty_response(&StreamResult::default()));
        assert!(empty_response(&StreamResult {
            finish_reason: "stop".into(),
            ..Default::default()
        }));
        assert!(!empty_response(&StreamResult {
            content: "answer".into(),
            ..Default::default()
        }));
        assert!(!empty_response(&StreamResult {
            reasoning: "thinking".into(),
            ..Default::default()
        }));
        assert!(!empty_response(&StreamResult {
            tool_calls: vec![mk_tool_call("a", "bash", r#"{"command":"true"}"#)],
            ..Default::default()
        }));
    }

    #[test]
    fn truncate_title_bytes_like_go() {
        assert_eq!(truncate_bytes_ellipsis("short", 60), "short");
        let long = "x".repeat(80);
        assert_eq!(
            truncate_bytes_ellipsis(&long, 60),
            format!("{}...", "x".repeat(60))
        );
    }

    #[tokio::test]
    async fn stream_model_relay_emits_reasoning_end_for_truncated_thinking() {
        use crate::state::ConnTracker;
        use atom_core::types::StreamChunk;
        use atom_sandbox::policy::SandboxConfig;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(atom_core::session::store::SessionStore::open_in_dir(dir.path()).unwrap());
        let state = Arc::new(AppState::new(
            store,
            SandboxConfig {
                mode: atom_sandbox::policy::SandboxMode::Off,
                ..Default::default()
            },
            Arc::new(ConnTracker::new()),
        ));
        let chunks: Vec<anyhow::Result<StreamChunk>> = vec![
            Ok(serde_json::from_str::<StreamChunk>(
                r#"{"choices":[{"delta":{"reasoning_content":"plan "}}]}"#,
            )
            .unwrap()),
            Ok(serde_json::from_str::<StreamChunk>(
                r#"{"choices":[{"delta":{"reasoning_content":"more"},"finish_reason":"length"}]}"#,
            )
            .unwrap()),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let out = EventOut::Response(tx);
        let result = stream_model_to_client(
            &state,
            &out,
            "s1",
            futures::stream::iter(chunks),
            &CancelToken::new(),
            &CancelToken::new(),
            "reasoning_content",
        )
        .await
        .unwrap();
        assert_eq!(result.reasoning, "plan more");
        assert_eq!(result.finish_reason, "length");
        assert!(result.content.is_empty());
        assert!(result.tool_calls.is_empty());

        let mut raw = Vec::<u8>::new();
        while let Ok(bytes) = rx.try_recv() {
            raw.extend_from_slice(&bytes.unwrap());
        }
        let text = String::from_utf8(raw).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "reasoning");
        assert_eq!(lines[1]["type"], "reasoning");
        assert_eq!(lines[2]["type"], "reasoning_end");
    }

    #[tokio::test]
    async fn stream_model_relay_emits_tool_pending_once() {
        use crate::state::ConnTracker;
        use atom_core::types::StreamChunk;
        use atom_sandbox::policy::SandboxConfig;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(atom_core::session::store::SessionStore::open_in_dir(dir.path()).unwrap());
        let state = Arc::new(AppState::new(
            store,
            SandboxConfig {
                mode: atom_sandbox::policy::SandboxMode::Off,
                ..Default::default()
            },
            Arc::new(ConnTracker::new()),
        ));
        let chunks: Vec<anyhow::Result<StreamChunk>> = [
            r#"{"choices":[{"delta":{"content":"Retrying with low."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"dispatch","arguments":"{\"tasks\":["}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"]}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]
        .into_iter()
        .map(|chunk| Ok(serde_json::from_str(chunk).unwrap()))
        .collect();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let result = stream_model_to_client(
            &state,
            &EventOut::Response(tx),
            "s1",
            futures::stream::iter(chunks),
            &CancelToken::new(),
            &CancelToken::new(),
            "reasoning",
        )
        .await
        .unwrap();
        assert_eq!(result.tool_calls.len(), 1);

        let mut raw = Vec::<u8>::new();
        while let Ok(bytes) = rx.try_recv() {
            raw.extend_from_slice(&bytes.unwrap());
        }
        let event_types: Vec<String> = String::from_utf8(raw)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .into()
            })
            .collect();
        assert_eq!(event_types, vec!["content", "tool_pending"]);
    }

    #[tokio::test]
    async fn stream_model_relay_propagates_body_error() {
        use crate::state::ConnTracker;
        use atom_core::types::StreamChunk;
        use atom_sandbox::policy::SandboxConfig;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(atom_core::session::store::SessionStore::open_in_dir(dir.path()).unwrap());
        let state = AppState::new(
            store,
            SandboxConfig {
                mode: atom_sandbox::policy::SandboxMode::Off,
                ..Default::default()
            },
            Arc::new(ConnTracker::new()),
        );
        let chunks: Vec<anyhow::Result<StreamChunk>> =
            vec![Err(anyhow::anyhow!("connection reset"))];

        let err = stream_model_to_client(
            &state,
            &EventOut::Discard,
            "s1",
            futures::stream::iter(chunks),
            &CancelToken::new(),
            &CancelToken::new(),
            "reasoning",
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "connection reset");
    }
}
