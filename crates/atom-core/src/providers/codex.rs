//! OpenAI Codex (ChatGPT backend) responses API client, ported from
//! openai_codex.go. Request marshaling converts atom's chat history to
//! the responses "input" shape; the SSE event stream is parsed into a
//! StreamResult plus NDJSON-shaped events for the caller to relay
//! (Go's version wrote them straight to the HTTP client).

use super::auth::{auth_bearer, lookup_auth_entry, AuthEntry};
use crate::types::{FunctionCall, Message, StreamResult, StreamUsage, ToolCall, ToolDef};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

fn responses_url() -> String {
    RESPONSES_URL.read().unwrap().to_string()
}

static RESPONSES_URL: RwLock<&'static str> =
    RwLock::new("https://chatgpt.com/backend-api/codex/responses?client_version=0.144.6");

#[doc(hidden)]
pub fn set_responses_url_for_test(url: &str) {
    // Leak is fine: only tests override this, a handful of times.
    let leaked: &'static str = Box::leak(url.to_string().into_boxed_str());
    *RESPONSES_URL.write().unwrap() = leaked;
}

static CODEX_SESSION_ID: once_cell::sync::OnceCell<String> = once_cell::sync::OnceCell::new();

pub fn openai_codex_session() -> String {
    CODEX_SESSION_ID.get_or_init(random_uuidv4).clone()
}

pub fn random_uuidv4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&b[0..4]),
        hex::encode(&b[4..6]),
        hex::encode(&b[6..8]),
        hex::encode(&b[8..10]),
        hex::encode(&b[10..])
    )
}

pub fn openai_codex_auth_for_key(key: &str) -> Option<AuthEntry> {
    if key.is_empty() {
        return None;
    }
    let e = lookup_auth_entry("openai")?;
    if e.r#type != "oauth" {
        return None;
    }
    if auth_bearer("openai", &e) != key {
        return None;
    }
    Some(e)
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenAICodexTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAICodexReasoning {
    effort: String,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAICodexRequest {
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    input: Vec<serde_json::Value>,
    tools: Vec<OpenAICodexTool>,
    parallel_tool_calls: bool,
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAICodexReasoning>,
}

pub fn flatten_openai_codex_tools(tools: &[ToolDef]) -> Vec<OpenAICodexTool> {
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let name = &t.function.name;
        if name.is_empty() {
            continue;
        }
        out.push(OpenAICodexTool {
            kind: "function".into(),
            name: name.clone(),
            description: t.function.description.clone(),
            parameters: if t.function.parameters.is_null() {
                None
            } else {
                Some(t.function.parameters.clone())
            },
        });
    }
    out
}

pub fn openai_codex_instructions_and_input(msgs: &[Message]) -> (String, Vec<serde_json::Value>) {
    let mut sys: Vec<String> = Vec::new();
    let mut input: Vec<serde_json::Value> = Vec::new();
    for m in msgs {
        match m.role.as_str() {
            "system" => {
                if !m.content.trim().is_empty() {
                    sys.push(m.content.clone());
                }
            }
            "user" => {
                let mut content = Vec::with_capacity(1 + m.images.len());
                if !m.content.is_empty() {
                    content.push(serde_json::json!({
                        "type": "input_text",
                        "text": m.content,
                    }));
                }
                for img in &m.images {
                    content.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", img.mime, img.data),
                    }));
                }
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": content,
                }));
            }
            "assistant" => {
                if !m.content.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": m.content}],
                    }));
                }
                for tc in &m.tool_calls {
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }));
                }
            }
            "tool" => {
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id,
                    "output": m.content,
                }));
            }
            _ => {}
        }
    }
    (sys.join("\n"), input)
}

pub fn marshal_openai_codex_request(
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
    thinking: &str,
) -> anyhow::Result<Vec<u8>> {
    let (instr, input) = openai_codex_instructions_and_input(msgs);
    let req = OpenAICodexRequest {
        model: model.to_string(),
        instructions: instr,
        input,
        tools: flatten_openai_codex_tools(tools),
        parallel_tool_calls: false,
        store: false,
        stream: true,
        reasoning: if !thinking.is_empty() {
            Some(OpenAICodexReasoning {
                effort: thinking.to_string(),
            })
        } else {
            None
        },
    };
    Ok(serde_json::to_vec(&req)?)
}

