//! Amazon Bedrock Converse API client: request marshaling from atom's
//! chat history to the Converse wire shape, and ConverseStream SSE
//! translation into the shared OpenAI-delta StreamChunk shape.
//!
//! Auth is bearer-token only ("API key" mode): Bedrock accepts
//! `Authorization: Bearer <token>` (AWS_BEARER_TOKEN_BEDROCK) and skips
//! SigV4 entirely. SigV4 access-key signing is deliberately not
//! implemented — atom stores one key per provider, and a raw access key
//! id cannot sign anything without the secret.
//!
//! Streaming translates Bedrock's named events into types::StreamChunk
//! so the server's stream_model_to_client relay, tool-call accumulator,
//! NDJSON events, and TUI stay untouched, mirroring anthropic.rs.

use super::retry;
use crate::types::{FunctionCall, Message, StreamChunk, StreamDelta, StreamToolCallDelta, ToolDef};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::{json, Value};

/// Default region when AWS_REGION / AWS_DEFAULT_REGION are unset. The
/// models.dev fallback base URL points at this region's runtime host.
const DEFAULT_REGION: &str = "us-east-1";

/// Regions that can serve each cross-region inference-profile geo
/// prefix (`us.`/`eu.`/`apac.`/`au.`/`jp.`). A geo-prefixed profile id
/// like "eu.anthropic.claude-..." is only servable from its own geo, so
/// routing one at us-east-1 fails with "invalid model identifier".
fn region_serves_geo(region: &str, geo: &str) -> bool {
    match geo {
        "us" => region.starts_with("us-") && !region.starts_with("us-gov-"),
        "us-gov" => region.starts_with("us-gov-"),
        "eu" => region.starts_with("eu-"),
        // ap- regions overlap across apac/au/jp profiles; pin each to the
        // regions that actually host those profiles.
        "au" => region == "ap-southeast-2" || region == "ap-southeast-4",
        "jp" => region == "ap-northeast-1" || region == "ap-northeast-3",
        "apac" => region.starts_with("ap-"),
        _ => false,
    }
}

/// Default region per inference-profile geo prefix.
fn geo_default_region(geo: &str) -> &'static str {
    match geo {
        "us" => "us-east-1",
        "us-gov" => "us-gov-west-1",
        "eu" => "eu-west-1",
        "apac" => "ap-southeast-1",
        "au" => "ap-southeast-2",
        "jp" => "ap-northeast-1",
        _ => DEFAULT_REGION,
    }
}

/// resolveBedrockRegion picks the runtime region for a model id. The
/// base_url argument carries the configured host (its region segment is
/// honored as an explicit override); otherwise AWS_REGION /
/// AWS_DEFAULT_REGION win when they can serve the profile's geo,
/// falling back to the geo's default region.
pub fn resolve_bedrock_region(base_url: &str, model_id: &str) -> String {
    // Explicit override: the region embedded in the configured host
    // ("bedrock-runtime.eu-west-1.amazonaws.com" → "eu-west-1").
    if let Some(rest) = base_url.strip_prefix("https://bedrock-runtime.") {
        if let Some(region) = rest.split('.').next() {
            if !region.is_empty() {
                return region.to_string();
            }
        }
    }
    let ambient = super::providers::ambient_aws_region();
    let Some((geo, _)) = model_id.split_once('.') else {
        return ambient.unwrap_or_else(|| DEFAULT_REGION.to_string());
    };
    match geo {
        "us" | "us-gov" | "eu" | "apac" | "au" | "jp" => {}
        _ => return ambient.unwrap_or_else(|| DEFAULT_REGION.to_string()),
    }
    if let Some(r) = ambient {
        if region_serves_geo(&r, geo) {
            return r;
        }
    }
    geo_default_region(geo).to_string()
}

/// converseStreamURL builds the regional ConverseStream endpoint for a
/// model id: https://bedrock-runtime.{region}.amazonaws.com/model/{id}/converse-stream
pub fn converse_stream_url(base_url: &str, model_id: &str) -> String {
    let region = resolve_bedrock_region(base_url, model_id);
    format!(
        "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse-stream",
        region,
        urlencode_component(model_id),
    )
}

/// Minimal percent-encoding for a path component (model ids are
/// alphanumerics, dots, dashes, colons, and slashes in practice).
fn urlencode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Request marshaling.
// ---------------------------------------------------------------------------

