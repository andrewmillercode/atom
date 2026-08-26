//! Anthropic Messages wire-style client: request marshaling, SSE event
//! streaming into the shared OpenAI-delta StreamChunk shape, and model
//! listing. Covers first-party api.anthropic.com plus every gateway that
//! exposes the Messages dialect (models.dev npm = "@ai-sdk/anthropic",
//! e.g. MiniMax's /anthropic/v1 endpoints).
//!
//! Streaming deliberately translates Anthropic's named events into
//! types::StreamChunk so the server's stream_model_to_client relay, the
//! tool-call accumulator, NDJSON events, and the TUI stay untouched.

use super::providers::{sse_data, SseLineReader};
use super::retry;
use crate::types::{FunctionCall, Message, StreamChunk, StreamDelta, StreamToolCallDelta, ToolDef};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The version header every Messages endpoint expects.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Minimum thinking budget Anthropic accepts.
const MIN_THINKING_BUDGET: i64 = 1024;
/// Tokens kept for the visible answer above the thinking budget.
const THINKING_ANSWER_HEADROOM: i64 = 1024;

// ---------------------------------------------------------------------------
// Auth headers.
// ---------------------------------------------------------------------------

/// applyAuthHeaders adds the auth headers a Messages endpoint accepts.
/// First-party Anthropic wants x-api-key; Bearer-only gateways (MiniMax
/// et al., models.dev issue #1398) reject x-api-key alone, so both are
/// sent whenever a key exists. Empty keys omit both.
pub fn apply_auth_headers(
    builder: reqwest::RequestBuilder,
    api_key: &str,
) -> reqwest::RequestBuilder {
    let builder = builder.header("anthropic-version", ANTHROPIC_VERSION);
    if api_key.is_empty() {
        return builder;
    }
    builder
        .header("x-api-key", api_key)
        .header("Authorization", format!("Bearer {}", api_key))
}

// ---------------------------------------------------------------------------
// Request marshaling.
// ---------------------------------------------------------------------------

/// deriveMaxTokens picks the required max_tokens from the model's context
/// window (the compact catalog keeps only `context`). Capped so prompt +
/// completion always fits, floored so tiny windows still work.
pub fn derive_max_tokens(context_window: i64) -> i64 {
    if context_window <= 0 {
        return 8192;
    }
    let half = context_window / 2;
    half.min(8192).clamp(1, context_window)
}

/// thinkingConfig maps an atom thinking level to the Messages thinking
/// parameter. Empty/"none"/"off" omits it. Named levels map to budgets;
/// numeric strings pass through as raw budgets. Budgets that cannot
/// satisfy Anthropic's constraints (>= 1024, < max_tokens) drop thinking
/// instead of sending an invalid request; unknown words degrade to off.
pub fn thinking_config(level: &str, max_tokens: i64) -> Option<Value> {
    let level = level.trim().to_lowercase();
    if level.is_empty() || level == "none" || level == "off" {
        return None;
    }
    let requested = match level.as_str() {
        "minimal" | "low" => 2048,
        "medium" => 8192,
        "high" => 16384,
        "max" => 32768,
        other => {
            return other
                .parse::<i64>()
                .ok()
                .and_then(|b| clamp_budget(b, max_tokens))
        }
    };
    clamp_budget(requested, max_tokens)
}

fn clamp_budget(budget: i64, max_tokens: i64) -> Option<Value> {
    let ceiling = max_tokens - THINKING_ANSWER_HEADROOM;
    if ceiling < MIN_THINKING_BUDGET || budget < MIN_THINKING_BUDGET {
        return None;
    }
    Some(json!({
        "type": "enabled",
        "budget_tokens": budget.clamp(MIN_THINKING_BUDGET, ceiling),
    }))
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn image_block(mime: &str, data: &str) -> Value {
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": mime, "data": data},
    })
}

