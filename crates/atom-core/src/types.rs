//! Wire types shared across the atom port, mirroring main.go's structs
//! exactly (field names and JSON shapes) so persisted sessions and the
//! client/server protocol stay compatible with the Go implementation.

use serde::{Deserialize, Serialize};

/// imageData: base64-encoded image attached to a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(rename = "mime")]
    pub mime: String,
    /// base64-encoded file bytes
    #[serde(rename = "data")]
    pub data: String,
}

/// Max raw image bytes ingested (paste/drop/read_file): 20MB.
pub const MAX_IMAGE_SOURCE_BYTES: usize = 20 << 20;
/// OpenCode-style attachment limits.
pub const MAX_IMAGE_DIM: u32 = 2000;
pub const MAX_IMAGE_BASE64_BYTES: usize = 5 << 20;
pub const MAX_PENDING_IMAGES: usize = 5;

/// A function call requested by the model (OpenAI tool_calls entry).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Streamed tool-call deltas fragment fields across chunks: a delta
    /// carrying only `arguments` has no `name`, and one starting a call
    /// has no `arguments`. Go's json.Unmarshal zero-filled both; keep the
    /// same tolerance here so stray fragments never fail the chunk parse.
    ///
    /// `null_as_default` is required (not just `#[serde(default)]`) for
    /// the same reason as StreamToolCallDelta.id: some providers (notably
    /// OpenCode Go's MiMo-v2.5) deliver fragment chunks with `"name":
    /// null`, which would otherwise fail the whole StreamChunk parse and
    /// get silently dropped by stream_chat.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::serde_null::string_or_object")]
    pub arguments: String,
}