/// parseToolInput decodes streamed tool arguments into the object shape
/// toolUse.input requires. Empty or malformed arguments become {}.
fn parse_tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// marshalBedrockRequest converts atom's chat history into the Bedrock
/// Converse request shape: system messages become `system` text blocks,
/// tool results become user-turn toolResult blocks, assistant tool calls
/// become toolUse blocks (only for calls actually answered later), and
/// OpenAI JSON-schema tools become toolSpec entries. Adjacent same-role
/// turns are coalesced and the sequence is guaranteed to start with user.
///
/// `model` + `thinking` + `max_tokens` drive Claude extended/adaptive
/// thinking via `additionalModelRequestFields` (see `bedrock_thinking`);
/// non-Claude models and an off thinking level omit it entirely, leaving
/// the request byte-identical to the pre-thinking behavior.
pub fn marshal_bedrock_request(
    msgs: &[Message],
    tools: &[ToolDef],
    model: &str,
    thinking: &str,
    max_tokens: i64,
) -> anyhow::Result<Value> {
    let mut system_parts: Vec<String> = Vec::new();

    // Tool_use ids answered somewhere later in the transcript; unanswered
    // calls would make Bedrock reject the whole request.
    let mut answered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in msgs {
        if m.role == "tool" && !m.tool_call_id.is_empty() {
            answered.insert(m.tool_call_id.as_str());
        }
    }

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
            "nudge" => turns.push(Turn::User(vec![json!({ "text": m.content })])),
            "tool" => {
                let mut content: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    content.push(json!({ "text": m.content }));
                }
                // Images inside tool results are accepted by Bedrock but
                // rare; keep parity with the anthropic dialect anyway.
                for img in &m.images {
                    content.push(image_block(&img.mime, &img.data));
                }
                if content.is_empty() {
                    content.push(json!({ "text": "" }));
                }
                let block = json!({
                    "toolResult": {
                        "toolUseId": m.tool_call_id,
                        "content": content,
                        "status": "success",
                    }
                });
                // Consecutive tool rows share one user turn — Bedrock
                // requires all tool results in a single message.
                match turns.last_mut() {
                    Some(Turn::User(blocks))
                        if blocks.first().and_then(|b| b.get("toolResult")).is_some() =>
                    {
                        blocks.push(block);
                    }
                    _ => turns.push(Turn::User(vec![block])),
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({ "text": m.content }));
                }
                // Replay prior reasoning WITH its signature so thinking
                // survives across turns. Bedrock (like Anthropic) requires
                // an opaque `signature` on any thinking block it receives
                // back; without one it 400s with
                // "thinking.signature: Field required". History saved
                // before signatures were captured (or a turn that never
                // thought) has no signature, so we omit the block then —
                // Bedrock accepts an assistant turn without reasoningContent.
                if !m.reasoning.is_empty() && !m.reasoning_signature.is_empty() {
                    blocks.push(json!({
                        "reasoningContent": {
                            "reasoningText": {
                                "text": m.reasoning,
                                "signature": m.reasoning_signature,
                            }
                        }
                    }));
                }
                for tc in &m.tool_calls {
                    if !answered.contains(tc.id.as_str()) {
                        continue;
                    }
                    blocks.push(json!({
                        "toolUse": {
                            "toolUseId": tc.id,
                            "name": tc.function.name,
                            "input": parse_tool_input(&tc.function.arguments),
                        }
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
                    blocks.push(json!({ "text": m.content }));
                }
                for img in &m.images {
                    blocks.push(image_block(&img.mime, &img.data));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "text": "" }));
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
        merged.push((true, vec![json!({ "text": "" })]));
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

    let mut body = serde_json::Map::new();
    body.insert("messages".into(), Value::Array(messages));
    let system = system_parts.join("\n\n");
    if !system.is_empty() {
        body.insert("system".into(), json!([{ "text": system }]));
    }
    if !tools.is_empty() {
        body.insert(
            "toolConfig".into(),
            json!({
                "tools": tools.iter().map(|t| json!({
                    "toolSpec": {
                        "name": t.function.name,
                        "description": t.function.description,
                        "inputSchema": { "json": t.function.parameters.clone() },
                    }
                })).collect::<Vec<_>>(),
                "toolChoice": { "auto": {} },
            }),
        );
    }
    if let Some(t) = bedrock_thinking(model, thinking, max_tokens) {
        body.insert("additionalModelRequestFields".into(), t.additional);
        body.insert("inferenceConfig".into(), t.inference);
    }
    Ok(Value::Object(body))
}

/// BedrockThinking bundles the Converse fields atom emits to turn Claude
/// reasoning on: `additionalModelRequestFields` carries the model-native
/// `thinking` (and for adaptive models the `effort`), while
/// `inferenceConfig` sets `maxTokens` (and, for extended thinking,
/// `temperature: 1`, which Bedrock requires while reasoning is on).
struct BedrockThinking {
    additional: Value,
    inference: Value,
}

/// bedrockThinkingMaxTokens picks a `maxTokens` that leaves real room for
/// a thinking budget yet stays within every Claude Bedrock model's output
/// limit (the smallest is 32K), and within the documented recommendation
/// to use batch processing above 32K. Derived from the context window.
fn bedrock_thinking_max_tokens(context: i64) -> i64 {
    if context <= 0 {
        return 8192;
    }
    // Half the window for completion, clamped to [8K, 32K] (32K is the
    // smallest Claude Bedrock output limit and the documented batch
    // threshold), then never larger than the whole window.
    (context / 2).clamp(8192, 32000).min(context)
}

/// bedrockClaudeAdaptive reports whether a Bedrock Claude model id speaks
/// the adaptive-thinking dialect (`thinking.type: "adaptive"` +
/// `output_config.effort`) instead of the legacy extended-thinking one
/// (`thinking.type: "enabled"` + `budget_tokens`).
///
/// Adaptive: Opus 4.6+, Sonnet 4.6+, Opus/Sonnet 5, Fable, Mythos. The
/// legacy `enabled`+`budget_tokens` shape is deprecated on Opus/Sonnet
/// 4.6 and rejected with a 400 on Opus 4.7+/Sonnet 5/Opus 5/Fable/Mythos,
/// so those MUST use adaptive. Older Claude (3.7, 4, 4.1, 4.5 incl.
/// Haiku 4.5) keeps extended thinking.
fn bedrock_claude_adaptive(id_lower: &str) -> bool {
    if id_lower.contains("fable") || id_lower.contains("mythos") {
        return true;
    }
    for fam in ["opus", "sonnet", "haiku"] {
        let marker = format!("claude-{fam}");
        let Some(rest) = id_lower.split_once(&marker).map(|(_, r)| r) else {
            continue;
        };
        let Some(nums) = rest.strip_prefix('-') else {
            continue;
        };
        let mut it = nums.split(['-', '.']);
        let major = it.next().and_then(|t| t.parse::<u32>().ok()).unwrap_or(0);
        if major >= 5 {
            return true;
        }
        if major == 4 {
            // The segment after the major is either a minor version
            // (4-6) or a release date (4-20250514). A date is many
            // digits, so only a small number counts as the minor; a
            // date (or no segment at all) means major-only (Claude 4).
            let minor = it
                .next()
                .and_then(|t| t.parse::<u32>().ok())
                .filter(|m| *m < 100)
                .unwrap_or(0);
            return minor >= 6;
        }
        return false;
    }
    false
}