/// parseToolInput decodes streamed tool arguments into the object shape
/// tool_use.input requires. Empty or malformed arguments become {}.
fn parse_tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// mapStopReason folds Anthropic stop reasons onto the OpenAI-shaped
/// finish reasons the rest of atom understands. Unknown values stop the
/// turn normally instead of wedging it.
pub fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// strField reads a string field, treating null/missing as "".
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// num reads an i64 field, treating null/missing as 0.
fn num(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// One pending conversation turn: role plus its content blocks.
#[derive(Debug)]
enum Turn {
    User(Vec<Value>),
    Assistant(Vec<Value>),
}

impl Turn {
    fn blocks(self) -> Vec<Value> {
        match self {
            Turn::User(b) | Turn::Assistant(b) => b,
        }
    }
}

/// marshalAnthropicRequest converts atom's chat history into a Messages
/// API body: system messages fold into top-level `system`; tool calls
/// become tool_use blocks answered by tool_result blocks; adjacent
/// same-role turns coalesce because Anthropic demands strict
/// user/assistant alternation starting with user.
///
/// Tool_use blocks with no matching tool_result (orphans left behind by
/// sanitization) are dropped, as are tool_results with no tool_use —
/// either would make the API reject the whole request.
pub fn marshal_anthropic_request(
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
    thinking: &str,
    max_tokens: i64,
) -> anyhow::Result<Value> {
    let mut system_parts: Vec<String> = Vec::new();

    // Tool_use ids answered somewhere later in the transcript.
    let mut answered: HashSet<&str> = HashSet::new();
    for m in msgs {
        if m.role == "tool" && !m.tool_call_id.is_empty() {
            answered.insert(m.tool_call_id.as_str());
        }
    }

    let mut turns: Vec<Turn> = Vec::new();
    for m in msgs {
        match m.role.as_str() {
            "system" => {
                if !m.content.trim().is_empty() {
                    system_parts.push(m.content.clone());
                }
            }
            // Compaction markers are folded away upstream; error records
            // are bookkeeping, not speakable turns.
            "compaction" | "error" => {}
            "nudge" => turns.push(Turn::User(vec![text_block(&m.content)])),
            "tool" => {
                let mut content: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    content.push(text_block(&m.content));
                }
                for img in &m.images {
                    content.push(image_block(&img.mime, &img.data));
                }
                if content.is_empty() {
                    content.push(text_block(""));
                }
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id,
                    "content": content,
                });
                // Consecutive tool rows share one user turn.
                match turns.last_mut() {
                    Some(Turn::User(blocks))
                        if blocks
                            .first()
                            .and_then(|b| b.get("type"))
                            .and_then(|t| t.as_str())
                            == Some("tool_result") =>
                    {
                        blocks.push(block);
                    }
                    _ => turns.push(Turn::User(vec![block])),
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                // Replay prior reasoning WITH its signature so thinking
                // survives across turns. Anthropic requires an opaque
                // `signature` on any thinking block it receives back, and
                // thinking blocks must precede the turn's text/tool_use.
                // History without a signature (saved before signatures were
                // captured, or a non-thinking turn) omits the block —
                // emitting one without a signature would 400.
                if !m.reasoning.is_empty() && !m.reasoning_signature.is_empty() {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": m.reasoning,
                        "signature": m.reasoning_signature,
                    }));
                }
                if !m.content.is_empty() {
                    blocks.push(text_block(&m.content));
                }
                for tc in &m.tool_calls {
                    if !answered.contains(tc.id.as_str()) {
                        continue;
                    }
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.function.name,
                        "input": parse_tool_input(&tc.function.arguments),
                    }));
                }
                if !blocks.is_empty() {
                    turns.push(Turn::Assistant(blocks));
                }
            }
            // user and anything else speakable.
            _ => {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(text_block(&m.content));
                }
                for img in &m.images {
                    blocks.push(image_block(&img.mime, &img.data));
                }
                if blocks.is_empty() {
                    blocks.push(text_block(""));
                }
                turns.push(Turn::User(blocks));
            }
        }
    }

    // Coalesce adjacent same-role turns, then guarantee the sequence
    // starts with user (drop a leading assistant remnant).
    let mut merged: Vec<(bool, Vec<Value>)> = Vec::new();
    for t in turns {
        let is_user = matches!(t, Turn::User(_));
        match merged.last_mut() {
            Some((same_user, blocks)) if *same_user == is_user => blocks.extend(t.blocks()),
            _ => merged.push((is_user, t.blocks())),
        }
    }
    while merged.first().map(|(u, _)| !*u).unwrap_or(false) {
        merged.remove(0);
    }
    if merged.is_empty() {
        merged.push((true, vec![text_block("")]));
    }

    let messages: Vec<Value> = merged
        .into_iter()
        .map(|(is_user, blocks)| {
            json!({
                "role": if is_user { "user" } else { "assistant" },
                "content": blocks,
            })
        })
        .collect();

    let tool_defs: Vec<Value> = tools
        .iter()
        .filter(|t| !t.function.name.is_empty())
        .map(|t| {
            json!({
                "name": t.function.name,
                "description": t.function.description,
                "input_schema": if t.function.parameters.is_null() {
                    json!({"type": "object"})
                } else {
                    t.function.parameters.clone()
                },
            })
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("messages".into(), Value::Array(messages));
    let system = system_parts.join("\n\n");
    if !system.is_empty() {
        body.insert("system".into(), json!(system));
    }
    if !tool_defs.is_empty() {
        body.insert("tools".into(), Value::Array(tool_defs));
    }
    if let Some(t) = thinking_config(thinking, max_tokens) {
        body.insert("thinking".into(), t);
    }
    body.insert("stream".into(), json!(true));
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// Streaming.
// ---------------------------------------------------------------------------

fn usage_from(
    prompt: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
) -> Option<crate::types::StreamUsage> {
    if prompt + cache_read + cache_write + output <= 0 {
        return None;
    }
    // Anthropic reports uncached input separately from cache traffic; the
    // prompt total (what the rest of atom treats as context size) sums
    // all three.
    let prompt_total = prompt + cache_read + cache_write;
    Some(crate::types::StreamUsage {
        prompt_tokens: prompt_total,
        completion_tokens: output,
        total_tokens: prompt_total + output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ..Default::default()
    })
}

/// streamAnthropic POSTs {base}/messages and translates the SSE event
/// stream into StreamChunks. Transport failures and non-retryable
/// statuses go through the shared provider retry policy; a mid-stream
/// `error` event (overloaded_error etc.) surfaces as a stream error
/// rather than a silently-empty reply.
pub async fn stream_anthropic(
    base_url: &str,
    api_key: &str,
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
    thinking: &str,
) -> anyhow::Result<impl futures::Stream<Item = anyhow::Result<StreamChunk>>> {
    let url = format!("{}/messages", base_url.trim_end_matches('/'));
    let max_tokens = derive_max_tokens(super::context_window_tokens("", model));
    let body = serde_json::to_vec(&marshal_anthropic_request(
        model, msgs, tools, thinking, max_tokens,
    )?)?;
    let resp = retry::do_http_with_retry(|| {
        let builder = retry::long_timeout_client()
            .post(url.clone())
            .header("Content-Type", "application/json");
        apply_auth_headers(builder, api_key)
            .body(body.clone())
            .send()
    })
    .await?;
    let byte_stream = Box::pin(resp.bytes_stream());
    Ok(futures::stream::unfold(
        AnthropicStreamState::new(SseLineReader::new(byte_stream)),
        // unfold owns the state between polls, so next_item consumes and
        // hands back the state instead of borrowing self.
        |st| async move { st.next_item().await },
    ))
}

struct AnthropicStreamState<S> {
    reader: SseLineReader<S>,
    prompt: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    finished: bool,
}

impl<S> AnthropicStreamState<S>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    fn new(reader: SseLineReader<S>) -> Self {
        AnthropicStreamState {
            reader,
            prompt: 0,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            finished: false,
        }
    }

    fn usage(&self) -> Option<crate::types::StreamUsage> {
        usage_from(self.prompt, self.cache_read, self.cache_write, self.output)
    }

    fn tool_delta(index: i64, id: &str, name: &str, arguments: &str) -> StreamChunk {
        StreamChunk {
            choices: vec![crate::types::StreamChunkChoice {
                delta: StreamDelta {
                    tool_calls: vec![StreamToolCallDelta {
                        index,
                        id: id.to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: arguments.to_string(),
                        },
                    }],
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Handles one decoded data payload. Returns an optional chunk to
    /// emit; None means "read the next event".
    fn handle_event(&mut self, v: &Value) -> Option<StreamChunk> {
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                let usage = &v["message"]["usage"];
                self.prompt = num(usage, "input_tokens");
                self.cache_read = num(usage, "cache_read_input_tokens");
                self.cache_write = num(usage, "cache_creation_input_tokens");
                None
            }
            "ping" | "content_block_stop" | "signature_delta_placeholder" => None,
            "content_block_start" => {
                let cb = &v["content_block"];
                if cb.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    return None;
                }
                Some(Self::tool_delta(
                    v.get("index").and_then(Value::as_i64).unwrap_or(0),
                    cb.get("id").and_then(Value::as_str).unwrap_or(""),
                    cb.get("name").and_then(Value::as_str).unwrap_or(""),
                    "",
                ))
            }
            "content_block_delta" => {
                let delta = &v["delta"];
                let index = v.get("index").and_then(Value::as_i64).unwrap_or(0);
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => Some(StreamChunk {
                        choices: vec![crate::types::StreamChunkChoice {
                            delta: StreamDelta {
                                content: str_field(delta, "text"),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    "thinking_delta" => Some(StreamChunk {
                        choices: vec![crate::types::StreamChunkChoice {
                            delta: StreamDelta {
                                reasoning: str_field(delta, "thinking"),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    "signature_delta" => Some(StreamChunk {
                        choices: vec![crate::types::StreamChunkChoice {
                            delta: StreamDelta {
                                reasoning_signature: str_field(delta, "signature"),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    "input_json_delta" => Some(Self::tool_delta(
                        index,
                        "",
                        "",
                        &str_field(delta, "partial_json"),
                    )),
                    _ => None,
                }
            }
            "message_delta" => {
                let output = num(&v["usage"], "output_tokens");
                if output > 0 {
                    self.output = output;
                }
                let finish = map_stop_reason(
                    v.pointer("/delta/stop_reason")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                Some(StreamChunk {
                    choices: vec![crate::types::StreamChunkChoice {
                        delta: StreamDelta::default(),
                        finish_reason: finish.to_string(),
                    }],
                    usage: self.usage(),
                })
            }
            "message_stop" => {
                self.finished = true;
                Some(StreamChunk {
                    choices: vec![],
                    usage: self.usage(),
                })
            }
            _ => None,
        }
    }

    /// Handles one SSE line at a time until a chunk is ready or the
    /// stream ends; consumes and returns the state because unfold owns
    /// it between polls.
    async fn next_item(mut self) -> Option<(anyhow::Result<StreamChunk>, Self)> {
        loop {
            if self.finished {
                return None;
            }
            let line = match self.reader.next().await {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    self.finished = true;
                    return Some((Err(e), self));
                }
                None => return None,
            };
            let trim = line.trim();
            if trim.is_empty() || trim.starts_with(':') {
                continue;
            }
            let data = match sse_data(trim) {
                Some(d) => d,
                None => continue,
            };
            if data == "[DONE]" {
                self.finished = true;
                return None;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(Value::as_str) == Some("error") {
                self.finished = true;
                let kind = str_field(&v["error"], "type");
                let msg = str_field(&v["error"], "message");
                let detail = if msg.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind}: {msg}")
                };
                return Some((
                    Err(anyhow::anyhow!("anthropic stream error: {detail}")),
                    self,
                ));
            }
            if let Some(chunk) = self.handle_event(&v) {
                return Some((Ok(chunk), self));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{marshal_anthropic_request, AnthropicStreamState, SseLineReader};
    use crate::types::Message;
    use serde_json::{json, Value};

    fn msgs_with_signed_reasoning() -> Vec<Message> {
        vec![
            Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                content: "hello".into(),
                reasoning: "I considered greeting.".into(),
                reasoning_signature: "sig-abc".into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "again".into(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn marshal_replays_thinking_with_signature() {
        // A prior assistant turn with a signature replays its thinking
        // block (type:thinking, thinking, signature) BEFORE the text
        // block, so reasoning survives across turns. Anthropic requires
        // the signature on any thinking block it receives back.
        let msgs = msgs_with_signed_reasoning();
        let body = marshal_anthropic_request("claude-opus-4-6", &msgs, &[], "max", 32000).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        // thinking block first, then text.
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "I considered greeting.");
        assert_eq!(blocks[0]["signature"], "sig-abc");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "hello");
    }

    #[test]
    fn marshal_omits_thinking_without_signature() {
        // History without a signature (saved before signatures were
        // captured, or a non-thinking turn) omits the thinking block —
        // emitting one without a signature would 400.
        let mut msgs = msgs_with_signed_reasoning();
        msgs[1].reasoning_signature = String::new();
        let body = marshal_anthropic_request("claude-opus-4-6", &msgs, &[], "max", 32000).unwrap();
        let blocks = body["messages"].as_array().unwrap()[1]["content"]
            .as_array()
            .unwrap();
        // only the text block; no thinking block.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
        assert!(
            !blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking")),
            "no thinking block without a signature"
        );
    }

    #[test]
    fn signature_delta_captures_signature() {
        // A streamed signature_delta event populates delta.reasoning
        // _signature (text/reasoning stay empty) so the thinking block
        // can be replayed across turns. The reader is unused since we
        // drive handle_event directly with a synthetic event.
        let reader = SseLineReader::new(futures::stream::empty());
        let mut st = AnthropicStreamState::new(reader);
        let v: Value = json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "signature_delta", "signature": "sig-xyz" }
        });
        let chunk = st.handle_event(&v).expect("signature_delta emits a chunk");
        assert_eq!(chunk.choices[0].delta.reasoning_signature, "sig-xyz");
        assert!(chunk.choices[0].delta.reasoning.is_empty());
        assert!(chunk.choices[0].delta.content.is_empty());
    }
}