/// One conversation entry. Content is plain text unless images are set,
/// in which case the JSON form switches to an OpenAI content array
/// (text part + image_url parts) — see the custom Serialize/Deserialize.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub images: Vec<ImageData>,
    pub reasoning: String,
    /// Claude/Bedrock opaque signature for the thinking block, so prior
    /// reasoning can be replayed across turns without a 400.
    pub reasoning_signature: String,
    /// provider or measured thinking duration
    pub reasoning_ms: i64,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    /// For tool-role messages: which provider served the call
    /// (search/fetch only, e.g. "exa", "direct", "mcp:<server>").
    pub tool_provider: String,
    pub diff: String,
    /// Who answered this message, so stats can attribute usage after a
    /// model switch.
    pub provider: String,
    pub model: String,
    /// Total wall-clock duration of the completed turn.
    pub duration_ms: i64,
    /// Generation speed of the final model round: completion tokens
    /// (reasoning included) divided by the first-token → stream-end
    /// window. 0 when unknown (provider reported no usage).
    pub tokens_per_sec: f64,
    /// Token count of the request that produced this message.
    pub usage: Option<StreamUsage>,
    /// When the message was written; absent for transcripts persisted
    /// before this field existed. The /fork overlay uses it to render
    /// the HH:MM trailing tag on each user-message row.
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn message_plain_fields(m: &Message) -> serde_json::Value {
    use serde_json::json;
    let mut v = json!({
        "role": m.role,
        "content": m.content,
    });
    let obj = v.as_object_mut().unwrap();
    if !m.reasoning.is_empty() {
        obj.insert("reasoning".into(), json!(m.reasoning));
    }
    if !m.reasoning_signature.is_empty() {
        obj.insert("reasoning_signature".into(), json!(m.reasoning_signature));
    }
    if m.reasoning_ms != 0 {
        obj.insert("reasoning_ms".into(), json!(m.reasoning_ms));
    }
    if !m.tool_calls.is_empty() {
        obj.insert(
            "tool_calls".into(),
            serde_json::to_value(&m.tool_calls).unwrap(),
        );
    }
    if !m.tool_call_id.is_empty() {
        obj.insert("tool_call_id".into(), json!(m.tool_call_id));
    }
    if !m.tool_provider.is_empty() {
        obj.insert("tool_provider".into(), json!(m.tool_provider));
    }
    if !m.diff.is_empty() {
        obj.insert("diff".into(), json!(m.diff));
    }
    if !m.provider.is_empty() {
        obj.insert("provider".into(), json!(m.provider));
    }
    if !m.model.is_empty() {
        obj.insert("model".into(), json!(m.model));
    }
    if m.duration_ms > 0 {
        obj.insert("duration_ms".into(), json!(m.duration_ms));
    }
    if m.tokens_per_sec > 0.0 {
        obj.insert("tokens_per_sec".into(), json!(m.tokens_per_sec));
    }
    if let Some(u) = &m.usage {
        obj.insert("usage".into(), serde_json::to_value(u).unwrap());
    }
    if let Some(ts) = m.created_at {
        obj.insert("created_at".into(), json!(ts.to_rfc3339()));
    }
    v
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.images.is_empty() {
            return message_plain_fields(self).serialize(s);
        }
        // OpenAI-style content array: optional text part then image_url parts.
        use serde_json::json;
        let mut parts = Vec::with_capacity(1 + self.images.len());
        if !self.content.is_empty() {
            parts.push(json!({"type": "text", "text": self.content}));
        }
        for img in &self.images {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", img.mime, img.data)},
            }));
        }
        let mut v = message_plain_fields(self);
        v["content"] = serde_json::Value::Array(parts);
        v.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            role: String,
            #[serde(default)]
            content: serde_json::Value,
            #[serde(default)]
            reasoning: String,
            #[serde(default)]
            reasoning_signature: String,
            #[serde(default, rename = "reasoning_ms")]
            reasoning_ms: i64,
            #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
            tool_calls: Vec<ToolCall>,
            #[serde(default)]
            tool_call_id: String,
            #[serde(default)]
            tool_provider: String,
            #[serde(default)]
            diff: String,
            #[serde(default)]
            provider: String,
            #[serde(default)]
            model: String,
            #[serde(default)]
            duration_ms: i64,
            #[serde(default)]
            tokens_per_sec: f64,
            #[serde(default)]
            usage: Option<StreamUsage>,
            #[serde(default)]
            created_at: Option<String>,
        }
        let mut raw = Raw::deserialize(d)?;
        let created_at = raw
            .created_at
            .take()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        let mut msg = Message {
            role: raw.role,
            content: String::new(),
            images: Vec::new(),
            reasoning: std::mem::take(&mut raw.reasoning),
            reasoning_signature: std::mem::take(&mut raw.reasoning_signature),
            reasoning_ms: raw.reasoning_ms,
            tool_calls: std::mem::take(&mut raw.tool_calls),
            tool_call_id: std::mem::take(&mut raw.tool_call_id),
            tool_provider: std::mem::take(&mut raw.tool_provider),
            diff: std::mem::take(&mut raw.diff),
            provider: std::mem::take(&mut raw.provider),
            model: std::mem::take(&mut raw.model),
            duration_ms: raw.duration_ms,
            tokens_per_sec: raw.tokens_per_sec,
            usage: raw.usage.take(),
            created_at,
        };
        match &raw.content {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => msg.content = s.clone(),
            serde_json::Value::Array(parts) => {
                for p in parts {
                    let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            msg.content += p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        }
                        "image_url" => {
                            if let Some(url) = p.pointer("/image_url/url").and_then(|u| u.as_str())
                            {
                                if let Some((mime, data)) = parse_data_url(url) {
                                    msg.images.push(ImageData { mime, data });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unexpected content form: {}",
                    other
                )))
            }
        }
        Ok(msg)
    }
}

/// Splits "data:<mime>;base64,<data>" into its parts.
pub fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (head, tail) = rest.split_once(";base64,")?;
    Some((head.to_string(), tail.to_string()))
}