/// bedrockEffort maps an atom thinking level onto a Bedrock adaptive
/// `effort`. Bedrock accepts low/medium/high/xhigh/max; xhigh and max are
/// Opus 4.6 / Opus 5 only, so "max" degrades to "high" on other adaptive
/// models to avoid a ValidationException.
fn bedrock_effort(model_id_lower: &str, level: &str) -> &'static str {
    let opus_max = model_id_lower.contains("opus-4-6") || model_id_lower.contains("opus-5");
    match level {
        "max" if opus_max => "max",
        "max" => "high",
        "minimal" | "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

/// bedrockThinking builds the Converse reasoning fields for a Claude
/// model from an atom thinking level, or returns None to leave reasoning
/// off. Non-Claude Bedrock models reason natively and reject the
/// `thinking` field, so they get None. Off levels ("", "none", "off")
/// also get None: omitting `thinking` is the off state on 4.6/4.5, and
/// the always-adaptive models (Opus 5/Sonnet 5/Fable/Mythos) can't be
/// forced off anyway.
fn bedrock_thinking(model: &str, level: &str, max_tokens: i64) -> Option<BedrockThinking> {
    let id = model.to_lowercase();
    if !id.contains("claude") {
        return None;
    }
    let lvl = level.trim().to_lowercase();
    if lvl.is_empty() || lvl == "none" || lvl == "off" {
        return None;
    }
    if bedrock_claude_adaptive(&id) {
        // Adaptive: newer Claude rejects a pinned temperature/topP, so
        // inferenceConfig carries only maxTokens (thinking room).
        let effort = bedrock_effort(&id, &lvl);
        Some(BedrockThinking {
            additional: json!({
                "thinking": { "type": "adaptive" },
                "output_config": { "effort": effort },
            }),
            inference: json!({ "maxTokens": max_tokens }),
        })
    } else {
        // Extended thinking (Claude 3.7 / 4 / 4.5): budget must be <
        // maxTokens, and temperature must be 1 while reasoning is on.
        super::anthropic::thinking_config(&lvl, max_tokens).map(|t| BedrockThinking {
            additional: json!({ "thinking": t }),
            inference: json!({ "maxTokens": max_tokens, "temperature": 1.0 }),
        })
    }
}

fn image_block(mime: &str, data: &str) -> Value {
    let format = match mime {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "",
    };
    if format.is_empty() {
        // Unknown image type: degrade to an empty text block rather than
        // failing the whole request.
        return json!({ "text": "" });
    }
    json!({ "image": { "format": format, "source": { "bytes": data } } })
}

/// mapStopReason folds Bedrock stop reasons onto the OpenAI-shaped
/// finish reasons the rest of atom understands. Unknown values stop the
/// turn normally instead of wedging it.
pub fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

// ---------------------------------------------------------------------------
// AWS eventstream binary framing.
// ---------------------------------------------------------------------------
//
// ConverseStream over raw HTTP responds with Content-Type
// application/vnd.amazon.eventstream — NOT server-sent events. Each
// message frame is:
//
//   [4B total length][4B headers length][4B prelude CRC]
//   [headers][payload][4B message CRC]
//
// Headers are typed key/value pairs; we need ":message-type"
// ("event"/"exception"/"error") and ":event-type" (e.g.
// "contentBlockDelta"). Payloads on the Converse path are the event JSON
// itself. CRCs are skipped, matching other minimal decoders.

/// Header value types per the event-stream encoding spec. Payloads are
/// retained to document the wire type; only String values
/// (":message-type", ":event-type") are ever read back. The numeric
/// variants exist so the parser skips the correct byte width for every
/// header it encounters.
#[allow(dead_code)]
enum HeaderValue {
    True,
    False,
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    ByteArray(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    if buf.len() < off + 4 {
        return None;
    }
    Some(u32::from_be_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
    ]))
}