pub async fn post_openai_codex(auth: &AuthEntry, body: &[u8]) -> anyhow::Result<reqwest::Response> {
    let client = super::retry::long_timeout_client();
    // The closure rebuilds the request on every retry attempt (Go's
    // newReq pattern), so headers are re-applied per attempt.
    super::retry::do_http_with_retry(|| {
        let mut inner = client
            .post(responses_url())
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", auth_bearer("openai", auth)),
            )
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "atom")
            .header("session_id", openai_codex_session())
            .body(body.to_vec());
        if let Some(id) = auth.metadata.as_ref().and_then(|m| m.get("account_id")) {
            inner = inner.header("ChatGPT-Account-Id", id);
        }
        inner.send()
    })
    .await
}

/// Outcome of one codex round: parsed result, relayable events, and the
/// (possibly refreshed) bearer key.
#[derive(Debug, Clone, Default)]
pub struct CodexRoundOutcome {
    pub result: StreamResult,
    pub events: Vec<serde_json::Value>,
    pub key: String,
}

pub async fn do_openai_codex_round(key: &str, body: &[u8]) -> anyhow::Result<CodexRoundOutcome> {
    let mut key = key.to_string();
    let mut auth = openai_codex_auth_for_key(&key)
        .ok_or_else(|| anyhow::anyhow!("openai oauth credentials missing"))?;
    let mut refreshed = false;
    loop {
        match post_openai_codex(&auth, body).await {
            Err(err) => {
                if let Some(pe) = err.downcast_ref::<super::retry::ProviderHTTPError>() {
                    if pe.status_code == 401 && !refreshed {
                        let live =
                            super::oauth::ensure_openai_auth_opt(true)
                                .await
                                .map_err(|e| {
                                    anyhow::anyhow!(
                                        "openai oauth refresh: {}",
                                        super::oauth::redact_oauth_text(&e.to_string())
                                    )
                                })?;
                        key = auth_bearer("openai", &live);
                        auth = live;
                        refreshed = true;
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "{}: {}",
                        pe.status,
                        super::oauth::redact_oauth_text(&pe.body)
                    ));
                }
                return Err(err);
            }
            Ok(resp) => {
                let lines = super::providers::SseLineReader::new(Box::pin(resp.bytes_stream()));
                let outcome = stream_openai_codex(lines).await?;
                return Ok(CodexRoundOutcome {
                    result: outcome.result,
                    events: outcome.events,
                    key,
                });
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct CodexEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    delta: String,
    #[serde(default, rename = "item_id")]
    item_id: String,
    #[serde(default)]
    duration_ms: f64,
    #[serde(default)]
    elapsed_ms: f64,
    #[serde(default)]
    item: Option<CodexEventItem>,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    response: Option<CodexEventResponse>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexEventItem {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default, rename = "call_id")]
    call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    duration_ms: f64,
    #[serde(default)]
    elapsed_ms: f64,
}

#[derive(Debug, Deserialize, Default)]
struct CodexEventResponse {
    #[serde(default)]
    usage: Option<CodexEventUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct CodexEventUsage {
    #[serde(default, rename = "input_tokens")]
    input_tokens: i64,
    #[serde(default, rename = "output_tokens")]
    output_tokens: i64,
    #[serde(default, rename = "total_tokens")]
    total_tokens: i64,
}

/// openaiDurationMs returns the first positive duration value, in whole
/// milliseconds (0 when none).
pub fn openai_duration_ms(values: &[f64]) -> i64 {
    // Go: time.Duration(v*float64(time.Millisecond)).Milliseconds() —
    // i.e. the provider's millisecond count truncated to an integer.
    for v in values {
        if *v > 0.0 {
            return *v as i64;
        }
    }
    0
}

#[derive(Debug, Clone, Default)]
pub struct CodexStreamOutcome {
    pub result: StreamResult,
    pub events: Vec<serde_json::Value>,
}

struct Emitter {
    saw_reasoning: bool,
    reasoning_started: Option<Instant>,
    reasoning_ms: i64,
    events: Vec<serde_json::Value>,
}

impl Emitter {
    fn new() -> Self {
        Emitter {
            saw_reasoning: false,
            reasoning_started: None,
            reasoning_ms: 0,
            events: Vec::new(),
        }
    }

    fn emit_reasoning_start(&mut self) {
        if self.saw_reasoning {
            return;
        }
        self.events.push(serde_json::json!({"type": "reasoning"}));
        self.saw_reasoning = true;
        self.reasoning_started = Some(Instant::now());
    }

    fn emit_reasoning(&mut self, text: &str) {
        self.emit_reasoning_start();
        if text.is_empty() {
            return;
        }
        self.events
            .push(serde_json::json!({"type": "reasoning", "text": text}));
    }

    fn emit_reasoning_end(&mut self, dur_ms: i64) {
        if !self.saw_reasoning {
            return;
        }
        let mut dur = dur_ms;
        if dur <= 0 {
            if let Some(t) = self.reasoning_started {
                dur = t.elapsed().as_millis() as i64;
            }
        }
        let mut ev = serde_json::json!({"type": "reasoning_end"});
        if dur > 0 {
            ev["duration_ms"] = serde_json::Value::String(dur.to_string());
            self.reasoning_ms += dur;
        }
        self.events.push(ev);
        self.saw_reasoning = false;
        self.reasoning_started = None;
    }

    fn emit_content(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.emit_reasoning_end(0);
        self.events
            .push(serde_json::json!({"type": "content", "text": text}));
    }
}

fn ensure_call<'a>(
    calls_by_item: &'a mut HashMap<String, ToolCall>,
    call_order: &mut Vec<String>,
    item_id: &str,
    call_id: &str,
    name: &str,
    args: &str,
) -> &'a mut ToolCall {
    let existing = calls_by_item.get_mut(item_id);
    match existing {
        None => {
            let tc = ToolCall {
                id: call_id.to_string(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: args.to_string(),
                },
            };
            calls_by_item.insert(item_id.to_string(), tc);
            call_order.push(item_id.to_string());
        }
        Some(tc) => {
            if !call_id.is_empty() {
                tc.id = call_id.to_string();
            }
            if !name.is_empty() {
                tc.function.name = name.to_string();
            }
            if !args.is_empty() && tc.function.arguments.is_empty() {
                tc.function.arguments = args.to_string();
            }
        }
    }
    calls_by_item.get_mut(item_id).unwrap()
}

