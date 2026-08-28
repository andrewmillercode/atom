//! Conversation compaction folds older session turns into a short summary
//! once the last reported prompt is large enough. The TUI still shows the
//! full Messages transcript; only the context sent to the model shrinks.
//! Ported from compaction.go (pure logic plus one async HTTP call).

use crate::session::store::Session;
use crate::types::{ChatRequest, ImageData, Message, StreamUsage};
use anyhow::anyhow;
use serde::Deserialize;
use std::time::{Duration, Instant};

/// compactionTokenThreshold is the default last-reported prompt size that
/// triggers a compact. Equal to the threshold is not enough: we wait until
/// the session has clearly grown past it. Small-window models compact
/// earlier: see compactionThreshold.
pub const COMPACTION_TOKEN_THRESHOLD: i64 = 150_000;

/// compactionHeadroomTokens is kept below the model's context window when
/// the auto-compact threshold is derived from it, so the summarizer round
/// (prompt plus summary output) still fits before the provider starts
/// rejecting requests.
pub const COMPACTION_HEADROOM_TOKENS: i64 = 20_000;

/// compactionThreshold is the prompt size that triggers an auto-compact
/// for a model whose context window holds window tokens: the smaller of
/// the fixed threshold and window minus headroom. Windows too small to
/// leave any headroom fall back to the fixed threshold rather than a
/// zero or negative one that would fold every round.
pub fn compaction_threshold(context_window: i64) -> i64 {
    if context_window <= COMPACTION_HEADROOM_TOKENS {
        return COMPACTION_TOKEN_THRESHOLD;
    }
    (context_window - COMPACTION_HEADROOM_TOKENS).min(COMPACTION_TOKEN_THRESHOLD)
}

/// Built-in fallback used when compaction has not been configured yet.
pub const COMPACTION_MODEL_ID: &str = crate::config::DEFAULT_COMPACTION_MODEL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionTarget {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub key: String,
}

/// compactionToolResultLimit caps how much of a tool result is serialized
/// into the summary prompt so one huge read cannot dominate the request.
pub const COMPACTION_TOOL_RESULT_LIMIT: usize = 2000;

pub const COMPACTION_SUMMARY_PREAMBLE: &str = "Previous conversation summary:\n\n";
pub const COMPACTION_SUMMARY_ACK: &str = "Understood. I will continue from this summary.";

/// compactionSystemPrompt asks the summarizer for a structured brief, not
/// a reply that continues the conversation.
pub const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting a coding-agent conversation into a structured summary. Do not continue the conversation or answer the user. Produce only the summary.\n\nUse this outline:\n\n## Goal\n## Constraints & Preferences\n## Progress (Done / In Progress / Blocked)\n## Key Decisions\n## Next Steps\n## Critical Context\n\nPreserve the user's latest intent and any critical file paths, decisions, and unfinished work. If a previous summary is included, update it rather than discarding it.";

/// errNothingToCompact is returned when every message is already folded
/// (or the only leftover is a trailing user turn kept for the next send).
#[derive(Debug, thiserror::Error)]
#[error("nothing to compact")]
pub struct NothingToCompact;

/// True when err is the nothing-to-compact condition.
pub fn is_nothing_to_compact(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NothingToCompact>().is_some()
}

/// Resolves the configured compaction provider and model. Catalog-backed
/// providers keep their configured endpoint even when their credential is
/// currently absent, so the resulting request fails honestly instead of
/// silently changing providers. Unknown provider ids fall back to local
/// Ollama; no preference is written implicitly.
pub async fn compaction_target() -> CompactionTarget {
    let selected = crate::config::load().resolved_compaction();
    crate::providers::modelsdev::ensure_models_dev_catalog().await;
    let providers = crate::providers::providers::build_providers().await;
    let provider = if let Some(provider) = providers
        .iter()
        .find(|provider| provider.name == selected.provider || provider.id == selected.provider)
        .cloned()
    {
        provider
    } else if matches!(selected.provider.as_str(), "ollama" | "ollama-cloud") {
        crate::providers::providers::Provider {
            name: "ollama".into(),
            id: "ollama-cloud".into(),
            base_url: "https://ollama.com/v1".into(),
            key: crate::providers::auth::load_provider_key("ollama-cloud").await,
            reasoning_field: "reasoning".into(),
        }
    } else {
        let base_url = crate::providers::modelsdev::models_dev_base_url(&selected.provider);
        if base_url.is_empty() {
            crate::providers::providers::Provider {
                name: "ollama-local".into(),
                base_url: "http://localhost:11434/v1".into(),
                reasoning_field: "reasoning".into(),
                ..Default::default()
            }
        } else {
            crate::providers::providers::Provider {
                name: selected.provider.clone(),
                id: selected.provider.clone(),
                key: crate::providers::auth::load_provider_key(&selected.provider).await,
                reasoning_field: crate::providers::providers::reasoning_field_for_url(&base_url),
                base_url,
            }
        }
    };
    CompactionTarget {
        provider: if provider.id.is_empty() {
            provider.name.clone()
        } else {
            provider.id.clone()
        },
        model: selected.model,
        base_url: provider.base_url,
        key: provider.key,
    }
}