/// stream_options asks the provider to include a usage object in the
/// final streamed chunk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(rename = "include_usage")]
    pub include_usage: bool,
}

/// streamUsage: provider-reported token count in OpenAI's usage shape.
/// Deserialize pulls extras from the fields routers actually send
/// (prompt_cache_hit_tokens, completion_tokens_details.reasoning_tokens,
/// prompt_tokens_details.cached_tokens, total_cost). Serialize emits the
/// canonical fields with omitempty semantics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning_tokens: i64,
    #[serde(skip_serializing_if = "is_zero", rename = "cache_read_tokens")]
    pub cache_read_tokens: i64,
    #[serde(skip_serializing_if = "is_zero", rename = "cache_write_tokens")]
    pub cache_write_tokens: i64,
    #[serde(skip_serializing_if = "is_zero_f")]
    pub cost: f64,
    /// Sum of prompt tokens across rounds in this session. Display-only;
    /// never serialized.
    #[serde(skip)]
    pub prompt_tokens_all: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn is_zero_f(n: &f64) -> bool {
    *n == 0.0
}

impl<'de> Deserialize<'de> for StreamUsage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            prompt_tokens: i64,
            completion_tokens: i64,
            total_tokens: i64,
            cache_read_tokens: i64,
            cache_write_tokens: i64,
            prompt_cache_hit_tokens: i64,
            prompt_cache_miss_tokens: i64,
            total_cost: f64,
            completion_tokens_details: Details,
            prompt_tokens_details: PromptDetails,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Details {
            reasoning_tokens: i64,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct PromptDetails {
            cached_tokens: i64,
        }
        let r = Raw::deserialize(d)?;
        Ok(StreamUsage {
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            total_tokens: r.total_tokens,
            reasoning_tokens: r.completion_tokens_details.reasoning_tokens,
            cache_read_tokens: first_positive(&[
                r.cache_read_tokens,
                r.prompt_cache_hit_tokens,
                r.prompt_tokens_details.cached_tokens,
            ]),
            cache_write_tokens: first_positive(&[r.cache_write_tokens, r.prompt_cache_miss_tokens]),
            cost: r.total_cost,
            prompt_tokens_all: 0,
        })
    }
}

/// firstPositive returns the first n > 0, or 0 if none are positive.
pub fn first_positive(ns: &[i64]) -> i64 {
    ns.iter().copied().find(|n| *n > 0).unwrap_or(0)
}

/// toolDef declares a function the model may call, in OpenAI
/// function-calling format. Parameters stays raw JSON schema verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolDefFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefFunction {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        ToolDef {
            kind: "function".into(),
            function: ToolDefFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// One tool-call fragment inside a streamed chunk.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamToolCallDelta {
    #[serde(default)]
    pub index: i64,
    // OpenCode Go's MiMo-v2.5 and a handful of other providers stream
    // a tool call in two phases: first a chunk with id/type/name set
    // and empty arguments, then fragment chunks with id/type/name as
    // JSON `null` and a partial arguments string. `#[serde(default)]`
    // alone only fills the field when the key is *absent*, not when
    // it is present-but-null, so without `null_as_default` every
    // fragment chunk fails to deserialize, `stream_chat` silently
    // drops it (Err(_) => continue), and the tool call ends up with
    // permanently empty arguments — the model then retries the same
    // broken call forever. null_as_default treats null as Default.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub id: String,
    #[serde(
        rename = "type",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub call_type: String,
    #[serde(default)]
    pub function: FunctionCall,
}