/// Parses one complete eventstream frame from the front of `buf`.
/// Returns None when more bytes are needed. On success returns the
/// consumed length plus the ":event-type"-ish header map and payload.
fn parse_frame(buf: &[u8]) -> Option<(usize, Vec<(String, HeaderValue)>, Bytes)> {
    let total = read_u32(buf, 0)? as usize;
    if total < 16 || total > 8_000_000 || buf.len() < total {
        return None;
    }
    let headers_len = read_u32(buf, 4)? as usize;
    if headers_len + 12 > total {
        return None;
    }
    let headers_end = 12 + headers_len;
    let payload = Bytes::copy_from_slice(&buf[headers_end..total - 4]);
    let mut headers = Vec::new();
    let mut off = 12usize;
    let hdr = &buf[..headers_end];
    while off < headers_end {
        let name_len = *hdr.get(off)? as usize;
        off += 1;
        let name = String::from_utf8_lossy(hdr.get(off..off + name_len)?).to_string();
        off += name_len;
        let ty = *hdr.get(off)?;
        off += 1;
        let value = match ty {
            0 => HeaderValue::True,
            1 => HeaderValue::False,
            2 => {
                let v = *hdr.get(off)? as i8;
                off += 1;
                HeaderValue::Byte(v)
            }
            3 => {
                let v = i16::from_be_bytes([*hdr.get(off)?, *hdr.get(off + 1)?]);
                off += 2;
                HeaderValue::Short(v)
            }
            4 => {
                let v = i32::from_be_bytes([
                    *hdr.get(off)?,
                    *hdr.get(off + 1)?,
                    *hdr.get(off + 2)?,
                    *hdr.get(off + 3)?,
                ]);
                off += 4;
                HeaderValue::Integer(v)
            }
            5 => {
                let v = i64::from_be_bytes([
                    *hdr.get(off)?,
                    *hdr.get(off + 1)?,
                    *hdr.get(off + 2)?,
                    *hdr.get(off + 3)?,
                    *hdr.get(off + 4)?,
                    *hdr.get(off + 5)?,
                    *hdr.get(off + 6)?,
                    *hdr.get(off + 7)?,
                ]);
                off += 8;
                HeaderValue::Long(v)
            }
            6 => {
                let len = (*hdr.get(off)? as usize) << 8 | *hdr.get(off + 1)? as usize;
                off += 2;
                let v = hdr.get(off..off + len)?.to_vec();
                off += len;
                HeaderValue::ByteArray(v)
            }
            7 => {
                let len = (*hdr.get(off)? as usize) << 8 | *hdr.get(off + 1)? as usize;
                off += 2;
                let v = String::from_utf8_lossy(hdr.get(off..off + len)?).to_string();
                off += len;
                HeaderValue::String(v)
            }
            8 => {
                let v = i64::from_be_bytes([
                    *hdr.get(off)?,
                    *hdr.get(off + 1)?,
                    *hdr.get(off + 2)?,
                    *hdr.get(off + 3)?,
                    *hdr.get(off + 4)?,
                    *hdr.get(off + 5)?,
                    *hdr.get(off + 6)?,
                    *hdr.get(off + 7)?,
                ]);
                off += 8;
                HeaderValue::Timestamp(v)
            }
            9 => {
                let mut v = [0u8; 16];
                v.copy_from_slice(hdr.get(off..off + 16)?);
                off += 16;
                HeaderValue::Uuid(v)
            }
            _ => return None,
        };
        headers.push((name, value));
    }
    Some((total, headers, payload))
}

fn header_string(headers: &[(String, HeaderValue)], name: &str) -> Option<String> {
    for (k, v) in headers {
        if k == name {
            return match v {
                HeaderValue::String(s) => Some(s.clone()),
                _ => None,
            };
        }
    }
    None
}

/// Incremental eventstream decoder over reqwest's byte stream. Yields
/// (message-type, event-type, payload) triples.
struct EventStreamDecoder {
    stream: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: BytesMut,
    done: bool,
}

impl EventStreamDecoder {
    fn new(
        stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    ) -> Self {
        EventStreamDecoder {
            stream: Box::pin(stream),
            buf: BytesMut::new(),
            done: false,
        }
    }