/// Compatibility helper for callers that only need endpoint credentials.
pub async fn compaction_provider() -> (String, String) {
    let target = compaction_target().await;
    (target.base_url, target.key)
}

/// shouldCompact reports whether the session's last provider usage is
/// large enough to fold. A missing Usage means we have no signal yet, so
/// we must not compact.
pub fn should_compact(sess: &Session) -> bool {
    match &sess.usage {
        Some(u) => u.prompt_tokens > COMPACTION_TOKEN_THRESHOLD,
        None => false,
    }
}

/// shouldCompactWithThreshold is shouldCompact against a caller-computed
/// threshold (see compaction_threshold). The session's model context
/// window decides it, so small-window models fold earlier than the fixed
/// threshold.
pub fn should_compact_with_threshold(sess: &Session, threshold: i64) -> bool {
    match &sess.usage {
        Some(u) => u.prompt_tokens > threshold,
        None => false,
    }
}

/// clampIndex keeps i inside [0, n] so CompactedThrough cannot panic
/// after a session is edited or loaded with a stale index.
fn clamp_index(i: i64, n: usize) -> usize {
    if i < 0 {
        return 0;
    }
    if i as usize > n {
        return n;
    }
    i as usize
}

/// compactSpan is the half-open range of Messages that should be folded.
/// A trailing user message is left out so the current question is sent
/// verbatim after the summary.
pub fn compact_span(sess: &Session) -> Option<(usize, usize)> {
    let n = sess.messages.len();
    let start = clamp_index(sess.compacted_through, n);
    let mut end = n;
    while end > start {
        let role = sess.messages[end - 1].role.as_str();
        if role == "compaction" {
            end -= 1;
            continue;
        }
        if role == "user" || role == "nudge" || role == "stopped" {
            end -= 1;
        }
        break;
    }
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// llmMessages builds the context for a model request: fresh instructions,
/// an optional compaction brief, then only the unsummarized tail.
pub fn llm_messages(sess: &Session) -> Vec<Message> {
    let mut msgs: Vec<Message> = sess.instructions.clone();
    if !sess.compaction_summary.is_empty() {
        msgs.push(Message {
            role: "user".into(),
            content: format!("{}{}", COMPACTION_SUMMARY_PREAMBLE, sess.compaction_summary),
            ..Default::default()
        });
        msgs.push(Message {
            role: "assistant".into(),
            content: COMPACTION_SUMMARY_ACK.into(),
            ..Default::default()
        });
    }
    let start = clamp_index(sess.compacted_through, sess.messages.len());
    for m in &sess.messages[start..] {
        if m.role == "compaction" {
            continue;
        }
        if m.role == "nudge" || m.role == "stopped" {
            msgs.push(Message {
                role: "user".into(),
                content: m.content.clone(),
                ..Default::default()
            });
            continue;
        }
        msgs.push(m.clone());
    }
    sanitize_messages(&msgs)
}

/// serializeConversation renders history as labeled prose so the
/// summarizer does not treat it as a live chat to continue.
pub fn serialize_conversation(msgs: &[Message], previous_summary: &str) -> String {
    let mut b = String::new();
    if !previous_summary.is_empty() {
        b.push_str("Previous summary:\n");
        b.push_str(previous_summary);
        b.push_str("\n\n");
    }
    for m in msgs {
        match m.role.as_str() {
            "compaction" => continue,
            "nudge" => {
                if !m.content.is_empty() {
                    b.push_str("[User]: ");
                    b.push_str(&m.content);
                    b.push('\n');
                }
            }
            "user" => {
                let text = serialize_text(&m.content, &m.images);
                if !text.is_empty() {
                    b.push_str("[User]: ");
                    b.push_str(&text);
                    b.push('\n');
                }
            }
            "assistant" => {
                if !m.reasoning.is_empty() {
                    b.push_str("[Assistant thinking]: ");
                    b.push_str(&m.reasoning);
                    b.push('\n');
                }
                if !m.content.is_empty() {
                    b.push_str("[Assistant]: ");
                    b.push_str(&m.content);
                    b.push('\n');
                }
                if !m.tool_calls.is_empty() {
                    let parts: Vec<String> = m
                        .tool_calls
                        .iter()
                        .map(|tc| format!("{}({})", tc.function.name, tc.function.arguments))
                        .collect();
                    b.push_str("[Assistant tool calls]: ");
                    b.push_str(&parts.join("; "));
                    b.push('\n');
                }
            }
            "tool" => {
                let text = serialize_text(&truncate_tool_result(&m.content), &m.images);
                if !text.is_empty() {
                    b.push_str("[Tool result]: ");
                    b.push_str(&text);
                    b.push('\n');
                }
            }
            _ => {}
        }
    }
    b.trim().to_string()
}

/// serializeText joins message text with image placeholders. Empty
/// sections are omitted so the summary prompt stays compact.
fn serialize_text(content: &str, images: &[ImageData]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !content.is_empty() {
        parts.push(content.to_string());
    }
    for _ in images {
        parts.push("[image attached]".to_string());
    }
    parts.join("\n")
}

pub fn truncate_tool_result(s: &str) -> String {
    if s.len() <= COMPACTION_TOOL_RESULT_LIMIT {
        return s.to_string();
    }
    let omitted = s.len() - COMPACTION_TOOL_RESULT_LIMIT;
    let head = String::from_utf8_lossy(&s.as_bytes()[..COMPACTION_TOOL_RESULT_LIMIT]).into_owned();
    format!("{head}\n...[{omitted} characters omitted]")
}

/// compactSession summarizes Messages[start:end] and stores the brief on
/// sess. The last user message, if any, stays in the live tail. On
/// failure the session is left unchanged so the turn can continue.
pub async fn compact_session(
    sess: &mut Session,
    client: Option<&reqwest::Client>,
    base_url: &str,
    key: &str,
    model: &str,
    extra: &str,
) -> anyhow::Result<()> {
    let model = if model.trim().is_empty() {
        COMPACTION_MODEL_ID
    } else {
        model.trim()
    };
    let (start, end) = match compact_span(sess) {
        Some(span) => span,
        None => return Err(NothingToCompact.into()),
    };
    let mut body = serialize_conversation(&sess.messages[start..end], &sess.compaction_summary);
    let extra = extra.trim();
    if !extra.is_empty() {
        body += &format!("\n\nAdditional instructions from the user:\n{extra}");
    }
    if body.is_empty() {
        return Err(NothingToCompact.into());
    }
    let client = match client {
        Some(c) => c.clone(),
        None => reqwest::Client::builder()
            .timeout(Duration::from_secs(10 * 60))
            .build()?,
    };
    let base_url = base_url.trim_end_matches('/');

    let req_body = serde_json::to_vec(&ChatRequest {
        model: model.into(),
        messages: vec![
            Message {
                role: "system".into(),
                content: COMPACTION_SYSTEM_PROMPT.into(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: body,
                ..Default::default()
            },
        ],
        stream: false,
        tools: vec![],
        reasoning_effort: thinking_off_value(&provider_name_for_url(base_url), model),
        stream_options: None,
    })?;
    let started_at = Instant::now();
    let raw = post_chat_completion(&client, base_url, key, &req_body).await?;
    let duration_ms = started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;

    let parsed: ChatCompletionResponse =
        serde_json::from_slice(&raw).map_err(|e| anyhow!("compaction response parse: {e}"))?;
    let choice = match parsed.choices.into_iter().next() {
        Some(c) => c.message,
        None => return Err(anyhow!("compaction response had no choices")),
    };
    let summary = compact_choice_text(
        &choice.content,
        &choice.reasoning,
        &choice.reasoning_content,
    );
    if summary.is_empty() {
        return Err(anyhow!("compaction response was empty"));
    }

    let prompt = compaction_prompt_text(&summary);
    let entry = Message {
        role: "compaction".into(),
        content: prompt,
        provider: provider_name_for_url(base_url),
        model: model.into(),
        duration_ms,
        usage: parsed.usage,
        ..Default::default()
    };
    // Append the brief at the end so the TUI shows it as the latest
    // model output. CompactedThrough still points at the live tail;
    // llmMessages skips role=compaction so this display copy is not
    // sent twice.
    sess.messages.push(entry);
    sess.compaction_summary = summary;
    sess.compacted_through = end as i64;
    sess.usage = Some(estimate_session_usage(sess));
    Ok(())
}

/// compactionPromptText is the user-side payload llmMessages sends after
/// a fold: the brief, without system instructions or the assistant ack.
pub fn compaction_prompt_text(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREAMBLE}{summary}")
}

/// One chat-completions response body (non-streaming).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatCompletionResponse {
    #[serde(default)]
    pub choices: Vec<ChatCompletionChoice>,
    #[serde(default)]
    pub usage: Option<StreamUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatCompletionChoice {
    #[serde(default)]
    pub message: ChatCompletionMessage,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatCompletionMessage {
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default, rename = "reasoning_content")]
    pub reasoning_content: String,
}

