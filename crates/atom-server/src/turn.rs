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
    anthropic_style_for_url, api_protocol_for, bedrock_style_for_url, context_window_tokens,
    model_supports_image_input, provider_name_for_url, reasoning_field_for_url, stream_anthropic,
    stream_bedrock, stream_chat, stream_responses, APIProtocol,
};
use atom_core::session::compaction::{
    compact_session, compact_span, compaction_prompt_text, compaction_target, compaction_threshold,
    llm_messages, should_compact_with_threshold, COMPACTION_MODEL_ID,
};
use atom_core::session::store::{usage_for_display, Session};
use atom_core::session::title::{generate_title, title_provider};
use atom_core::types::{
    ChatRequest, Message, StreamOptions, StreamResult, StreamToolCallDelta, ToolCall,
};
use atom_sandbox::exec::PendingExit;
use atom_tools::defs::without_tool;
use chrono::{Local, Utc};
use futures::{FutureExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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

/// strip_images_for_text_only_model removes attached images from every
/// message so a text-only model does not reject the request with a 400.
/// Messages that carried only an image (no text) get a placeholder so
/// their content is not empty, which some APIs reject outright. Only the
/// outgoing messages are touched; the persisted session keeps the images.
fn strip_images_for_text_only_model(msgs: &mut [Message]) {
    for m in msgs.iter_mut() {
        if m.images.is_empty() {
            continue;
        }
        m.images.clear();
        if m.content.trim().is_empty() {
            m.content = "[image attached]".to_string();
        }
    }
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
        // Some routers stream an empty arguments object as a placeholder
        // and send the real object in a later delta; the placeholder must
        // be overwritten, not treated as a second call.
        let mut replace_args = false;
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
                // the delta is a second call reusing this index — unless the
                // first is just an empty placeholder object.
                if e.function.arguments == "{}" {
                    replace_args = true;
                } else {
                    existing = None;
                }
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
        if replace_args {
            e.function.arguments = d.function.arguments.clone();
        } else {
            e.function.arguments += &d.function.arguments;
        }
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
        reasoning_signature: result.reasoning_signature.clone(),
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
#[allow(clippy::too_many_arguments)]
pub async fn stream_model_to_client<S>(
    state: &AppState,
    out: &EventOut,
    session_id: &str,
    chunks: S,
    turn_cancel: &CancelToken,
    parent_cancel: &CancelToken,
    round_cancel: &CancelToken,
    reasoning_field: &str,
) -> anyhow::Result<StreamResult>
where
    S: Stream<Item = anyhow::Result<atom_core::types::StreamChunk>>,
{
    let mut reply = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = String::new();
    let mut accumulator = ToolCallAccumulator::default();
    let mut usage = None;
    let mut finish_reason = String::new();
    let mut saw_reasoning = false;
    let mut saw_tool_call = false;
    let mut chunks = std::pin::pin!(chunks);
    loop {
        // Stop reading when the turn was paused. The cancelled request
        // also closes the body, so this is just a fast path.
        if turn_cancel.is_cancelled() || parent_cancel.is_cancelled() || round_cancel.is_cancelled()
        {
            break;
        }
        let item = tokio::select! {
            _ = turn_cancel.cancelled() => None,
            _ = parent_cancel.cancelled() => None,
            _ = round_cancel.cancelled() => None,
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
            // Claude/Bedrock attach an opaque signature to each thinking
            // block. Keep the latest one so the assistant turn can be
            // replayed with it (reasoning preserved across turns).
            if !choice.delta.reasoning_signature.is_empty() {
                reasoning_signature = choice.delta.reasoning_signature.clone();
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
        reasoning_signature,
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

async fn save_turn_snapshot(state: &Arc<AppState>, sess: &Session, id: &str) -> bool {
    let snapshot = sess.clone();
    let title = fallback_title(sess);
    let id = id.to_string();
    state
        .store_call(move |store| store.update_turn_snapshot(&id, &snapshot, &title))
        .await
}

/// persistSession saves a session's messages. A generated LLM title is
/// kept; otherwise the first user message (truncated) is used so the
/// picker has a name immediately. Title generation runs in the background.
pub async fn persist_session(state: &Arc<AppState>, sess: &Session, id: &str) {
    if !save_turn_snapshot(state, sess, id).await {
        return;
    }
    state.subs.broadcast(id, &json!({"type": "saved"}));
    if !sess.title_generated {
        kickoff_title_generation(state, id.to_string());
    }
}

/// persistSessionNow saves a session's messages mid-turn without a
/// `saved` broadcast (that would trigger a racy full reload on viewing
/// clients while the stream is live) and without scheduling title
/// generation — the end-of-turn persist still kicks title generation
/// once. Viewing clients learn about the user message from the
/// dedicated `user_message` event broadcast at turn start.
async fn persist_session_now(state: &Arc<AppState>, sess: &Session, id: &str) {
    save_turn_snapshot(state, sess, id).await;
}

/// fallbackTitle derives the picker name from the first user message.
fn fallback_title(sess: &Session) -> String {
    if sess.title_generated {
        return String::new();
    }
    for m in &sess.messages {
        if m.role == "user" {
            return truncate_bytes_ellipsis(&m.content, 60);
        }
    }
    String::new()
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
        let load_id = id.clone();
        let Some(sess) = state.store_call(move |store| store.get(&load_id)).await else {
            return;
        };
        if sess.title_generated {
            return;
        }
        let Ok(title) =
            generate_title(&sess, None, &target.base_url, &target.key, &target.model).await
        else {
            return;
        };
        if title.is_empty() {
            return;
        }
        let update_id = id.clone();
        let stored_title = title.clone();
        let stored = state
            .store_call(move |store| {
                if store
                    .get(&update_id)
                    .is_none_or(|session| session.title_generated)
                {
                    return false;
                }
                store.update_title(&update_id, &stored_title);
                true
            })
            .await;
        if stored {
            state
                .subs
                .broadcast(&id, &json!({"type": "title", "text": title}));
        }
    });
}

/// finishPausedTurn ends a paused turn: it tells the client the stream
/// stopped, persists the partial conversation, and notifies other
/// viewers. A prompt that landed in the pending slot right before the
/// pause (Esc racing a mid-turn injection) is never dropped: it is run
/// as a follow-up turn once this one has fully unwound.
#[allow(clippy::too_many_arguments)]
async fn finish_paused_turn(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    ctx: &TurnCtx,
    parent_id: &str,
    opts: &TurnOpts,
    key: &str,
    base_url: &str,
    running: &mut Vec<PendingCmd>,
    done_rx: &mut mpsc::Receiver<(String, PendingExit)>,
) {
    // Commands still running when the turn pauses are killed with their
    // partial output recorded, exactly like an Esc while parked — every
    // pause path funnels through here, so none of them can leak a command
    // or drop its result. No-op when nothing is running.
    cancel_running(state, sess, out, id, running, done_rx).await;
    let ev = json!({"type": "paused"});
    emit(state, out, id, &ev).await;
    let leftover = ctx.handle.take_pending();
    persist_session(state, sess, id).await;
    end_of_turn(state, sess, id, &ctx.handle, parent_id).await;
    if leftover.is_empty() {
        return;
    }
    let message = leftover
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    let images = leftover.iter().flat_map(|m| m.images.clone()).collect();
    let follow = TurnOpts {
        message,
        thinking: opts.thinking.clone(),
        key: key.to_string(),
        base_url: base_url.to_string(),
        reasoning_field: opts.reasoning_field.clone(),
        turn_id: format!("{}-followup", ctx.handle.turn_id),
        images,
        compact: false,
        compact_instructions: String::new(),
        skip_append: false,
    };
    // Detached: the paused turn's stream is unwinding, but the follow-up
    // shares the same EventOut, so a still-connected client keeps
    // receiving its events on the same response body. The spawn goes
    // through a plain fn so the Send proof for this future doesn't
    // recurse into the follow-up's own future (a follow-up turn can
    // itself be paused).
    spawn_turn(
        state.clone(),
        sess.clone(),
        id.to_string(),
        follow,
        out.clone(),
        ctx.parent.clone(),
    );
}

/// Spawns one turn task on the shared EventOut. A plain fn boundary:
/// /send and the paused-turn follow-up both go through it, which keeps
/// the follow-up recursion (paused turn → follow-up turn) out of the
/// Send inference for run_session_turn's future.
pub fn spawn_turn(
    state: Arc<AppState>,
    mut sess: Session,
    id: String,
    opts: TurnOpts,
    out: EventOut,
    parent: CancelToken,
) {
    tokio::spawn(async move {
        run_session_turn_guarded(&state, &mut sess, &id, opts, out, parent).await;
    });
}

/// One bash command that is still running after the tool phase. The
/// turn parks on it; the result is recorded as the original tool call's
/// result when the command exits (or is killed by Esc).
struct PendingCmd {
    call_id: String,
    /// sess.messages index of the assistant tool_calls message that owns
    /// this call — its result belongs right after that message, before
    /// anything an interjected prompt added.
    after_idx: usize,
    command: String,
    started: Instant,
    /// Signalled when the turn stops waiting on this command (Esc).
    kill: CancelToken,
    /// sess.messages index of the placeholder pushed for this call while
    /// it runs (None until an interjection pushes one). The exact slot
    /// matters: record_pending_result replaces it in place, and a fold
    /// can shift indexes, so the entry is re-verified before use.
    placeholder_idx: Option<usize>,
    /// Live output written so far, for placeholder refresh.
    output: atom_sandbox::exec::PendingOutput,
}

/// How parking on running commands ended.
enum ParkOutcome {
    /// Every command exited; the real results are in history.
    AllDone,
    /// The user interjected: placeholders were pushed for the still-
    /// running commands, and the prompts + any compaction request are
    /// handed back for the next (placeholder) round.
    Prompt {
        injected: Vec<Message>,
        compact: Option<String>,
    },
    /// Esc while parked: commands killed, partial results recorded.
    Cancelled,
}

/// First free slot for a tool result of the assistant message at
/// `after_idx`: right after it, skipping tool results already recorded
/// there. Inserting every result at this slot keeps the provider
/// history valid (assistant(tool_calls) → tool(result) → …).
fn result_insert_pos(sess: &Session, after_idx: usize) -> usize {
    let mut pos = after_idx + 1;
    while sess.messages.get(pos).is_some_and(|m| m.role == "tool") {
        pos += 1;
    }
    pos
}

/// Live placeholder tool results for commands that are still running.
/// Called once per round before the request is built, so the model
/// always sees the current state of its running commands — elapsed
/// seconds and the output written so far — without any status tool. A
/// command without an entry yet (first interjection since it started)
/// gets one inserted; the real result replaces it on completion.
fn refresh_placeholders(sess: &mut Session, running: &mut [PendingCmd]) {
    for p in running {
        // One entry per call: a second interjection must not stack
        // another placeholder on top of the first — record_pending_result
        // replaces only one, and two tool results for one call id are out
        // of contract for several providers (Anthropic rejects outright).
        let slot = p
            .placeholder_idx
            .filter(|&i| {
                sess.messages
                    .get(i)
                    .is_some_and(|m| m.role == "tool" && m.tool_call_id == p.call_id)
            })
            .or_else(|| {
                sess.messages
                    .iter()
                    .position(|m| m.role == "tool" && m.tool_call_id == p.call_id)
            });
        let slot = match slot {
            Some(i) => i,
            None => {
                // Never inserted: only when the owning assistant message
                // still exists — if a fold removed the call itself, an
                // entry for it would be an orphan (and dead weight for
                // sanitize_messages).
                let Some(after_idx) = sess.messages.iter().position(|m| {
                    m.role == "assistant" && m.tool_calls.iter().any(|tc| tc.id == p.call_id)
                }) else {
                    continue;
                };
                let pos = result_insert_pos(sess, after_idx);
                sess.messages.insert(
                    pos,
                    Message {
                        role: "tool".into(),
                        tool_call_id: p.call_id.clone(),
                        ..Default::default()
                    },
                );
                p.placeholder_idx = Some(pos);
                pos
            }
        };
        sess.messages[slot].content = pending_placeholder_text(p);
    }
}

/// The placeholder text for one running command: elapsed time plus the
/// output written so far (tail-capped — the full output arrives when
/// the command exits and replaces this entry).
fn pending_placeholder_text(p: &PendingCmd) -> String {
    let secs = p.started.elapsed().as_secs();
    let mut text = format!(
        "[background command still running: {} — started {}s ago",
        p.command, secs
    );
    let so_far = p.output.text();
    let so_far = so_far.trim_end();
    if so_far.is_empty() {
        text.push_str("; no output yet");
    } else {
        let head_cut = so_far.chars().count().saturating_sub(600);
        let tail: String = if head_cut > 0 {
            format!("…{}", so_far.chars().skip(head_cut).collect::<String>())
        } else {
            so_far.to_string()
        };
        text.push_str("; output so far:\n");
        text.push_str(&tail);
    }
    text.push_str(
        "\nThis entry is refreshed automatically; its output replaces it \
         when the command exits. Do not sleep, poll, or re-run it to wait. \
         If the user asked something, answer them now in plain text.]",
    );
    text
}

/// Records a pending command's exit as the original tool call's result:
/// replaces the placeholder pushed for an interjected prompt, or
/// inserts right after the owning assistant message — before anything
/// the prompt added.
async fn record_pending_result(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    p: &PendingCmd,
    exit: PendingExit,
) {
    let mut content = if exit.killed {
        atom_tools::format_bash_cancelled(&exit)
    } else {
        atom_tools::format_bash_exit(exit.exit_code, &exit.output)
    };
    cap_tool_output(&mut content);
    // Slot resolution, most specific first: the exact entry the
    // placeholder occupied (verified — a fold can shift indexes), then
    // any other tool entry for this call, then re-anchor next to the
    // assistant message that owns the call. When even the owning call is
    // gone from history (folded away entirely), nothing is inserted: an
    // orphan result would be dead weight sanitize_messages drops anyway.
    let slot = p
        .placeholder_idx
        .filter(|&i| {
            sess.messages
                .get(i)
                .is_some_and(|m| m.role == "tool" && m.tool_call_id == p.call_id)
        })
        .or_else(|| {
            sess.messages
                .iter()
                .position(|m| m.role == "tool" && m.tool_call_id == p.call_id)
        })
        .or_else(|| {
            sess.messages
                .iter()
                .position(|m| {
                    m.role == "assistant" && m.tool_calls.iter().any(|tc| tc.id == p.call_id)
                })
                .map(|after_idx| result_insert_pos(sess, after_idx))
        });
    match slot {
        Some(i)
            if sess
                .messages
                .get(i)
                .is_some_and(|m| m.role == "tool" && m.tool_call_id == p.call_id) =>
        {
            sess.messages[i].content = content.clone();
        }
        Some(i) => {
            sess.messages.insert(
                i,
                Message {
                    role: "tool".into(),
                    tool_call_id: p.call_id.clone(),
                    content: content.clone(),
                    ..Default::default()
                },
            );
        }
        None => {}
    }
    persist_session_now(state, sess, id).await;
    let ev = event(vec![
        ("type", json!("tool_result")),
        ("text", json!(content)),
        ("tool_provider", json!("")),
        ("call_id", json!(p.call_id)),
    ]);
    emit(state, out, id, &ev).await;
}

/// Esc unwind with commands still running: kill them all (waiters also
/// watch the turn token) and record their partial output before the
/// turn finishes.
async fn cancel_running(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    running: &mut Vec<PendingCmd>,
    done_rx: &mut mpsc::Receiver<(String, PendingExit)>,
) {
    for p in running.iter() {
        p.kill.cancel();
    }
    while !running.is_empty() {
        match tokio::time::timeout(Duration::from_secs(10), done_rx.recv()).await {
            Ok(Some((call_id, exit))) => {
                if let Some(pos) = running.iter().position(|p| p.call_id == call_id) {
                    let p = running.remove(pos);
                    record_pending_result(state, sess, out, id, &p, exit).await;
                }
            }
            _ => break, // drain timed out or the channel closed
        }
    }
    running.clear();
}

/// Parks the turn on the running commands: selects over their exits,
/// a mid-turn prompt (the pending-message slot + round cancel), and
/// the turn cancel (Esc kills the commands' process groups). There is
/// no timeout — a test suite may run for minutes; runaway protection
/// is the user's Esc.
#[allow(clippy::too_many_arguments)]
async fn park_for_pending(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    ctx: &TurnCtx,
    running: &mut Vec<PendingCmd>,
    done_rx: &mut mpsc::Receiver<(String, PendingExit)>,
) -> ParkOutcome {
    // A parked turn is interruptible by a mid-turn prompt exactly like
    // a model round is: register a round-cancel token so /send's
    // cancel_round wakes the select below.
    let park_cancel = CancelToken::new();
    ctx.handle.set_round_cancel(Some(park_cancel.clone()));
    let outcome = loop {
        // Completions that landed while the turn was streaming a round
        // are already buffered in the channel: record them first.
        while let Ok((call_id, exit)) = done_rx.try_recv() {
            if let Some(pos) = running.iter().position(|p| p.call_id == call_id) {
                let p = running.remove(pos);
                record_pending_result(state, sess, out, id, &p, exit).await;
            }
        }
        if running.is_empty() {
            break ParkOutcome::AllDone;
        }
        // A prompt (or compaction) queued while the turn was busy?
        let injected = ctx.handle.take_pending();
        let compact = ctx.handle.take_compact();
        if !injected.is_empty() || compact.is_some() {
            // No placeholders needed here: the next round top refreshes
            // every running command before the request is built.
            break ParkOutcome::Prompt { injected, compact };
        }
        let turn_cancel = ctx.handle.cancel_token();
        tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => {
                // Esc while parked: kill every pending command's process
                // group and record the partial output as its tool result.
                cancel_running(state, sess, out, id, running, done_rx).await;
                break ParkOutcome::Cancelled;
            }
            _ = ctx.parent.cancelled() => {
                cancel_running(state, sess, out, id, running, done_rx).await;
                break ParkOutcome::Cancelled;
            }
            _ = park_cancel.cancelled() => {
                let injected = ctx.handle.take_pending();
                let compact = ctx.handle.take_compact();
                if injected.is_empty() && compact.is_none() {
                    // Spurious wake (raced with a completion): keep parking.
                    continue;
                }
                break ParkOutcome::Prompt { injected, compact };
            }
            Some((call_id, exit)) = done_rx.recv() => {
                if let Some(pos) = running.iter().position(|p| p.call_id == call_id) {
                    let p = running.remove(pos);
                    record_pending_result(state, sess, out, id, &p, exit).await;
                    if running.is_empty() {
                        break ParkOutcome::AllDone;
                    }
                }
            }
        }
    };
    ctx.handle.set_round_cancel(None);
    outcome
}

/// Handles a [`ParkOutcome::Prompt`]: folds if compaction was queued,
/// appends the injected prompts (the placeholders were already pushed),
/// and persists.
async fn after_park_prompt(
    state: &Arc<AppState>,
    sess: &mut Session,
    out: &EventOut,
    id: &str,
    injected: Vec<Message>,
    compact: Option<String>,
) {
    if let Some(extra) = compact {
        if let Err(ferr) = fold_session(state, sess, out, id, &extra).await {
            emit(state, out, id, &fold_error_event(&ferr)).await;
        }
    }
    for m in injected {
        state
            .subs
            .broadcast(id, &json!({"type": "user_message", "text": m.content}));
        sess.messages.push(m);
    }
    persist_session_now(state, sess, id).await;
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

async fn await_round<F, T>(
    future: F,
    turn_cancel: &CancelToken,
    parent_cancel: &CancelToken,
    round_cancel: &CancelToken,
) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::select! {
        _ = turn_cancel.cancelled() => None,
        _ = parent_cancel.cancelled() => None,
        _ = round_cancel.cancelled() => None,
        output = future => Some(output),
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

/// runSessionTurnGuarded wraps run_session_turn so a turn task that dies
/// mid-flight (a panic, most commonly) can never wedge the session: its
/// turn-table registration — or the pre-start reservation — would
/// otherwise stay behind, and every later /send would be rejected with
/// 409 "session already has an active turn" until the server restarted.
/// On the healthy path end_turn already deregistered the turn and
/// force_end_session_turns is a no-op.
pub async fn run_session_turn_guarded(
    state: &Arc<AppState>,
    sess: &mut Session,
    id: &str,
    opts: TurnOpts,
    out: EventOut,
    parent: CancelToken,
) {
    let res = AssertUnwindSafe(run_session_turn(state, sess, id, opts, out, parent))
        .catch_unwind()
        .await;
    if res.is_err() {
        eprintln!("atoms: turn task for session {id} panicked; clearing active-turn state");
        // The turn died without reaching its own unwind paths: cancel the
        // handle so background-command waiters select their kill branch
        // instead of shepherding an orphan whose result can no longer be
        // recorded.
        state.turns.cancel_session_turns(id);
    }
    state.turns.force_end_session_turns(id);
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
            created_at: Some(Utc::now()),
            ..Default::default()
        });
        // Persist the session log now so the user message is on the
        // server before generation starts, and tell viewing clients to
        // show it right away (a dedicated event appends the user block
        // without forcing a reload that would wipe the live stream).
        // No `saved` broadcast or title-gen kick here — the end-of-turn
        // persist handles both, once.
        persist_session_now(state, sess, id).await;
        if !opts.message.is_empty() {
            state
                .subs
                .broadcast(id, &json!({"type": "user_message", "text": opts.message}));
        }
    }

    let handle = state.turns.start_turn(id, &opts.turn_id);
    let ctx = TurnCtx { handle, parent };
    let parent_id = sess.parent_id.clone();
    if !parent_id.is_empty() {
        let child_id = id.to_string();
        state
            .store_call(move |store| {
                store.update_delegate_status(
                    &child_id,
                    atom_core::session::store::DelegateStatus::Working,
                )
            })
            .await;
        state
            .subs
            .broadcast(&parent_id, &json!({"type": "children"}));
    }

    let cwd = PathBuf::from(sess.cwd.clone());
    let mut tools = atom_tools::tool_definitions_with_mcp(&cwd).await;
    // Deferred MCP catalogs surface a single search entry point instead
    // of flooding the context with every server tool.
    if atom_tools::has_deferred_tools(&cwd).await {
        tools.push(atom_tools::defs::find_tool_def());
    }
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
        persist_session(state, sess, id).await;
        let done = done_event(turn_duration_ms(started_at), "");
        emit(state, &out, id, &done).await;
        end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
        return;
    }

    let mut finished = false;
    let mut nudge_attempt: usize = 0;
    // Commands still running from the tool phase: the turn parks on
    // them instead of looping, and their results are recorded into
    // history when they exit (see PendingCmd / park_for_pending).
    let mut running: Vec<PendingCmd> = Vec::new();
    let (done_tx, mut done_rx) = mpsc::channel::<(String, PendingExit)>(16);
    let mut empty_response_attempt: usize = 0;
    // The "current time" system note is captured once per turn, not per
    // round: it is folded into the system prefix of every request, and a
    // per-minute regeneration would rewrite that prefix each round,
    // invalidating the provider's prompt cache for the whole conversation.
    let now_note = Local::now().format("%D,%H:%M").to_string();
    'rounds: for _round in 0..MAX_TOOL_ROUNDS {
        // Stop immediately when the turn was paused.
        if ctx.err() {
            finish_paused_turn(
                state,
                sess,
                &out,
                id,
                &ctx,
                &parent_id,
                &opts,
                &key,
                &base_url,
                &mut running,
                &mut done_rx,
            )
            .await;
            return;
        }

        // Prompts injected while the previous round's tools were
        // running (or between rounds) land here: fold them into
        // history so the model sees them this round. Nothing is
        // paused or killed by an injection — this drain is what makes
        // a mid-turn prompt reach the model at the next round.
        let injected = ctx.handle.take_pending();
        if !injected.is_empty() {
            for m in injected {
                state
                    .subs
                    .broadcast(id, &json!({"type": "user_message", "text": m.content}));
                sess.messages.push(m);
            }
            persist_session_now(state, sess, id).await;
        }

        let forced = ctx.handle.take_compact();
        // Auto-compact threshold: the smaller of the fixed threshold and
        // the session model's window minus headroom, so small-context
        // models fold before the provider starts rejecting requests.
        // The settings toggle can disable auto-compaction entirely; an
        // explicitly requested compact still runs.
        let auto_compaction = atom_core::config::load()
            .resolved_compaction()
            .resolved_enabled();
        let provider = provider_name_for_url(&base_url);
        let auto_threshold = compaction_threshold(context_window_tokens(&provider, &sess.model));
        let mut fold_now = forced.is_some()
            || (auto_compaction
                && !compact_failed
                && should_compact_with_threshold(sess, auto_threshold));
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

        // Live status for running commands: elapsed time and output so
        // far, written into each command's placeholder entry right before
        // the request is built. This is how the model "checks on its
        // tool" — no status tool, the state is simply in the history it
        // is about to see. Runs after any fold so entries are placed
        // against post-fold history.
        if !running.is_empty() {
            refresh_placeholders(sess, &mut running);
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
                content: now_note.clone(),
                ..Default::default()
            },
        );
        // A text-only model rejects any request carrying an image with a
        // 400 ("this model does not support image input"). When the
        // models.dev catalog explicitly lists the selected model as
        // text-only, drop attached images so the turn still goes through.
        // Models the catalog does not know keep their images (a custom
        // model may be multimodal even if it is not catalogued). Only the
        // outgoing request is affected; the session log still keeps the
        // images for display and replay.
        if matches!(
            model_supports_image_input(&provider, &sess.model),
            Some(false)
        ) {
            strip_images_for_text_only_model(&mut msgs);
        }
        let reasoning_field = if !opts.reasoning_field.is_empty() {
            opts.reasoning_field.clone()
        } else {
            reasoning_field_for_url(&base_url)
        };
        let round_cancel = CancelToken::new();
        let turn_cancel = ctx.handle.cancel_token();
        ctx.handle.set_round_cancel(Some(round_cancel.clone()));

        let result: StreamResult = if bedrock_style_for_url(&base_url) {
            // Bedrock Converse: bearer-token auth, per-request regional
            // endpoint, binary eventstream response. Chunks come back in
            // the shared OpenAI-delta shape, so the relay below is
            // unchanged.
            let opened = await_round(
                stream_bedrock(&base_url, &key, &sess.model, &msgs, &tools, &opts.thinking),
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
            )
            .await;
            let chunks = match opened {
                None => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                    }
                    continue 'rounds;
                }
                Some(result) => match result {
                    Ok(c) => c,
                    Err(err) => {
                        ctx.handle.set_round_cancel(None);
                        if ctx.err() {
                            finish_paused_turn(
                                state,
                                sess,
                                &out,
                                id,
                                &ctx,
                                &parent_id,
                                &opts,
                                &key,
                                &base_url,
                                &mut running,
                                &mut done_rx,
                            )
                            .await;
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
                        // A dead turn must not leak its background commands: kill them and
                        // record their partial output as the tool results they own.
                        cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                        end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                        return;
                    }
                },
            };
            let r = stream_model_to_client(
                state,
                &out,
                id,
                chunks,
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                    return;
                }
            }
        } else if anthropic_style_for_url(&base_url) {
            // Anthropic Messages dialect: first-party api.anthropic.com
            // plus gateways exposing an /anthropic/ path (MiniMax's
            // /anthropic/v1 mirror). Routing these to stream_chat would
            // POST {base}/chat/completions — a path the Messages-only
            // mirrors answer with a plain-text "404 page not found".
            // stream_anthropic POSTs {base}/messages and translates the
            // SSE events into the shared OpenAI-delta shape, so the
            // relay below is unchanged.
            let opened = await_round(
                stream_anthropic(&base_url, &key, &sess.model, &msgs, &tools, &opts.thinking),
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
            )
            .await;
            let chunks = match opened {
                None => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                    }
                    continue 'rounds;
                }
                Some(result) => match result {
                    Ok(c) => c,
                    Err(err) => {
                        ctx.handle.set_round_cancel(None);
                        if ctx.err() {
                            finish_paused_turn(
                                state,
                                sess,
                                &out,
                                id,
                                &ctx,
                                &parent_id,
                                &opts,
                                &key,
                                &base_url,
                                &mut running,
                                &mut done_rx,
                            )
                            .await;
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
                        // A dead turn must not leak its background commands: kill them and
                        // record their partial output as the tool results they own.
                        cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                        end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                        return;
                    }
                },
            };
            let r = stream_model_to_client(
                state,
                &out,
                id,
                chunks,
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                    return;
                }
            }
        } else if api_protocol_for(&provider, &sess.model) == APIProtocol::OpenAIResponses
            // ChatGPT-Plan OAuth tokens issued by the openai oauth
            // flow (openai_oauth.rs) only carry ChatGPT-Plan scopes
            // (openid/profile/email/offline_access) and are rejected
            // by api.openai.com/v1/responses with "Missing scopes:
            // api.responses.write". They are authorized for the
            // chatgpt.com/backend-api/codex/responses endpoint, so
            // route them through the codex branch below instead.
            // Real OPENAI_API_KEYs and any other token that doesn't
            // match a stored ChatGPT oauth entry keep going to the
            // Responses API (muse-spark on opencode Zen, etc.).
            && openai_codex_auth_for_key(&key).is_none()
        {
            let opened = await_round(
                stream_responses(&base_url, &key, &sess.model, &msgs, &tools, &opts.thinking),
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
            )
            .await;
            let chunks = match opened {
                None => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                    }
                    continue 'rounds;
                }
                Some(result) => match result {
                    Ok(c) => c,
                    Err(err) => {
                        ctx.handle.set_round_cancel(None);
                        if ctx.err() {
                            finish_paused_turn(
                                state,
                                sess,
                                &out,
                                id,
                                &ctx,
                                &parent_id,
                                &opts,
                                &key,
                                &base_url,
                                &mut running,
                                &mut done_rx,
                            )
                            .await;
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
                        // A dead turn must not leak its background commands: kill them and
                        // record their partial output as the tool results they own.
                        cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                        end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                        return;
                    }
                },
            };
            let r = stream_model_to_client(
                state,
                &out,
                id,
                chunks,
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
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
                        // A dead turn must not leak its background commands: kill them and
                        // record their partial output as the tool results they own.
                        cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                        end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                        return;
                    }
                };
            let opened = await_round(
                do_openai_codex_round(&key, &codex_body),
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
            )
            .await;
            match opened {
                None => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                    }
                    continue 'rounds;
                }
                Some(Ok(outcome)) => {
                    key = outcome.key;
                    for ev in &outcome.events {
                        emit(state, &out, id, ev).await;
                    }
                    outcome.result
                }
                Some(Err(round_err)) => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
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

            let opened = await_round(
                stream_chat(&base_url, &key, req_body, &reasoning_field),
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
            )
            .await;
            let chunks = match opened {
                None => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                    if let Some(extra) = ctx.handle.take_compact() {
                        if let Err(ferr) = fold_session(state, sess, &out, id, &extra).await {
                            emit(state, &out, id, &fold_error_event(&ferr)).await;
                        }
                    }
                    continue 'rounds;
                }
                Some(Ok(c)) => c,
                Some(Err(err)) => {
                    ctx.handle.set_round_cancel(None);
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
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
                &turn_cancel,
                &ctx.parent,
                &round_cancel,
                &reasoning_field,
            )
            .await;
            ctx.handle.set_round_cancel(None);
            match r {
                Ok(result) => result,
                Err(err) => {
                    if ctx.err() {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
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
                    // A dead turn must not leak its background commands: kill them and
                    // record their partial output as the tool results they own.
                    cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
                    return;
                }
            }
        };

        if empty_response(&result) {
            if empty_response_attempt < atom_core::providers::MAX_EMPTY_RESPONSE_RETRIES {
                let delays = atom_core::providers::retry::empty_response_retry_delays();
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
                    finish_paused_turn(
                        state,
                        sess,
                        &out,
                        id,
                        &ctx,
                        &parent_id,
                        &opts,
                        &key,
                        &base_url,
                        &mut running,
                        &mut done_rx,
                    )
                    .await;
                    return;
                }
                continue 'rounds;
            }
            let msg = format!(
                "provider returned an empty response after {} attempts \
                 (some free tiers, like OpenCode Zen, rate-limit by silently \
                 returning empty streams; wait a moment and retry)",
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
            // A dead turn must not leak its background commands: kill them and
            // record their partial output as the tool results they own.
            cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
            end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
            return;
        }
        empty_response_attempt = 0;

        // A prompt injected mid-stream cancelled this round (round
        // cancel only — the turn and any tools are untouched). Flush
        // the partial reply and the queued message into history, then
        // keep going: the model sees the prompt next round.
        if round_cancel.is_cancelled() {
            let pending = ctx.handle.take_pending();
            if !pending.is_empty() {
                if !result.content.is_empty() || !result.reasoning.is_empty() {
                    sess.messages
                        .push(assistant_message(&sess.model, &base_url, &result, None));
                }
                for m in pending {
                    state
                        .subs
                        .broadcast(id, &json!({"type": "user_message", "text": m.content}));
                    sess.messages.push(m);
                }
                persist_session_now(state, sess, id).await;
                continue 'rounds;
            }
        }

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
            finish_paused_turn(
                state,
                sess,
                &out,
                id,
                &ctx,
                &parent_id,
                &opts,
                &key,
                &base_url,
                &mut running,
                &mut done_rx,
            )
            .await;
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
                    finish_paused_turn(
                        state,
                        sess,
                        &out,
                        id,
                        &ctx,
                        &parent_id,
                        &opts,
                        &key,
                        &base_url,
                        &mut running,
                        &mut done_rx,
                    )
                    .await;
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
            // A prompt that arrived while the final answer streamed
            // must not end the turn unanswered: keep the answer, fold
            // the prompt into history, and answer it next round.
            let pending = ctx.handle.take_pending();
            if !pending.is_empty() {
                for m in pending {
                    state
                        .subs
                        .broadcast(id, &json!({"type": "user_message", "text": m.content}));
                    sess.messages.push(m);
                }
                persist_session_now(state, sess, id).await;
                continue 'rounds;
            }
            // Commands are still running: the spoken answer stands, but
            // the turn parks until they exit — their results land in
            // history and the model reports next round.
            if !running.is_empty() {
                match park_for_pending(state, sess, &out, id, &ctx, &mut running, &mut done_rx)
                    .await
                {
                    ParkOutcome::AllDone => {}
                    ParkOutcome::Prompt { injected, compact } => {
                        after_park_prompt(state, sess, &out, id, injected, compact).await;
                    }
                    ParkOutcome::Cancelled => {
                        finish_paused_turn(
                            state,
                            sess,
                            &out,
                            id,
                            &ctx,
                            &parent_id,
                            &opts,
                            &key,
                            &base_url,
                            &mut running,
                            &mut done_rx,
                        )
                        .await;
                        return;
                    }
                }
                continue 'rounds;
            }
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
        let assistant_idx = sess.messages.len() - 1;
        persist_session_now(state, sess, id).await;

        // Execute each tool and feed the result back to the model.
        for (tool_index, tc) in result.tool_calls.iter().enumerate() {
            let ev = event(vec![
                ("type", json!("tool")),
                ("name", json!(tc.function.name)),
                ("arguments", json!(tc.function.arguments)),
                ("call_id", json!(tc.id.clone())),
            ]);
            emit(state, &out, id, &ev).await;

            let approver = crate::dispatch::ServerApprover::for_turn(
                state.clone(),
                id.to_string(),
                out.clone(),
                Some(ctx.handle.cancel_token()),
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

            // A bash command: the tool call returns immediately with the
            // running process. The turn parks below — the model says
            // nothing more until the command finishes or the user
            // interjects — and the result is recorded as this call's
            // result when it exits. The TUI block spins until then.
            if let Some(proc) = outcome.pending.take().map(|b| *b) {
                let kill = CancelToken::new();
                let call_id = tc.id.clone();
                running.push(PendingCmd {
                    call_id: call_id.clone(),
                    after_idx: assistant_idx,
                    command: proc.command().to_string(),
                    started: proc.started(),
                    kill: kill.clone(),
                    placeholder_idx: None,
                    output: proc.output_handle(),
                });
                let tx = done_tx.clone();
                let turn_cancel = ctx.handle.cancel_token();
                let parent_cancel = ctx.parent.clone();
                tokio::spawn(async move {
                    let mut proc = proc;
                    tokio::select! {
                        biased;
                        _ = kill.cancelled() => proc.kill().await,
                        _ = turn_cancel.cancelled() => proc.kill().await,
                        _ = parent_cancel.cancelled() => proc.kill().await,
                        _ = proc.wait_exit() => {}
                    }
                    let exit = proc.collect().await;
                    let _ = tx.send((call_id, exit)).await;
                });
                continue;
            }

            sess.messages.push(Message {
                role: "tool".into(),
                tool_call_id: tc.id.clone(),
                content: outcome.text.clone(),
                images: outcome.images.clone(),
                diff: outcome.diff.clone(),
                tool_provider: outcome.tool_provider.clone(),
                ..Default::default()
            });
            // Save each tool result immediately. Besides crash safety, this
            // makes an in-progress blocking dispatch visible after navigation.
            persist_session_now(state, sess, id).await;
            let result_ev = event(vec![
                ("type", json!("tool_result")),
                ("text", json!(outcome.text)),
                ("tool_provider", json!(outcome.tool_provider)),
                ("call_id", json!(tc.id.clone())),
            ]);
            emit(state, &out, id, &result_ev).await;
            // Send any file diff as its own event so the client can attach
            // it to the tool block it already rendered.
            if !outcome.diff.is_empty() {
                let diff_ev = event(vec![
                    ("type", json!("tool_diff")),
                    ("diff", json!(outcome.diff)),
                    ("call_id", json!(tc.id.clone())),
                ]);
                emit(state, &out, id, &diff_ev).await;
            }
            // Stop between tools when the turn was paused.
            if ctx.err() {
                // Commands still running (this or earlier rounds) are
                // killed with their partial output recorded.
                cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
                for skipped in &result.tool_calls[tool_index + 1..] {
                    sess.messages.push(Message {
                        role: "tool".into(),
                        tool_call_id: skipped.id.clone(),
                        content: "error: tool execution cancelled before it started".into(),
                        ..Default::default()
                    });
                }
                finish_paused_turn(
                    state,
                    sess,
                    &out,
                    id,
                    &ctx,
                    &parent_id,
                    &opts,
                    &key,
                    &base_url,
                    &mut running,
                    &mut done_rx,
                )
                .await;
                return;
            }
        }

        // Commands are still running: park. The turn waits here — no
        // further model rounds — until they exit (the real results then
        // land in history and the model reports next round) or the user
        // interjects (the prompt is answered against placeholder
        // results without stopping the commands).
        if !running.is_empty() {
            match park_for_pending(state, sess, &out, id, &ctx, &mut running, &mut done_rx).await {
                ParkOutcome::AllDone => {}
                ParkOutcome::Prompt { injected, compact } => {
                    after_park_prompt(state, sess, &out, id, injected, compact).await;
                }
                ParkOutcome::Cancelled => {
                    finish_paused_turn(
                        state,
                        sess,
                        &out,
                        id,
                        &ctx,
                        &parent_id,
                        &opts,
                        &key,
                        &base_url,
                        &mut running,
                        &mut done_rx,
                    )
                    .await;
                    return;
                }
            }
        }
        // Loop again: the model now sees the tool results.
    }

    if !finished {
        // Exhausted the runaway guard while the model was still calling
        // tools. Persist the partial turn and tell the client, instead of
        // going silent with unfinished work. Commands still running are
        // killed with their partial output recorded.
        cancel_running(state, sess, &out, id, &mut running, &mut done_rx).await;
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
    }

    persist_session(state, sess, id).await;
    let done = done_event(
        turn_duration_ms(started_at),
        if finished { &sess.model } else { "" },
    );
    emit(state, &out, id, &done).await;
    end_of_turn(state, sess, id, &ctx.handle, &parent_id).await;
}