    async fn next_frame(&mut self) -> Option<anyhow::Result<(String, String, Value)>> {
        loop {
            if !self.done {
                if let Some((consumed, headers, payload)) = parse_frame(&self.buf) {
                    let _ = self.buf.split_to(consumed);
                    let mtype =
                        header_string(&headers, ":message-type").unwrap_or_else(|| "event".into());
                    let etype = header_string(&headers, ":event-type")
                        .or_else(|| header_string(&headers, ":exception-type"))
                        .unwrap_or_default();
                    let payload: Value = if payload.is_empty() {
                        Value::Null
                    } else {
                        match serde_json::from_slice(&payload) {
                            Ok(v) => v,
                            Err(_) => Value::Null,
                        }
                    };
                    return Some(Ok((mtype, etype, payload)));
                }
                match self.stream.next().await {
                    Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                    Some(Err(e)) => {
                        self.done = true;
                        return Some(Err(anyhow::anyhow!("bedrock stream read: {e}")));
                    }
                    None => {
                        self.done = true;
                        // Truncated final frame: nothing more to emit.
                        return None;
                    }
                }
            } else {
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming.
// ---------------------------------------------------------------------------

fn usage_from(
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
) -> Option<crate::types::StreamUsage> {
    if input + cache_read + cache_write + output <= 0 {
        return None;
    }
    let prompt_total = input + cache_read + cache_write;
    Some(crate::types::StreamUsage {
        prompt_tokens: prompt_total,
        completion_tokens: output,
        total_tokens: prompt_total + output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ..Default::default()
    })
}

struct BedrockStreamState {
    decoder: EventStreamDecoder,
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    finished: bool,
}

impl BedrockStreamState {
    fn usage(&self) -> Option<crate::types::StreamUsage> {
        usage_from(self.input, self.cache_read, self.cache_write, self.output)
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

    /// Handles one decoded Converse event. Returns an optional chunk to
    /// emit; None means "read the next frame".
    fn handle_event(&mut self, event_type: &str, v: &Value) -> Option<StreamChunk> {
        match event_type {
            "messageStart" | "contentBlockStop" => None,
            "contentBlockStart" => {
                let tu = &v["start"]["toolUse"];
                if !tu.is_object() {
                    return None;
                }
                Some(Self::tool_delta(
                    num(v, "contentBlockIndex"),
                    &str_field(tu, "toolUseId"),
                    &str_field(tu, "name"),
                    "",
                ))
            }
            "contentBlockDelta" => {
                let delta = &v["delta"];
                let index = num(v, "contentBlockIndex");
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    return Some(StreamChunk {
                        choices: vec![crate::types::StreamChunkChoice {
                            delta: StreamDelta {
                                content: text.to_string(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    });
                }
                if let Some(tu) = delta.get("toolUse").filter(|t| t.is_object()) {
                    return Some(Self::tool_delta(index, "", "", &str_field(tu, "input")));
                }
                if let Some(rc) = delta.get("reasoningContent").filter(|r| r.is_object()) {
                    // Claude-on-Bedrock reasoning deltas carry text under
                    // reasoningContent.text and an opaque signature under
                    // reasoningContent.signature. The signature may arrive
                    // in the same delta as the text or in a signature-only
                    // delta; capture both so the assistant turn can be
                    // replayed with its signature (reasoning preserved
                    // across turns).
                    let text = rc
                        .get("text")
                        .or_else(|| rc.pointer("/reasoningText/text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let sig = rc
                        .get("signature")
                        .or_else(|| rc.pointer("/reasoningText/signature"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !text.is_empty() || !sig.is_empty() {
                        return Some(StreamChunk {
                            choices: vec![crate::types::StreamChunkChoice {
                                delta: StreamDelta {
                                    reasoning: text.to_string(),
                                    reasoning_signature: sig.to_string(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }],
                            ..Default::default()
                        });
                    }
                }
                None
            }
            "messageStop" => {
                let finish = map_stop_reason(str_field(v, "stopReason").trim());
                Some(StreamChunk {
                    choices: vec![crate::types::StreamChunkChoice {
                        delta: StreamDelta::default(),
                        finish_reason: finish.to_string(),
                    }],
                    usage: self.usage(),
                })
            }
            "metadata" => {
                let u = &v["usage"];
                let input = num(u, "inputTokens");
                let output = num(u, "outputTokens");
                if input > 0 {
                    self.input = input;
                }
                if output > 0 {
                    self.output = output;
                }
                let cr = num(u, "cacheReadInputTokens");
                if cr > 0 {
                    self.cache_read = cr;
                }
                let cw = num(u, "cacheWriteInputTokens");
                if cw > 0 {
                    self.cache_write = cw;
                }
                None
            }
            _ => None,
        }
    }

    async fn next_item(mut self) -> Option<(anyhow::Result<StreamChunk>, Self)> {
        loop {
            if self.finished {
                return None;
            }
            let (mtype, etype, payload) = match self.decoder.next_frame().await {
                Some(Ok(t)) => t,
                Some(Err(e)) => {
                    self.finished = true;
                    return Some((Err(e), self));
                }
                None => return None,
            };
            match mtype.as_str() {
                "exception" => {
                    self.finished = true;
                    let kind = if etype.is_empty() {
                        "Exception".to_string()
                    } else {
                        etype
                    };
                    let msg = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let detail = if msg.is_empty() {
                        kind.clone()
                    } else {
                        format!("{kind}: {msg}")
                    };
                    return Some((
                        Err(anyhow::anyhow!("bedrock stream exception: {detail}")),
                        self,
                    ));
                }
                "error" => {
                    self.finished = true;
                    let msg = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    return Some((Err(anyhow::anyhow!("bedrock stream error: {msg}")), self));
                }
                _ => {}
            }
            if mtype != "event" {
                continue;
            }
            if let Some(chunk) = self.handle_event(&etype, &payload) {
                return Some((Ok(chunk), self));
            }
        }
    }
}

/// streamBedrock POSTs the ConverseStream request with bearer auth and
/// translates the binary eventstream response into StreamChunks.
/// Establishing the request uses the shared provider retry policy.
pub async fn stream_bedrock(
    base_url: &str,
    api_key: &str,
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
    thinking: &str,
) -> anyhow::Result<impl futures::Stream<Item = anyhow::Result<StreamChunk>>> {
    let url = converse_stream_url(base_url, model);
    let max_tokens = bedrock_thinking_max_tokens(super::context_window_tokens("", model));
    let body = serde_json::to_vec(&marshal_bedrock_request(
        msgs, tools, model, thinking, max_tokens,
    )?)?;
    let resp = retry::do_http_with_retry(|| {
        let builder = retry::long_timeout_client()
            .post(url.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/vnd.amazon.eventstream")
            .body(body.clone());
        if api_key.is_empty() {
            builder.send()
        } else {
            builder
                .header("Authorization", format!("Bearer {}", api_key))
                .send()
        }
    })
    .await?;
    let byte_stream = Box::pin(resp.bytes_stream());
    Ok(futures::stream::unfold(
        BedrockStreamState {
            decoder: EventStreamDecoder::new(byte_stream),
            input: 0,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            finished: false,
        },
        // unfold owns the state between polls, so next_item consumes and
        // hands back the state instead of borrowing self.
        |st| async move { st.next_item().await },
    ))
}

// ---------------------------------------------------------------------------
// Shared small helpers (kept local to mirror anthropic.rs's structure).
// ---------------------------------------------------------------------------

/// strField reads a string field, treating null/missing as "".
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// num reads an i64 field, treating null/missing as 0.
fn num(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;
    use bytes::Bytes;

    /// encode_frame builds one eventstream frame the way AWS encodes
    /// them: big-endian prelude, typed string headers, JSON payload, and
    /// zeroed CRCs (the decoder never validates them).
    fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Bytes {
        let mut hdr = Vec::new();
        for (name, value) in headers {
            hdr.push(name.len() as u8);
            hdr.extend_from_slice(name.as_bytes());
            hdr.push(7); // header value type: string
            hdr.extend_from_slice(&(value.len() as u16).to_be_bytes());
            hdr.extend_from_slice(value.as_bytes());
        }
        let total = 12 + hdr.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total);
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0]); // prelude CRC (skipped)
        frame.extend_from_slice(&hdr);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&[0, 0, 0, 0]); // message CRC (skipped)
        assert_eq!(frame.len(), total);
        Bytes::from(frame)
    }

    fn decode_all(chunks: Vec<Bytes>) -> Vec<anyhow::Result<(String, String, Value)>> {
        let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, reqwest::Error>));
        let mut dec = EventStreamDecoder::new(stream);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut out = Vec::new();
                while let Some(item) = dec.next_frame().await {
                    out.push(item);
                }
                out
            })
    }

    #[test]
    fn parses_single_complete_frame() {
        let mut frames = decode_all(vec![encode_frame(
            &[(":message-type", "event"), (":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        )]);
        assert_eq!(frames.len(), 1);
        let (mtype, etype, payload) = frames.remove(0).unwrap();
        assert_eq!(mtype, "event");
        assert_eq!(etype, "messageStart");
        assert_eq!(payload["role"], "assistant");
    }

    #[test]
    fn recovers_every_event_across_split_reads() {
        // Regression shape for INC-014-style truncation: byte-level chunk
        // boundaries must never drop or corrupt a later frame.
        let mut wire: Vec<u8> = Vec::new();
        for i in 0..5 {
            let payload = format!(r#"{{"n":{i}}}"#);
            wire.extend_from_slice(&encode_frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockDelta"),
                ],
                payload.as_bytes(),
            ));
        }
        // Feed one byte at a time to force every possible boundary.
        let chunks: Vec<Bytes> = wire.iter().map(|b| Bytes::from(vec![*b])).collect();
        let frames = decode_all(chunks);
        assert_eq!(frames.len(), 5, "every frame must survive 1-byte reads");
        for (i, f) in frames.iter().enumerate() {
            let (_, etype, payload) = f.as_ref().unwrap();
            assert_eq!(etype, "contentBlockDelta");
            assert_eq!(payload["n"], i as i64);
        }
    }

    #[test]
    fn truncated_tail_yields_nothing_extra() {
        let full = encode_frame(
            &[(":message-type", "event"), (":event-type", "metadata")],
            br#"{"usage":{"inputTokens":3}}"#,
        );
        let frames = decode_all(vec![full.slice(..full.len() - 2)]);
        assert!(frames.is_empty(), "incomplete frame must not be emitted");
    }

    #[test]
    fn exception_frame_surfaces_as_error() {
        let mut frames = decode_all(vec![encode_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            br#"{"message":"Rate exceeded"}"#,
        )]);
        assert_eq!(frames.len(), 1);
        let (mtype, etype, _) = frames.remove(0).unwrap();
        assert_eq!(mtype, "exception");
        assert_eq!(etype, "throttlingException");
    }

    #[test]
    fn region_resolution_geo_and_ambient() {
        // eu. profile with a mismatched ambient region corrects to geo default.
        assert_eq!(
            resolve_bedrock_region("", "eu.anthropic.claude-sonnet-4-5"),
            "eu-west-1"
        );
        // apac. profile with serving ambient region keeps it.
        {
            let _g = crate::providers::testutil::set_env("AWS_REGION", "ap-northeast-1");
            assert_eq!(
                resolve_bedrock_region("", "apac.anthropic.claude-x"),
                "ap-northeast-1"
            );
            // au. profile rejects ap-northeast-1 and falls to the geo default.
            assert_eq!(
                resolve_bedrock_region("", "au.anthropic.claude-y"),
                "ap-southeast-2"
            );
        }
        // Plain model id with no ambient region → us-east-1.
        let _g = crate::providers::testutil::remove_env("AWS_REGION");
        let _g2 = crate::providers::testutil::remove_env("AWS_DEFAULT_REGION");
        assert_eq!(
            resolve_bedrock_region("", "anthropic.claude-z"),
            "us-east-1"
        );
        // Explicit host override wins over everything.
        assert_eq!(
            resolve_bedrock_region(
                "https://bedrock-runtime.eu-central-1.amazonaws.com",
                "us.anthropic.claude-w"
            ),
            "eu-central-1"
        );
    }

    #[test]
    fn converse_stream_url_encodes_model_id() {
        let url = converse_stream_url("", "anthropic.claude-sonnet-4-5");
        assert_eq!(
            url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-5/converse-stream"
        );
    }

    #[test]
    fn marshal_shapes_system_tools_and_tool_results() {
        let msgs = vec![
            Message {
                role: "system".into(),
                content: "be terse".into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                tool_calls: vec![ToolCall {
                    id: "call1".into(),
                    function: crate::types::FunctionCall {
                        name: "get_time".into(),
                        arguments: r#"{"tz":"utc"}"#.into(),
                    },
                    ..Default::default()
                }],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                content: "12:00".into(),
                tool_call_id: "call1".into(),
                ..Default::default()
            },
        ];
        let tools = vec![ToolDef {
            kind: "function".into(),
            function: crate::types::ToolDefFunction {
                name: "get_time".into(),
                description: "clock".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
        }];
        let body = marshal_bedrock_request(&msgs, &tools, "", "none", 8192).unwrap();
        assert_eq!(body["system"], serde_json::json!([{ "text": "be terse" }]));
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_time"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            3,
            "user + assistant(toolUse) + user(toolResult)"
        );
        assert_eq!(messages[1]["content"][0]["toolUse"]["name"], "get_time");
        assert_eq!(messages[1]["content"][0]["toolUse"]["input"]["tz"], "utc");
        assert_eq!(
            messages[2]["content"][0]["toolResult"]["toolUseId"],
            "call1"
        );
        assert_eq!(messages[2]["content"][0]["toolResult"]["status"], "success");
        // Unanswered calls must be dropped entirely.
        let msgs_unanswered = vec![
            Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "ghost".into(),
                    function: crate::types::FunctionCall {
                        name: "nope".into(),
                        arguments: "{}".into(),
                    },
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
        let body2 = marshal_bedrock_request(&msgs_unanswered, &[], "", "none", 8192).unwrap();
        assert_eq!(
            body2["messages"].as_array().unwrap().len(),
            1,
            "assistant turn with only unanswered calls disappears"
        );
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        assert_eq!(map_stop_reason("guardrail_intervened"), "stop");
        assert_eq!(map_stop_reason(""), "stop");
    }

    #[test]
    fn bedrock_claude_adaptive_splits_generations() {
        // Adaptive: Opus/Sonnet 4.6+, 5, Fable, Mythos, 4.7.
        for id in [
            "anthropic.claude-opus-4-6-v1",
            "us.anthropic.claude-opus-4-6-v1",
            "anthropic.claude-sonnet-4-6",
            "global.anthropic.claude-sonnet-4-6",
            "anthropic.claude-opus-4-7-v1",
            "anthropic.claude-opus-5",
            "eu.anthropic.claude-opus-5",
            "anthropic.claude-sonnet-5",
            "us.anthropic.claude-fable-5",
            "anthropic.claude-mythos-preview",
        ] {
            assert!(
                bedrock_claude_adaptive(id),
                "{id} should be adaptive-thinking"
            );
        }
        // Extended thinking: 3.7, 4, 4.1, 4.5 (incl. Haiku 4.5).
        for id in [
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "anthropic.claude-opus-4-1-20250805-v1:0",
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "anthropic.claude-opus-4-20250514-v1:0",
        ] {
            assert!(
                !bedrock_claude_adaptive(id),
                "{id} should use extended thinking"
            );
        }
    }

    #[test]
    fn bedrock_effort_caps_max_at_opus_46_and_5() {
        // "max" effort is Opus 4.6 / Opus 5 only; elsewhere it degrades.
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "max"), "max");
        assert_eq!(bedrock_effort("claude-opus-5", "max"), "max");
        assert_eq!(bedrock_effort("claude-sonnet-5", "max"), "high");
        assert_eq!(bedrock_effort("claude-sonnet-4-6", "max"), "high");
        assert_eq!(bedrock_effort("claude-fable-5", "max"), "high");
        assert_eq!(bedrock_effort("claude-opus-4-7", "max"), "high");
        // Named levels pass straight through; unknown defaults to high.
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "low"), "low");
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "minimal"), "low");
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "medium"), "medium");
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "high"), "high");
        assert_eq!(bedrock_effort("claude-opus-4-6-v1", "???"), "high");
    }

    #[test]
    fn bedrock_thinking_emits_adaptive_for_46_and_max_effort() {
        // Opus 4.6 + "max" → adaptive thinking, effort max, maxTokens only
        // (no pinned temperature on the newer models).
        let t = bedrock_thinking("us.anthropic.claude-opus-4-6-v1", "max", 32000)
            .expect("opus 4.6 max is adaptive");
        assert_eq!(t.additional["thinking"]["type"], "adaptive");
        assert_eq!(t.additional["output_config"]["effort"], "max");
        assert_eq!(t.inference["maxTokens"], 32000);
        assert!(t.inference.get("temperature").is_none());
    }

    #[test]
    fn bedrock_thinking_emits_extended_for_older_claude() {
        // Sonnet 4.5 + "high" → enabled + budget, temperature 1, budget <
        // maxTokens. Reuses anthropic::thinking_config's budget mapping.
        let t = bedrock_thinking("anthropic.claude-sonnet-4-5-20250929-v1:0", "high", 32000)
            .expect("sonnet 4.5 high is extended");
        assert_eq!(t.additional["thinking"]["type"], "enabled");
        let budget = t.additional["thinking"]["budget_tokens"].as_i64().unwrap();
        assert!(budget >= 1024 && budget < 32000, "budget {budget} must fit");
        assert_eq!(t.inference["maxTokens"], 32000);
        assert_eq!(t.inference["temperature"], 1.0);
    }

    #[test]
    fn bedrock_thinking_omits_for_non_claude_and_off() {
        // Non-Claude models reason natively and reject the thinking field.
        assert!(bedrock_thinking("deepseek.r1-v1:0", "high", 32000).is_none());
        assert!(bedrock_thinking("zai.glm-4.7-flash", "max", 32000).is_none());
        // Off / empty levels leave reasoning off on Claude too.
        assert!(bedrock_thinking("us.anthropic.claude-opus-4-6-v1", "", 32000).is_none());
        assert!(bedrock_thinking("us.anthropic.claude-opus-4-6-v1", "none", 32000).is_none());
        assert!(bedrock_thinking("us.anthropic.claude-opus-4-6-v1", "off", 32000).is_none());
    }

    #[test]
    fn marshal_bedrock_request_attaches_thinking_fields() {
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }];
        // Opus 4.6 max: adaptive thinking reaches the Converse body.
        let body =
            marshal_bedrock_request(&msgs, &[], "us.anthropic.claude-opus-4-6-v1", "max", 32000)
                .unwrap();
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "adaptive"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["effort"],
            "max"
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 32000);
        assert!(body["inferenceConfig"].get("temperature").is_none());
        // Off level: no thinking fields at all (unchanged request shape).
        let body_off =
            marshal_bedrock_request(&msgs, &[], "us.anthropic.claude-opus-4-6-v1", "none", 32000)
                .unwrap();
        assert!(body_off.get("additionalModelRequestFields").is_none());
        assert!(body_off.get("inferenceConfig").is_none());
    }

    #[test]
    fn marshal_drops_replayed_reasoning_without_signature() {
        // Bedrock 400s with "thinking.signature: Field required" when a
        // replayed assistant turn carries reasoningContent but no
        // signature (atom doesn't persist Claude's signature). The
        // marshaler must omit reasoningContent from prior assistant
        // turns entirely; Bedrock accepts an assistant turn without it.
        let msgs = vec![
            Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                content: "hello".into(),
                reasoning: "I considered greeting.".into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "again".into(),
                ..Default::default()
            },
        ];
        let body =
            marshal_bedrock_request(&msgs, &[], "us.anthropic.claude-opus-4-6-v1", "max", 32000)
                .unwrap();
        let messages = body["messages"].as_array().unwrap();
        // user + assistant(text only) + user: reasoning was dropped from
        // the replayed assistant turn, leaving just its text block.
        assert_eq!(messages.len(), 3);
        let asst = &messages[1]["content"];
        let asst_blocks = asst.as_array().unwrap();
        assert_eq!(asst_blocks.len(), 1, "replayed assistant keeps only text");
        assert_eq!(asst_blocks[0]["text"], "hello");
        assert!(
            !asst_blocks
                .iter()
                .any(|b| b.get("reasoningContent").is_some()),
            "no reasoningContent on a replayed assistant turn (no signature)"
        );
        // The current turn's thinking config is still attached.
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "adaptive"
        );
    }

    #[test]
    fn marshal_replays_reasoning_with_signature() {
        // When the prior assistant turn carries a signature (captured
        // during streaming), reasoning is replayed so thinking survives
        // across turns — Bedrock requires the signature on any thinking
        // block it receives back.
        let msgs = vec![
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
        ];
        let body =
            marshal_bedrock_request(&msgs, &[], "us.anthropic.claude-opus-4-6-v1", "max", 32000)
                .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let asst_blocks = messages[1]["content"].as_array().unwrap();
        // text block first, then the signed reasoningContent block.
        assert_eq!(asst_blocks.len(), 2);
        assert_eq!(asst_blocks[0]["text"], "hello");
        assert_eq!(
            asst_blocks[1]["reasoningContent"]["reasoningText"]["text"],
            "I considered greeting."
        );
        assert_eq!(
            asst_blocks[1]["reasoningContent"]["reasoningText"]["signature"],
            "sig-abc"
        );
    }

    #[tokio::test]
    async fn state_translates_converse_events_to_chunks() {
        // Drive BedrockStreamState directly through a synthetic frame
        // sequence covering text, reasoning, tool use, usage, and stop.
        let mk = |etype: &str, payload: String| {
            encode_frame(
                &[(":message-type", "event"), (":event-type", etype)],
                payload.as_bytes(),
            )
        };
        let mut wire: Vec<Bytes> = Vec::new();
        wire.push(mk("messageStart", r#"{"role":"assistant"}"#.into()));
        wire.push(mk(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#.into(),
        ));
        wire.push(mk(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"hmm"}}}"#.into(),
        ));
        wire.push(mk(
            "contentBlockStart",
            r#"{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"t1","name":"calc"}}}"#
                .into(),
        ));
        wire.push(mk(
            "contentBlockDelta",
            r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"x\":1}"}}}"#.into(),
        ));
        wire.push(mk(
            "metadata",
            r#"{"usage":{"inputTokens":11,"outputTokens":7,"totalTokens":18}}"#.into(),
        ));
        wire.push(mk("messageStop", r#"{"stopReason":"tool_use"}"#.into()));

        let stream = futures::stream::iter(wire.into_iter().map(Ok::<_, reqwest::Error>));
        let st = BedrockStreamState {
            decoder: EventStreamDecoder::new(stream),
            input: 0,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            finished: false,
        };
        let unfolded = futures::stream::unfold(st, |st| async move { st.next_item().await });
        let chunks: Vec<anyhow::Result<StreamChunk>> = unfolded.collect().await;

        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter_map(|c| c.choices.first().map(|ch| ch.delta.content.clone()))
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(texts.join(""), "Hello");

        let reasoning: String = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter_map(|c| c.choices.first().map(|ch| ch.delta.reasoning.clone()))
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(reasoning, "hmm");

        let tool_chunks: Vec<&StreamChunk> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .filter(|c| {
                c.choices
                    .first()
                    .map(|ch| !ch.delta.tool_calls.is_empty())
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(tool_chunks.len(), 2, "one start delta + one args delta");
        let start_tc = &tool_chunks[0].choices[0].delta.tool_calls[0];
        assert_eq!(start_tc.id, "t1");
        assert_eq!(start_tc.function.name, "calc");
        let args_tc = &tool_chunks[1].choices[0].delta.tool_calls[0];
        assert_eq!(args_tc.function.arguments, "{\"x\":1}");

        // Final chunk carries the finish reason plus accumulated usage.
        let last = chunks.last().unwrap().as_ref().unwrap();
        assert_eq!(last.choices[0].finish_reason, "tool_calls");
        let usage = last.usage.as_ref().expect("usage on final chunk");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
    }
}