/// compactChoiceText pulls the summary out of a chat-completions choice.
/// Flash models often put the brief in reasoning when content is empty,
/// and some routers send content as a part array instead of a string.
pub fn compact_choice_text(
    content: &serde_json::Value,
    reasoning: &str,
    reasoning_content: &str,
) -> String {
    let s = content_text(content).trim().to_string();
    if !s.is_empty() {
        return s;
    }
    let s = reasoning.trim().to_string();
    if !s.is_empty() {
        return s;
    }
    reasoning_content.trim().to_string()
}

pub fn content_text(raw: &serde_json::Value) -> String {
    match raw {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let mut b = String::new();
            for p in parts {
                let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "text" || ty.is_empty() {
                    b.push_str(p.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                }
            }
            b
        }
        _ => String::new(),
    }
}

/// estimateSessionUsage approximates the tokens the next chat request
/// will send: instructions, the compaction brief, and the live tail.
/// Providers don't report usage for a compact itself in a form that
/// matches the rebuilt context, so the status-bar meter uses this until
/// the next real chat round.
pub fn estimate_session_usage(sess: &Session) -> StreamUsage {
    let mut n: usize = 0;
    for m in llm_messages(sess) {
        n += crate::session::context_breakdown::estimate_message_chars(&m) as usize;
    }
    let tok = crate::session::context_breakdown::estimate_tokens(n).max(1);
    StreamUsage {
        prompt_tokens: tok,
        total_tokens: tok,
        ..Default::default()
    }
}