/// streamOpenAICodexToClient's parser half: consumes SSE lines and
/// returns the accumulated result plus relayable NDJSON events.
pub async fn stream_openai_codex<S>(mut lines: S) -> anyhow::Result<CodexStreamOutcome>
where
    S: futures::Stream<Item = anyhow::Result<String>> + Unpin,
{
    let mut reply = String::new();
    let mut reasoning = String::new();
    let mut usage: Option<StreamUsage> = None;
    let mut em = Emitter::new();
    let stream_started_at = Instant::now();
    let mut first_token_at: Option<Instant> = None;
    let mut item_kind: HashMap<String, String> = HashMap::new();
    let mut calls_by_item: HashMap<String, ToolCall> = HashMap::new();
    let mut call_order: Vec<String> = Vec::new();

    while let Some(line) = lines.next().await {
        let line = line.unwrap_or_default();
        let trim = line.trim();
        if trim.is_empty() || trim.starts_with(':') {
            continue;
        }
        let data = match sse_data(trim) {
            Some(d) => d,
            None => continue,
        };
        if data == "[DONE]" {
            break;
        }
        let ev: CodexEvent = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match ev.kind.as_str() {
            "response.output_item.added" | "response.output_item.done" => {
                let item = match &ev.item {
                    Some(i) => i,
                    None => continue,
                };
                let id = item.id.clone();
                item_kind.insert(id.clone(), item.kind.clone());
                if item.kind == "function_call" {
                    ensure_call(
                        &mut calls_by_item,
                        &mut call_order,
                        &id,
                        &item.call_id,
                        &item.name,
                        &item.arguments,
                    );
                }
                if item.kind == "reasoning" {
                    em.emit_reasoning_start();
                    if ev.kind == "response.output_item.done" {
                        let dur = openai_duration_ms(&[
                            item.duration_ms,
                            item.elapsed_ms,
                            ev.duration_ms,
                            ev.elapsed_ms,
                        ]);
                        em.emit_reasoning_end(dur);
                    }
                } else if ev.kind == "response.output_item.added" {
                    em.emit_reasoning_end(0);
                }
            }
            "response.reasoning.elapsed" => {
                em.emit_reasoning_start();
            }
            "response.output_text.delta" => {
                if item_kind.get(&ev.item_id).map(String::as_str) == Some("reasoning") {
                    em.emit_reasoning(&ev.delta);
                    reasoning.push_str(&ev.delta);
                } else {
                    em.emit_content(&ev.delta);
                    reply.push_str(&ev.delta);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                em.emit_reasoning(&ev.delta);
                reasoning.push_str(&ev.delta);
            }
            "response.function_call_arguments.delta" => {
                let tc = ensure_call(&mut calls_by_item, &mut call_order, &ev.item_id, "", "", "");
                tc.function.arguments.push_str(&ev.delta);
            }
            "response.function_call_arguments.done" => {
                let tc = ensure_call(&mut calls_by_item, &mut call_order, &ev.item_id, "", "", "");
                if !ev.arguments.is_empty() {
                    tc.function.arguments = ev.arguments.clone();
                }
            }
            "response.completed" => {
                if let Some(u) = ev.response.and_then(|r| r.usage) {
                    usage = Some(StreamUsage {
                        prompt_tokens: u.input_tokens,
                        completion_tokens: u.output_tokens,
                        total_tokens: u.total_tokens,
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
        // First delta of any kind ends the time-to-first-token window;
        // generation speed is measured from here to stream end.
        if first_token_at.is_none()
            && (!reply.is_empty() || !reasoning.is_empty() || !call_order.is_empty())
        {
            first_token_at = Some(Instant::now());
        }
    }
    em.emit_reasoning_end(0);

    let (ttft_ms, gen_ms) = match first_token_at {
        Some(first) => (
            first
                .saturating_duration_since(stream_started_at)
                .as_millis()
                .min(i64::MAX as u128) as i64,
            Instant::now()
                .saturating_duration_since(first)
                .as_millis()
                .min(i64::MAX as u128) as i64,
        ),
        None => (0, 0),
    };
    let mut calls = Vec::with_capacity(call_order.len());
    for id in &call_order {
        if let Some(mut tc) = calls_by_item.remove(id) {
            if tc.call_type.is_empty() {
                tc.call_type = "function".into();
            }
            calls.push(tc);
        }
    }
    Ok(CodexStreamOutcome {
        result: StreamResult {
            content: reply,
            reasoning,
            reasoning_signature: String::new(),
            reasoning_ms: em.reasoning_ms,
            tool_calls: calls,
            usage,
            finish_reason: String::new(),
            ttft_ms,
            gen_ms,
        },
        events: em.events,
    })
}

// SSE line decoding lives with the streaming client (providers.rs).
use super::providers::sse_data;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageData;

    fn mk_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn message_and_tool_conversion() {
        let msgs = vec![
            Message {
                role: "system".into(),
                content: "be terse".into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "hi".into(),
                images: vec![ImageData {
                    mime: "image/png".into(),
                    data: "xx".into(),
                }],
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                content: "calling".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                tool_calls: vec![mk_tool_call("c1", "bash", r#"{"command":"ls"}"#)],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "c1".into(),
                content: "ok".into(),
                ..Default::default()
            },
        ];
        let td = ToolDef::new("bash", "run", serde_json::json!({"type":"object"}));

        let body = marshal_openai_codex_request("gpt-5", &msgs, &[td], "medium").unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(raw["model"], "gpt-5");
        assert_eq!(raw["instructions"], "be terse");
        assert_eq!(raw["stream"], true);
        assert_eq!(raw["store"], false);
        assert_eq!(raw["parallel_tool_calls"], false);
        assert!(raw.get("stream_options").is_none());
        assert!(raw.get("temperature").is_none());
        assert_eq!(raw["reasoning"]["effort"], "medium");
        let input = raw["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "input: {}", String::from_utf8_lossy(&body));
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,xx"
        );
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[2]["name"], "bash");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["output"], "ok");
        let tools = raw["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "bash");

        let body2 = marshal_openai_codex_request("gpt-5", &[], &[], "").unwrap();
        let raw2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
        assert!(
            raw2.get("reasoning").is_none(),
            "empty thinking should omit reasoning"
        );
    }

    fn sse_stream(body: &str) -> futures::stream::Iter<std::vec::IntoIter<anyhow::Result<String>>> {
        let lines: Vec<anyhow::Result<String>> =
            body.split('\n').map(|l| Ok(l.to_string())).collect();
        futures::stream::iter(lines)
    }

    #[tokio::test]
    async fn stream_sse_happy_path() {
        let sse = [
            ": comment ignored",
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"think"}"#,
            r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"bash"}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n");
        let outcome = stream_openai_codex(sse_stream(&sse)).await.unwrap();
        let result = &outcome.result;
        assert_eq!(result.content, "Hello");
        assert_eq!(result.reasoning, "think");
        assert_eq!(result.tool_calls.len(), 1);
        let tc = &result.tool_calls[0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.call_type, "function");
        assert_eq!(tc.function.name, "bash");
        assert_eq!(tc.function.arguments, r#"{"command":"ls"}"#);
        let u = result.usage.as_ref().unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (10, 2, 12)
        );
        let out = serde_json::to_string(&outcome.events).unwrap();
        assert!(out.contains(r#""type":"reasoning""#), "{}", out);
        assert!(out.contains(r#""type":"content""#), "{}", out);
    }

    #[tokio::test]
    async fn stream_silent_reasoning_uses_provider_timings() {
        let sse = [
            r#"data: {"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning"}}"#,
            r#"data: {"type":"response.output_item.done","item":{"id":"rs_1","type":"reasoning","duration_ms":8300}}"#,
            r#"data: {"type":"response.output_text.delta","delta":"ok"}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n");
        let outcome = stream_openai_codex(sse_stream(&sse)).await.unwrap();
        let result = &outcome.result;
        assert_eq!(result.content, "ok");
        assert_eq!(result.reasoning, "", "silent reasoning has no text");
        assert_eq!(result.reasoning_ms, 8300);
        let out = serde_json::to_string(&outcome.events).unwrap();
        assert!(
            out.contains(r#""type":"reasoning""#),
            "missing start: {}",
            out
        );
        assert!(
            out.contains(r#""duration_ms":"8300""#),
            "missing provider duration: {}",
            out
        );
    }

    #[tokio::test]
    async fn auth_for_key_api_skipped_and_oauth_matched() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("codex-authkey");

        super::super::auth::set_auth(
            "openai",
            AuthEntry {
                r#type: "api".into(),
                key: "sk-api".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(openai_codex_auth_for_key("").is_none(), "empty key");
        assert!(
            openai_codex_auth_for_key("sk-api").is_none(),
            "api key must not use Codex path"
        );
        super::super::auth::set_auth(
            "openai",
            AuthEntry {
                r#type: "oauth".into(),
                access: "acc".into(),
                refresh: "r".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            openai_codex_auth_for_key("acc").is_some(),
            "oauth access should match"
        );
        assert!(
            openai_codex_auth_for_key("other").is_none(),
            "unrelated key"
        );
    }

    #[test]
    fn uuid_v4_shape() {
        let u = random_uuidv4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(u.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }
}