/// Shared tail of run_session_turn: deregister the turn and notify the
/// parent session's viewers (Go's deferred endTurn/children broadcast).
async fn end_of_turn(
    state: &Arc<AppState>,
    sess: &Session,
    id: &str,
    handle: &Arc<TurnHandle>,
    parent_id: &str,
) {
    state.turns.end_turn(id, handle);
    // A user-initiated stop (Esc via the pause route) ends the child's
    // turn: record a non-error transcript marker so the parent's dispatch
    // learns "stopped by the user", and a Stopped status instead of
    // deriving Done/Error from the last assistant/error message.
    let user_stopped = state.take_user_stop(id);
    if user_stopped && !parent_id.is_empty() {
        let stored_id = id.to_string();
        state
            .store_call(move |store| {
                if let Some(mut stored) = store.get(&stored_id) {
                    stored.messages.push(Message {
                        role: "stopped".into(),
                        content: "stopped by the user".into(),
                        ..Default::default()
                    });
                    store.update_turn_snapshot(&stored_id, &stored, "");
                }
            })
            .await;
    }
    if !parent_id.is_empty() {
        let status = if user_stopped {
            atom_core::session::store::DelegateStatus::Stopped
        } else if sess.cancelled {
            atom_core::session::store::DelegateStatus::Cancelled
        } else if sess
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
        let child_id = id.to_string();
        state
            .store_call(move |store| store.update_delegate_status(&child_id, status))
            .await;
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
    use atom_core::types::FunctionCall;
    use atom_sandbox::exec::PendingOutput;

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

    /// A second mid-turn interjection while the same command runs must
    /// not stack a second placeholder: record_pending_result replaces
    /// only one entry, duplicates are out of contract for several
    /// providers, and a stale "still running" copy would survive the real
    /// result forever.
    #[test]
    fn placeholders_do_not_stack_on_repeated_interjections() {
        let mut sess = Session::default();
        sess.messages.push(Message {
            role: "assistant".into(),
            tool_calls: vec![
                ToolCall {
                    id: "call1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: r#"{"command":"cargo test"}"#.into(),
                    },
                },
                ToolCall {
                    id: "call2".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: r#"{"command":"sleep 60"}"#.into(),
                    },
                },
            ],
            ..Default::default()
        });
        let output = PendingOutput::default();
        output.extend(true, b"running 45 tests...");
        let mut running = vec![PendingCmd {
            call_id: "call1".into(),
            after_idx: 0,
            command: "cargo test".into(),
            started: Instant::now(),
            kill: CancelToken::new(),
            placeholder_idx: None,
            output: output.clone(),
        }];

        refresh_placeholders(&mut sess, &mut running);
        refresh_placeholders(&mut sess, &mut running);

        let count = sess
            .messages
            .iter()
            .filter(|m| m.role == "tool" && m.tool_call_id == "call1")
            .count();
        assert_eq!(count, 1, "placeholders stacked: {:#?}", sess.messages);
        assert_eq!(running[0].placeholder_idx, Some(1));
        assert!(
            sess.messages[1].content.contains("running 45 tests..."),
            "live output missing: {:?}",
            sess.messages[1].content
        );
        assert!(sess.messages[1].content.contains("Do not sleep, poll"));
        // A command with no output yet reads differently.
        let mut quiet = vec![PendingCmd {
            call_id: "call2".into(),
            after_idx: 0,
            command: "sleep 60".into(),
            started: Instant::now(),
            kill: CancelToken::new(),
            placeholder_idx: None,
            output: PendingOutput::default(),
        }];
        refresh_placeholders(&mut sess, &mut quiet);
        assert!(sess.messages[2].content.contains("no output yet"));
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

    /// A router that streams object-form arguments may send an empty
    /// placeholder object first and the real object in a later delta
    /// reusing the index. The placeholder must be overwritten, keeping
    /// one call, instead of opening a bogus second call.
    #[test]
    fn accumulator_empty_object_placeholder() {
        let mut acc = ToolCallAccumulator::new();
        acc.add(&delta(0, "call_a", "grep", "{}"));
        acc.add(&delta(0, "", "", r#"{"pattern":"version"}"#));
        let calls = acc.list();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "grep");
        assert_eq!(calls[0].function.arguments, r#"{"pattern":"version"}"#);
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

    #[test]
    fn strip_images_for_text_only_model_drops_images() {
        let img = || atom_core::types::ImageData {
            mime: "image/png".into(),
            data: "AAAA".into(),
        };
        let mut msgs = vec![
            // text + image: image dropped, text kept.
            Message {
                role: "user".into(),
                content: "look".into(),
                images: vec![img()],
                ..Default::default()
            },
            // image only: becomes a placeholder so content isn't empty.
            Message {
                role: "user".into(),
                content: "".into(),
                images: vec![img()],
                ..Default::default()
            },
            // no images: untouched.
            Message {
                role: "assistant".into(),
                content: "hi".into(),
                ..Default::default()
            },
        ];
        strip_images_for_text_only_model(&mut msgs);
        assert!(msgs[0].images.is_empty());
        assert_eq!(msgs[0].content, "look");
        assert!(msgs[1].images.is_empty());
        assert_eq!(msgs[1].content, "[image attached]");
        assert!(msgs[2].images.is_empty());
        assert_eq!(msgs[2].content, "hi");
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
            &CancelToken::new(),
            "reasoning",
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "connection reset");
    }

    #[tokio::test]
    async fn provider_handshake_wait_observes_round_cancellation() {
        let turn = CancelToken::new();
        let parent = CancelToken::new();
        let round = CancelToken::new();
        let cancel = round.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            await_round(std::future::pending::<()>(), &turn, &parent, &round),
        )
        .await
        .expect("provider handshake ignored cancellation");
        assert!(result.is_none());
    }

    /// Esc on a live subagent turn: the user stop records a non-error
    /// "stopped" marker and Stopped status — never "done", never an error.
    #[tokio::test]
    async fn user_stop_marks_child_stopped_with_marker() {
        use crate::state::ConnTracker;
        use atom_sandbox::policy::SandboxConfig;
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(atom_core::session::store::SessionStore::open_in_dir(dir.path()).unwrap());
        let state = Arc::new(AppState::new(
            store.clone(),
            SandboxConfig {
                ..Default::default()
            },
            Arc::new(ConnTracker::new()),
        ));
        let parent = store.create("m", "/tmp", vec![]);
        let child = store.create_child(&parent.id, "m", "/tmp", "low", "child", vec![]);
        // The pause path persisted the partial reply before the turn ended.
        let mut partial = store.get(&child.id).unwrap();
        partial.messages.push(Message {
            role: "assistant".into(),
            content: "partial work".into(),
            model: "m".into(),
            ..Default::default()
        });
        store.update_turn_snapshot(&child.id, &partial, "");

        let handle = state.turns.start_turn(&child.id, "t1");
        state.mark_user_stop(&child.id);
        let sess = store.get(&child.id).unwrap();
        end_of_turn(&state, &sess, &child.id, &handle, &parent.id).await;

        let stored = store.get(&child.id).unwrap();
        let last = stored.messages.last().unwrap();
        assert_eq!(last.role, "stopped");
        assert_eq!(last.content, "stopped by the user");
        assert!(!stored.messages.iter().any(|m| m.role == "error"));
        assert_eq!(
            store.get_info(&child.id).unwrap().status,
            atom_core::session::store::DelegateStatus::Stopped
        );
        assert!(!state.take_user_stop(&child.id), "flag consumed");

        // Without a stop flag the same end derives its usual status.
        let sibling = store.create_child(&parent.id, "m", "/tmp", "low", "sibling", vec![]);
        let handle2 = state.turns.start_turn(&sibling.id, "t2");
        let sess2 = store.get(&sibling.id).unwrap();
        end_of_turn(&state, &sess2, &sibling.id, &handle2, &parent.id).await;
        assert_eq!(
            store.get_info(&sibling.id).unwrap().status,
            atom_core::session::store::DelegateStatus::Done
        );
    }

    /// Isolates the atom data dir for one test (XDG_DATA_HOME is
    /// process-global, so a mutex guards concurrent runs). Mirrors
    /// crates/atom-server/tests/integration.rs.
    struct DispatchTestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_xdg: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl DispatchTestEnv {
        fn new() -> Self {
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::TempDir::new().expect("tempdir");
            let prev_xdg = std::env::var_os("XDG_DATA_HOME");
            std::env::set_var("XDG_DATA_HOME", dir.path());
            DispatchTestEnv {
                _lock: lock,
                prev_xdg,
                _dir: dir,
            }
        }
    }

    impl Drop for DispatchTestEnv {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    /// Regression: v0.1.2 added an OpenAIResponses dispatch branch
    /// ahead of the chatgpt-backend (codex) branch, but the Responses
    /// branch had no guard against ChatGPT-Plan OAuth tokens. Those
    /// tokens (issued by atom's openai oauth flow with scopes
    /// "openid profile email offline_access") are only authorized for
    /// chatgpt.com/backend-api/codex/responses — not
    /// api.openai.com/v1/responses, which requires the
    /// `api.responses.write` scope. Without the guard, every ChatGPT
    /// subscriber running an openai/gpt-5+ model hit the upstream 401
    /// "Missing scopes: api.responses.write". The Responses branch must
    /// yield to the codex branch whenever the bearer resolves to a
    /// stored ChatGPT-Plan OAuth entry.
    #[test]
    fn dispatch_skips_responses_branch_for_chatgpt_oauth() {
        let _env = DispatchTestEnv::new();

        // The test env has no models.dev catalog on disk; inject a
        // minimal one so api_protocol_for resolves npm correctly.
        // Without this, every model falls through to ChatCompletions
        // and the regression isn't observable.
        let catalog: atom_core::providers::ModelsDevCatalog = std::collections::HashMap::from([(
            "openai".into(),
            atom_core::providers::ModelsDevProvider {
                name: "openai".into(),
                npm: "@ai-sdk/openai".into(),
                api: "https://api.openai.com/v1".into(),
                env: vec!["OPENAI_API_KEY".into()],
                doc: String::new(),
                models: [(
                    "gpt-5".into(),
                    atom_core::providers::ModelsDevModel {
                        reasoning: false,
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            },
        )]);
        atom_core::providers::set_models_dev_catalog_for_test(Some(catalog));

        let bearer = "codex-bearer-xyz";
        atom_core::providers::auth::set_auth(
            "openai",
            atom_core::providers::AuthEntry {
                r#type: "oauth".into(),
                access: bearer.into(),
                refresh: "refresh-token".into(),
                expires: i64::MAX, // not expired
                ..Default::default()
            },
        )
        .unwrap();

        // The codex branch is reachable for this key.
        assert!(
            openai_codex_auth_for_key(bearer).is_some(),
            "ChatGPT-Plan OAuth entry should resolve via openai_codex_auth_for_key"
        );

        // The model picked (gpt-5) is otherwise labelled as
        // Responses-API (npm = "@ai-sdk/openai") by models.dev, which
        // is what used to route the bearer to
        // api.openai.com/v1/responses before the guard was added.
        assert_eq!(
            api_protocol_for("openai", "gpt-5"),
            atom_core::providers::APIProtocol::OpenAIResponses,
            "gpt-5 should still resolve to OpenAIResponses for non-oauth auth"
        );

        // Mirror the dispatch predicate at turn.rs: the Responses
        // branch must be skipped for this key, so the codex branch
        // handles it.
        let responses_branch_taken = api_protocol_for("openai", "gpt-5")
            == atom_core::providers::APIProtocol::OpenAIResponses
            && openai_codex_auth_for_key(bearer).is_none();
        assert!(
            !responses_branch_taken,
            "ChatGPT-Plan OAuth must not enter the Responses API branch"
        );

        // Sanity: a non-matching bearer (e.g. an OPENAI_API_KEY) keeps
        // the Responses branch available — that's the path that makes
        // muse-spark reachable on opencode's Zen tier.
        assert!(
            openai_codex_auth_for_key("sk-not-a-chatgpt-token").is_none(),
            "non-oauth bearer must not resolve via openai_codex_auth_for_key"
        );
        let responses_branch_for_api_key = api_protocol_for("openai", "gpt-5")
            == atom_core::providers::APIProtocol::OpenAIResponses
            && openai_codex_auth_for_key("sk-not-a-chatgpt-token").is_none();
        assert!(
            responses_branch_for_api_key,
            "Responses branch must remain reachable for non-oauth keys"
        );
    }
}