/// thinkingOffValue is the wire token that disables reasoning for the
/// model (models.dev "none"), or the first catalog level; empty means
/// omit.
pub fn thinking_off_value(provider: &str, model: &str) -> String {
    crate::providers::modelsdev::thinking_off_value(provider, model)
}

/// providerNameForURL returns a human-readable provider name for a base
/// URL (models.go). The models.dev catalog scan is omitted here — those
/// providers are matched by the server before calling compaction.
pub fn provider_name_for_url(url: &str) -> String {
    if url.contains("opencode.ai/zen/go") {
        "opencode-go".into()
    } else if url.contains("opencode.ai/zen") {
        "opencode-zen".into()
    } else if url.contains("opencode.ai") {
        "opencode-go".into()
    } else if url.contains("ollama.com") {
        "ollama".into()
    } else if url.contains("localhost") || url.contains("127.0.0.1") {
        "ollama-local".into()
    } else {
        "custom".into()
    }
}

/// POST {base_url}/chat/completions with retry, mirroring retry.go's
/// doHTTPWithRetry. Returns the response body capped at 1MB.
pub(crate) async fn post_chat_completion(
    client: &reqwest::Client,
    base_url: &str,
    key: &str,
    body: &[u8],
) -> anyhow::Result<Vec<u8>> {
    const RETRY_DELAYS_MS: [u64; 10] =
        [670, 1200, 1400, 2000, 2400, 2600, 3000, 5000, 10000, 15000];
    const MAX_BODY: usize = 1 << 20;

    let mut attempt = 0usize;
    loop {
        let mut resp = client
            .post(format!("{base_url}/chat/completions"))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .body(body.to_vec())
            .send()
            .await
            .map_err(anyhow::Error::from)?;
        let status = resp.status();
        if status.is_success() {
            let mut raw: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                let room = MAX_BODY.saturating_sub(raw.len());
                raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if raw.len() >= MAX_BODY {
                    break;
                }
            }
            return Ok(raw);
        }
        let snippet: Vec<u8> = resp
            .bytes()
            .await
            .map(|b| b[..b.len().min(4096)].to_vec())
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&snippet).trim().to_string();
        if is_retryable_provider_error(status.as_u16(), &text) && attempt < RETRY_DELAYS_MS.len() {
            tokio::time::sleep(Duration::from_millis(RETRY_DELAYS_MS[attempt])).await;
            attempt += 1;
            continue;
        }
        let reason = status.canonical_reason().unwrap_or("");
        return Err(anyhow!(format!("{status} {reason}: {text}")
            .trim_end()
            .to_string()));
    }
}

