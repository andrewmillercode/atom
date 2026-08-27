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
    #[serde(default)]
    pub name: String,
    #[serde(default)]
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
    pub diff: String,
    /// Who answered this message, so stats can attribute usage after a
    /// model switch.
    pub provider: String,
    pub model: String,
    /// Total wall-clock duration of the completed turn.
    pub duration_ms: i64,
    /// Token count of the request that produced this message.
    pub usage: Option<StreamUsage>,
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
    if let Some(u) = &m.usage {
        obj.insert("usage".into(), serde_json::to_value(u).unwrap());
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
            diff: String,
            #[serde(default)]
            provider: String,
            #[serde(default)]
            model: String,
            #[serde(default)]
            duration_ms: i64,
            #[serde(default)]
            usage: Option<StreamUsage>,
        }
        let mut raw = Raw::deserialize(d)?;
        let mut msg = Message {
            role: raw.role,
            content: String::new(),
            images: Vec::new(),
            reasoning: std::mem::take(&mut raw.reasoning),
            reasoning_signature: std::mem::take(&mut raw.reasoning_signature),
            reasoning_ms: raw.reasoning_ms,
            tool_calls: std::mem::take(&mut raw.tool_calls),
            tool_call_id: std::mem::take(&mut raw.tool_call_id),
            diff: std::mem::take(&mut raw.diff),
            provider: std::mem::take(&mut raw.provider),
            model: std::mem::take(&mut raw.model),
            duration_ms: raw.duration_ms,
            usage: raw.usage.take(),
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
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
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