/// streamChunk: one SSE data payload (same schema opencode parses).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChunkChoice>,
    #[serde(default)]
    pub usage: Option<StreamUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamChunkChoice {
    #[serde(default)]
    pub delta: StreamDelta,
    #[serde(
        rename = "finish_reason",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub finish_reason: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamDelta {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub content: String,
    /// Ollama-style thinking field
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub reasoning: String,
    /// Claude/Bedrock opaque signature for a thinking block (streamed
    /// separately from the text); replayed with the text so reasoning
    /// survives across turns.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub reasoning_signature: String,
    /// OpenCode Go / DeepSeek-style field
    #[serde(
        rename = "reasoning_content",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub reasoning_content: String,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub tool_calls: Vec<StreamToolCallDelta>,
}

/// chatRequest body POSTed to /v1/chat/completions.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "String::is_empty", rename = "reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stream_options")]
    pub stream_options: Option<StreamOptions>,
}

/// streamResult is the outcome of one streaming turn.
#[derive(Debug, Clone, Default)]
pub struct StreamResult {
    pub content: String,
    pub reasoning: String,
    /// Claude/Bedrock opaque signature, captured during streaming and
    /// replayed with the reasoning text so thinking survives across turns.
    pub reasoning_signature: String,
    pub reasoning_ms: i64,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<StreamUsage>,
    pub finish_reason: String,
    /// Server-measured time to first token: stream start → first delta.
    pub ttft_ms: i64,
    /// First token → stream end. Together with ttft_ms this spans the
    /// whole model round that tokens-per-second is measured over.
    pub gen_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_chunk_accepts_null_openai_delta_fields() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"hello","reasoning":null,"tool_calls":null},"finish_reason":null}]}"#,
        )
        .unwrap();
        let choice = &chunk.choices[0];
        assert_eq!(choice.delta.content, "hello");
        assert!(choice.delta.reasoning.is_empty());
        assert!(choice.delta.tool_calls.is_empty());
        assert!(choice.finish_reason.is_empty());
    }

    /// Some OpenCode Zen free-tier routers stream tool-call arguments as
    /// an actual JSON object instead of a string. The chunk must decode
    /// with the object re-serialized to its compact string form; when it
    /// didn't, the whole chunk was dropped and the tool call survived
    /// with empty arguments, sending the model into a retry loop.
    #[test]
    fn stream_chunk_accepts_object_tool_arguments() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_718d","type":"function","function":{"name":"grep","arguments":{"pattern":"version"}}}]}}]}"#,
        )
        .unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.function.name, "grep");
        assert_eq!(tc.function.arguments, r#"{"pattern":"version"}"#);
    }

    /// The same routers also send `"arguments": null` placeholders; those
    /// must decode as empty strings rather than failing the chunk.
    #[test]
    fn stream_chunk_accepts_null_tool_arguments() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"grep","arguments":null}}]}}]}"#,
        )
        .unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.function.name, "grep");
        assert!(tc.function.arguments.is_empty());
    }

    #[test]
    fn message_round_trip_plain() {
        let m = Message {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["content"], serde_json::json!("hi"));
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn assistant_turn_metadata_round_trips() {
        let m = Message {
            role: "assistant".into(),
            content: "answer".into(),
            model: "model-b".into(),
            duration_ms: 134_600,
            ..Default::default()
        };
        let value = serde_json::to_value(&m).unwrap();

        assert_eq!(value["model"], "model-b");
        assert_eq!(value["duration_ms"], 134_600);
        assert_eq!(serde_json::from_value::<Message>(value).unwrap(), m);
    }

    #[test]
    fn message_with_images_uses_content_array() {
        let m = Message {
            role: "user".into(),
            content: "look".into(),
            images: vec![ImageData {
                mime: "image/png".into(),
                data: "QUJD".into(),
            }],
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v["content"].is_array());
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(
            v["content"][1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn usage_parses_router_variants() {
        let u: StreamUsage = serde_json::from_str(
            r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,
                "prompt_cache_hit_tokens":7,"completion_tokens_details":{"reasoning_tokens":3},
                "total_cost":0.25}"#,
        )
        .unwrap();
        assert_eq!(u.cache_read_tokens, 7);
        assert_eq!(u.reasoning_tokens, 3);
        assert_eq!(u.cost, 0.25);
    }
}