fn is_retryable_provider_error(status: u16, body: &str) -> bool {
    match status {
        400 | 401 | 403 | 404 => return false,
        429 | 502 | 503 | 504 | 529 => return true,
        _ => {}
    }
    let lower = body.to_lowercase();
    if lower.contains("service unavailable") {
        return true;
    }
    if lower.contains("rate limit") || lower.contains("rate-limited") || lower.contains("ratelimit")
    {
        return true;
    }
    if lower.contains("upstream") {
        for w in [
            "rate",
            "unavailable",
            "overloaded",
            "capacity",
            "temporarily",
        ] {
            if lower.contains(w) {
                return true;
            }
        }
    }
    false
}

/// sanitizeMessages returns protocol-safe messages for the model API.
/// Every retained tool call has a matching result, malformed calls and
/// orphaned results are removed, and local error records are not sent as
/// unsupported provider message roles. The input slice is not modified.
pub fn sanitize_messages(msgs: &[Message]) -> Vec<Message> {
    let answered_ids: std::collections::HashSet<&str> = msgs
        .iter()
        .filter(|m| m.role == "tool" && !m.tool_call_id.is_empty())
        .map(|m| m.tool_call_id.as_str())
        .collect();
    let mut out = Vec::with_capacity(msgs.len());
    let mut valid_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in msgs {
        if m.role == "error" {
            continue;
        }
        if m.role == "tool" {
            if valid_ids.contains(&m.tool_call_id) {
                out.push(m.clone());
            }
            continue; // orphaned tool result otherwise dropped
        }
        if m.role == "assistant" && !m.tool_calls.is_empty() {
            let mut calls = Vec::new();
            for tc in &m.tool_calls {
                if tc.id.is_empty()
                    || tc.function.name.is_empty()
                    || !json_valid(&tc.function.arguments)
                    || !answered_ids.contains(tc.id.as_str())
                {
                    continue;
                }
                valid_ids.insert(tc.id.clone());
                calls.push(tc.clone());
            }
            if calls.is_empty() {
                continue; // drop the empty tool-call turn entirely
            }
            let mut m2 = m.clone();
            m2.tool_calls = calls;
            out.push(m2);
            continue;
        }
        out.push(m.clone());
    }
    out
}

fn json_valid(s: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    /// Minimal canned chat-completions server for tests: records every
    /// request body and replies with `status` + `body`.
    async fn spawn_http_server(
        status: u16,
        body: serde_json::Value,
        capture: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let capture = capture.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let head_end = loop {
                        match stream.read(&mut tmp).await {
                            Ok(0) => break buf.len(),
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    break i + 4;
                                }
                            }
                            Err(_) => break buf.len(),
                        }
                    };
                    let content_length = String::from_utf8_lossy(&buf)
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + content_length {
                        match stream.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    if let Ok(req_body) =
                        serde_json::from_slice::<serde_json::Value>(&buf[head_end.min(buf.len())..])
                    {
                        capture.lock().unwrap().push(req_body);
                    }
                    let out = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        if status == 200 { "OK" } else { "Internal Server Error" },
                        body.to_string().len(),
                        body
                    );
                    let _ = stream.write_all(out.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn should_compact_thresholds() {
        assert!(!should_compact(&Session::default()), "nil usage");
        let below = Session {
            usage: Some(StreamUsage {
                prompt_tokens: 1000,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!should_compact(&below), "below threshold");
        let equal = Session {
            usage: Some(StreamUsage {
                prompt_tokens: 150_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!should_compact(&equal), "equal threshold");
        let above = Session {
            usage: Some(StreamUsage {
                prompt_tokens: 150_001,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(should_compact(&above), "above threshold");
    }

    #[test]
    fn compaction_threshold_uses_window_minus_headroom() {
        use crate::session::compaction::COMPACTION_HEADROOM_TOKENS;

        assert_eq!(
            compaction_threshold(200_000),
            COMPACTION_TOKEN_THRESHOLD,
            "large window keeps the fixed threshold"
        );
        assert_eq!(
            compaction_threshold(150_000),
            130_000,
            "window below fixed threshold+headroom compacts at window-20k"
        );
        assert_eq!(
            compaction_threshold(128_000),
            108_000,
            "128k window compacts at 108k"
        );
        assert_eq!(
            compaction_threshold(COMPACTION_HEADROOM_TOKENS),
            COMPACTION_TOKEN_THRESHOLD,
            "tiny window falls back to the fixed threshold"
        );
        assert_eq!(
            compaction_threshold(0),
            COMPACTION_TOKEN_THRESHOLD,
            "unknown window falls back to the fixed threshold"
        );
    }

    #[test]
    fn should_compact_with_threshold_matches_usage() {
        let sess = Session {
            usage: Some(StreamUsage {
                prompt_tokens: 108_001,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(should_compact_with_threshold(
            &sess,
            compaction_threshold(128_000)
        ));
        assert!(!should_compact_with_threshold(
            &sess,
            compaction_threshold(400_000)
        ));
    }

    #[test]
    fn default_compaction_target_is_explicit() {
        let target = crate::config::AtomConfig::default().resolved_compaction();
        assert_eq!(target.provider, "ollama-local");
        assert_eq!(target.model, COMPACTION_MODEL_ID);
    }

    #[test]
    fn compact_span_leaves_trailing_nudge() {
        let sess = Session {
            messages: vec![
                msg("user", "do it"),
                Message { role: "assistant".into(), reasoning: "plan".into(), ..Default::default() },
                msg(
                    "nudge",
                    "Continue. Your previous reply was cut off during reasoning. If work remains, call a tool. If the task is done, reply with the answer.",
                ),
            ],
            ..Default::default()
        };
        assert_eq!(compact_span(&sess), Some((0, 2)));
    }

    #[test]
    fn serialize_conversation_sections() {
        let long_tool = "x".repeat(COMPACTION_TOOL_RESULT_LIMIT + 25);
        let bash = ToolCall {
            id: String::new(),
            call_type: String::new(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        };
        let got = serialize_conversation(
            &[
                Message {
                    role: "user".into(),
                    content: "look at this".into(),
                    images: vec![ImageData {
                        mime: "image/png".into(),
                        data: "abc".into(),
                    }],
                    ..Default::default()
                },
                Message {
                    role: "assistant".into(),
                    reasoning: "plan".into(),
                    content: "calling ls".into(),
                    tool_calls: vec![bash],
                    ..Default::default()
                },
                msg("tool", &long_tool),
                msg("assistant", ""), // empty sections skipped
            ],
            "older brief",
        );

        assert!(got.contains("Previous summary:\nolder brief"), "{got}");
        assert!(
            got.contains("[User]: look at this\n[image attached]"),
            "{got}"
        );
        assert!(got.contains("[Assistant thinking]: plan"), "{got}");
        assert!(got.contains("[Assistant]: calling ls"), "{got}");
        assert!(
            got.contains(r#"[Assistant tool calls]: bash({"command":"ls"})"#),
            "{got}"
        );
        assert!(
            got.contains(&format!(
                "[Tool result]: {}",
                "x".repeat(COMPACTION_TOOL_RESULT_LIMIT)
            )),
            "{got}"
        );
        assert!(got.contains("...[25 characters omitted]"), "{got}");
        assert!(
            !got.contains("[Assistant]: \n"),
            "empty assistant section should be skipped"
        );
    }

    #[test]
    fn llm_messages_shapes() {
        let instr = vec![msg("system", "be helpful")];
        let tail = vec![
            msg("user", "one"),
            msg("assistant", "two"),
            msg("user", "three"),
        ];

        // no summary
        let got = llm_messages(&Session {
            instructions: instr.clone(),
            messages: tail.clone(),
            ..Default::default()
        });
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].content, "be helpful");
        assert_eq!(got[1].content, "one");

        // summary and CompactedThrough
        let got = llm_messages(&Session {
            instructions: instr.clone(),
            messages: tail.clone(),
            compaction_summary: "brief".into(),
            compacted_through: 2,
            ..Default::default()
        });
        assert_eq!(got.len(), 4);
        assert_eq!(got[1].role, "user");
        assert!(got[1].content.contains("brief"));
        assert_eq!(got[2].role, "assistant");
        assert_eq!(got[2].content, COMPACTION_SUMMARY_ACK);
        assert_eq!(got[3].content, "three");

        // skips display compaction message
        let mut msgs = tail.clone();
        msgs.push(Message {
            role: "compaction".into(),
            content: compaction_prompt_text("brief"),
            ..Default::default()
        });
        let got = llm_messages(&Session {
            instructions: instr.clone(),
            messages: msgs,
            compaction_summary: "brief".into(),
            compacted_through: 2,
            ..Default::default()
        });
        assert!(
            !got.iter().any(|m| m.role == "compaction"),
            "display compaction leaked"
        );
        assert_eq!(got.last().unwrap().content, "three");

        // clamp CompactedThrough
        let got = llm_messages(&Session {
            instructions: instr.clone(),
            messages: tail.clone(),
            compaction_summary: "brief".into(),
            compacted_through: 99,
            ..Default::default()
        });
        assert_eq!(got.len(), 3, "instructions+summary+ack");

        // last user kept after compact
        let sess = Session {
            instructions: instr.clone(),
            messages: tail.clone(),
            compaction_summary: "folded one and two".into(),
            compacted_through: 2,
            ..Default::default()
        };
        let got = llm_messages(&sess);
        assert_eq!(got.last().unwrap().role, "user");
        assert_eq!(got.last().unwrap().content, "three");
        assert_eq!(
            sess.messages.len(),
            3,
            "full transcript must stay on the session"
        );

        // nudge becomes user for the model
        let nudge_text = "Continue. Your previous reply was cut off during reasoning. If work remains, call a tool. If the task is done, reply with the answer.";
        let got = llm_messages(&Session {
            messages: vec![
                msg("user", "do it"),
                Message {
                    role: "assistant".into(),
                    reasoning: "plan".into(),
                    ..Default::default()
                },
                msg("nudge", nudge_text),
            ],
            ..Default::default()
        });
        let last = got.last().unwrap();
        assert_eq!(last.role, "user");
        assert_eq!(last.content, nudge_text);
        assert!(!got.iter().any(|m| m.role == "nudge"), "nudge role leaked");
    }

    #[tokio::test]
    async fn compact_session_success() {
        let capture: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_http_server(
            200,
            serde_json::json!({
                "choices": [{"message": {"content": "## Goal\nShip compaction"}}]
            }),
            capture.clone(),
        )
        .await;
        let mut sess = Session {
            messages: vec![
                msg("user", "hello"),
                msg("assistant", "hi there"),
                msg("user", "what next?"),
            ],
            usage: Some(StreamUsage {
                prompt_tokens: 150_001,
                ..Default::default()
            }),
            ..Default::default()
        };
        compact_session(
            &mut sess,
            Some(&reqwest::Client::new()),
            &url,
            "test-key",
            COMPACTION_MODEL_ID,
            "",
        )
        .await
        .unwrap();

        let got = capture.lock().unwrap()[0].clone();
        assert_eq!(got["model"], COMPACTION_MODEL_ID);
        assert_eq!(got["stream"], serde_json::json!(false), "must not stream");
        let req_msgs = got["messages"].as_array().unwrap();
        assert!(req_msgs.len() >= 2);
        assert!(
            req_msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("[User]: hello"),
            "serialized history missing"
        );
        assert!(
            req_msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("[Assistant]: hi there"),
            "assistant turn missing"
        );
        assert_eq!(sess.compaction_summary, "## Goal\nShip compaction");
        assert_eq!(sess.compacted_through, 2, "last user kept");
        assert_eq!(sess.messages.len(), 4, "summary appended");
        let last = sess.messages.last().unwrap();
        assert_eq!(last.role, "compaction");
        assert_eq!(
            last.content,
            compaction_prompt_text("## Goal\nShip compaction")
        );
        let usage = sess.usage.as_ref().unwrap();
        assert!(
            usage.total_tokens > 0,
            "usage should reflect compacted context"
        );

        let live = llm_messages(&sess);
        assert_eq!(live.last().unwrap().content, "what next?");
        assert!(
            !live.iter().any(|m| m.content == "hi there"),
            "folded turn must not be resent"
        );
    }

    #[tokio::test]
    async fn compact_session_failure_leaves_session_unchanged() {
        let capture: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_http_server(500, serde_json::json!("boom"), capture).await;

        let usage = StreamUsage {
            prompt_tokens: 150_001,
            ..Default::default()
        };
        let mut sess = Session {
            messages: vec![
                msg("user", "hello"),
                msg("assistant", "hi"),
                msg("user", "again"),
            ],
            usage: Some(usage),
            compaction_summary: "old brief".into(),
            compacted_through: 1,
            ..Default::default()
        };
        assert!(
            compact_session(
                &mut sess,
                Some(&reqwest::Client::new()),
                &url,
                "",
                COMPACTION_MODEL_ID,
                "",
            )
            .await
            .is_err(),
            "expected error from 500"
        );
        assert_eq!(sess.compaction_summary, "old brief");
        assert_eq!(sess.compacted_through, 1);
        assert_eq!(sess.messages.len(), 3);
        assert_eq!(sess.usage.as_ref().unwrap().prompt_tokens, 150_001);
    }

    #[tokio::test]
    async fn compact_session_extra_instructions() {
        let capture: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_http_server(
            200,
            serde_json::json!({"choices": [{"message": {"content": "focused brief"}}]}),
            capture.clone(),
        )
        .await;
        let mut sess = Session {
            messages: vec![msg("user", "hello"), msg("assistant", "hi")],
            ..Default::default()
        };
        compact_session(
            &mut sess,
            Some(&reqwest::Client::new()),
            &url,
            "",
            COMPACTION_MODEL_ID,
            "keep the file paths",
        )
        .await
        .unwrap();
        let got = capture.lock().unwrap()[0].clone();
        let content = got["messages"][1]["content"].as_str().unwrap();
        assert!(
            content.contains("Additional instructions from the user:\nkeep the file paths"),
            "{content}"
        );
        assert_eq!(sess.compaction_summary, "focused brief");
    }

    #[tokio::test]
    async fn compact_session_nothing_to_compact() {
        let mut sess = Session {
            messages: vec![msg("user", "only")],
            ..Default::default()
        };
        let err = compact_session(
            &mut sess,
            None,
            "http://unused",
            "",
            COMPACTION_MODEL_ID,
            "",
        )
        .await
        .unwrap_err();
        assert!(is_nothing_to_compact(&err));
    }

    #[test]
    fn compact_choice_text_fallbacks() {
        assert_eq!(
            compact_choice_text(&serde_json::json!("brief"), "think", ""),
            "brief"
        );
        assert_eq!(
            compact_choice_text(
                &serde_json::json!([{"type":"text","text":"from parts"}]),
                "",
                ""
            ),
            "from parts"
        );
        assert_eq!(
            compact_choice_text(&serde_json::Value::Null, "  in reasoning  ", ""),
            "in reasoning"
        );
        assert_eq!(
            compact_choice_text(&serde_json::Value::Null, "", "alt"),
            "alt"
        );
    }

    #[test]
    fn serialize_skips_compaction_role() {
        let got = serialize_conversation(
            &[
                msg("user", "hi"),
                Message {
                    role: "compaction".into(),
                    content: compaction_prompt_text("old"),
                    ..Default::default()
                },
                msg("assistant", "ok"),
            ],
            "",
        );
        assert!(
            !got.contains("Previous conversation summary") && !got.contains("old"),
            "compaction payload leaked into summarizer input:\n{got}"
        );
        assert!(got.contains("[User]: hi"));
        assert!(got.contains("[Assistant]: ok"));
    }

    #[test]
    fn sanitize_messages_drops_bad_tool_calls() {
        let good = ToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        };
        let bad = ToolCall {
            id: "t2".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: "{oops".into(),
            },
        };
        let msgs = vec![
            msg("user", "go"),
            Message {
                role: "assistant".into(),
                tool_calls: vec![good, bad],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "t1".into(),
                content: "ok".into(),
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "t2".into(),
                content: "orphan".into(),
                ..Default::default()
            },
        ];
        let out = sanitize_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].tool_calls.len(), 1);
        assert_eq!(out[1].tool_calls[0].id, "t1");
    }

    #[test]
    fn sanitize_messages_drops_unanswered_calls_and_errors() {
        let call = |id: &str, name: &str, arguments: &str| ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        };
        let msgs = vec![
            msg("user", "go"),
            Message {
                role: "assistant".into(),
                tool_calls: vec![
                    call("answered", "grep", r#"{"pattern":"needle"}"#),
                    call("unanswered", "glob", r#"{"pattern":"**/*.rs"}"#),
                ],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                tool_call_id: "answered".into(),
                content: "hit".into(),
                ..Default::default()
            },
            msg("error", "provider-local failure"),
            msg("user", "continue"),
        ];

        let out = sanitize_messages(&msgs);
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[1].tool_calls.len(), 1);
        assert_eq!(out[1].tool_calls[0].id, "answered");
        assert!(out.iter().all(|m| m.role != "error"));
    }
}
