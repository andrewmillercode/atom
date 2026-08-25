//! LLM-generated session titles, ported from title.go. The server
//! schedules generate_and_store_title in the background after each turn;
//! failures leave the fallback title in place and never block a turn.

use crate::session::compaction::{compact_choice_text, post_chat_completion};
use crate::session::store::{Session, SessionStore};
use crate::types::{ChatRequest, Message};
use anyhow::anyhow;
use std::time::Duration;

pub const TITLE_MODEL_ID: &str = crate::config::DEFAULT_COMPACTION_MODEL;

pub const TITLE_SYSTEM_PROMPT: &str =
    "You name coding-agent sessions. Reply with only a short descriptive title of at most 7 words. No quotes, no trailing punctuation, no markdown. Capture the user's task.";

/// titleProvider picks the configured compaction endpoint and model.
pub async fn title_provider() -> crate::session::compaction::CompactionTarget {
    crate::session::compaction::compaction_target().await
}

/// generateAndStoreTitle loads the session, asks the title model, and
/// stores it on success. Failures leave the fallback title in place.
/// Returns the generated title, or None when skipped/failed. (Go also
/// broadcasts a "title" event; the server does that around this call.)
pub async fn generate_and_store_title(
    store: &SessionStore,
    id: &str,
    base_url: &str,
    key: &str,
    model: &str,
) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let sess = store.get(id)?;
    if sess.title_generated {
        return None;
    }
    let has_user = sess
        .messages
        .iter()
        .any(|m| m.role == "user" && !m.content.trim().is_empty());
    if !has_user {
        return None;
    }
    let title = match generate_title(&sess, None, base_url, key, model).await {
        Ok(t) => t,
        Err(_) => return None,
    };
    if title.is_empty() {
        return None;
    }
    if let Some(got) = store.get(id) {
        if got.title_generated {
            return None;
        }
    }
    store.update_title(id, &title);
    Some(title)
}

/// generateSessionTitle asks the title model for a short name. The
/// transcript is not modified.
pub async fn generate_title(
    sess: &Session,
    client: Option<&reqwest::Client>,
    base_url: &str,
    key: &str,
    model: &str,
) -> anyhow::Result<String> {
    let model = if model.trim().is_empty() {
        TITLE_MODEL_ID
    } else {
        model.trim()
    };
    let body = title_user_payload(Some(sess));
    if body.is_empty() {
        return Err(anyhow!("no user message to title"));
    }
    let client = match client {
        Some(c) => c.clone(),
        None => reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
    };
    let base_url = base_url.trim_end_matches('/');

    let req_body = serde_json::to_vec(&ChatRequest {
        model: model.into(),
        messages: vec![
            Message {
                role: "system".into(),
                content: TITLE_SYSTEM_PROMPT.into(),
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
        reasoning_effort: crate::session::compaction::thinking_off_value(
            &crate::session::compaction::provider_name_for_url(base_url),
            model,
        ),
        stream_options: None,
    })?;
    let raw = post_chat_completion(&client, base_url, key, &req_body).await?;

    #[derive(serde::Deserialize)]
    struct Parsed {
        #[serde(default)]
        choices: Vec<crate::session::compaction::ChatCompletionChoice>,
    }
    let parsed: Parsed =
        serde_json::from_slice(&raw).map_err(|e| anyhow!("title response parse: {e}"))?;
    let msg = match parsed.choices.into_iter().next() {
        Some(c) => c.message,
        None => return Err(anyhow!("title response had no choices")),
    };
    let title = sanitize_title(&compact_choice_text(
        &msg.content,
        &msg.reasoning,
        &msg.reasoning_content,
    ));
    if title.is_empty() {
        return Err(anyhow!("title response was empty"));
    }
    Ok(title)
}

/// titleUserPayload is a compact slice of the first user turn and a
/// little later prose. Tool dumps are skipped.
pub fn title_user_payload(sess: Option<&Session>) -> String {
    let Some(sess) = sess else {
        return String::new();
    };
    let mut first = String::new();
    let mut extra: Vec<String> = Vec::new();
    for m in &sess.messages {
        if m.role == "tool" || m.role == "compaction" {
            continue;
        }
        let text = m.content.trim();
        if text.is_empty() {
            continue;
        }
        if m.role == "user" && first.is_empty() {
            first = clip_title_text(text, 1500);
            continue;
        }
        if m.role == "user" || m.role == "assistant" {
            extra.push(clip_title_text(text, 400));
            if extra.len() >= 3 {
                break;
            }
        }
    }
    if first.is_empty() {
        return String::new();
    }
    if extra.is_empty() {
        return first;
    }
    format!("{first}\n\n{}", extra.join("\n"))
}

pub fn clip_title_text(s: &str, n: usize) -> String {
    let r: Vec<char> = s.chars().collect();
    if n > 0 && r.len() > n {
        return r[..n].iter().collect();
    }
    s.to_string()
}

/// sanitizeTitle trims quotes, markdown, extra words, and trailing
/// punctuation so the result is safe as a terminal tab title.
pub fn sanitize_title(s: &str) -> String {
    let mut s = s.trim().to_string();
    if s.is_empty() {
        return String::new();
    }
    s = strip_wrapping_marks(&s).trim().to_string();
    if let Some(i) = s.find('\n') {
        s.truncate(i);
    }
    s = strip_wrapping_marks(s.trim()).trim().to_string();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut words: Vec<&str> = s.split_whitespace().collect();
    if words.len() > 7 {
        words.truncate(7);
    }
    s = words.join(" ");
    s = s
        .trim_end_matches(|r: char| ".!?:;".contains(r) || r.is_whitespace())
        .to_string();
    s.trim().to_string()
}

fn strip_wrapping_marks(s: &str) -> String {
    let mut s = s.to_string();
    loop {
        s = s.trim().to_string();
        let bytes = s.as_bytes();
        if bytes.len() < 2 {
            return s;
        }
        let (a, b) = (bytes[0], bytes[bytes.len() - 1]);
        if (a == b'"' && b == b'"') || (a == b'\'' && b == b'\'') || (a == b'`' && b == b'`') {
            s = s[1..s.len() - 1].to_string();
            continue;
        }
        return s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::compaction::COMPACTION_MODEL_ID;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn sanitize_title_cases() {
        let cases = [
            (r#""Fix the login bug""#, "Fix the login bug"),
            ("'Quoted title'", "Quoted title"),
            ("`backticked`", "backticked"),
            (
                "one two three four five six seven eight nine",
                "one two three four five six seven",
            ),
            ("first line\nsecond line should drop", "first line"),
            ("   ", ""),
            ("", ""),
            ("Ship the feature.", "Ship the feature"),
            ("Ready?!", "Ready"),
            ("  lots   of\tspace  ", "lots of space"),
        ];
        for (input, want) in cases {
            assert_eq!(sanitize_title(input), want, "sanitizeTitle({input:?})");
        }
    }

    #[test]
    fn title_payload_shape() {
        let sess = Session {
            messages: vec![
                msg("user", "please fix the auth middleware"),
                msg("tool", "tool dump skipped"),
                msg("assistant", "looking into it"),
                msg("user", "thanks"),
            ],
            ..Default::default()
        };
        let payload = title_user_payload(Some(&sess));
        assert!(payload.contains("please fix the auth middleware"));
        assert!(payload.contains("looking into it"));
        assert!(!payload.contains("tool dump skipped"));
        assert_eq!(title_user_payload(None), "");
        assert_eq!(
            title_user_payload(Some(&Session {
                messages: vec![msg("tool", "x")],
                ..Default::default()
            })),
            ""
        );
    }

    /// Minimal canned chat-completions server (same shape as the
    /// compaction tests); records captured request bodies.
    async fn spawn_http_server(
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
                let body = body.clone();
                let capture = capture.clone();
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
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.to_string().len(),
                        body
                    );
                    let _ = stream.write_all(out.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn generate_session_title_success() {
        let capture: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = spawn_http_server(
            serde_json::json!({
                "choices": [{"message": {"content": "\"Fix auth middleware.\""}}]
            }),
            capture.clone(),
        )
        .await;
        let sess = Session {
            messages: vec![msg("user", "please fix the auth middleware")],
            ..Default::default()
        };
        let before = sess.messages.len();
        let title = generate_title(
            &sess,
            Some(&reqwest::Client::new()),
            &url,
            "test-key",
            TITLE_MODEL_ID,
        )
        .await
        .unwrap();
        assert_eq!(title, "Fix auth middleware");
        assert_eq!(sess.messages.len(), before, "transcript mutated");

        let reqs = capture.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let got = &reqs[0];
        assert_eq!(got["model"], TITLE_MODEL_ID);
        assert_eq!(got["stream"], serde_json::json!(false), "must not stream");
        let req_msgs = got["messages"].as_array().unwrap();
        assert!(req_msgs.len() >= 2);
        assert_eq!(req_msgs[0]["role"], "system");
        assert_eq!(req_msgs[0]["content"], TITLE_SYSTEM_PROMPT);
        assert!(
            req_msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("please fix the auth middleware"),
            "user payload missing"
        );
    }

    #[tokio::test]
    async fn generate_session_title_reasoning_fallback() {
        let url = spawn_http_server(
            serde_json::json!({
                "choices": [{"message": {"content": "", "reasoning": "Rename session helper"}}]
            }),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        )
        .await;
        let sess = Session {
            messages: vec![msg("user", "rename the helper")],
            ..Default::default()
        };
        let title = generate_title(
            &sess,
            Some(&reqwest::Client::new()),
            &url,
            "",
            TITLE_MODEL_ID,
        )
        .await
        .unwrap();
        assert_eq!(title, "Rename session helper");
    }

    #[tokio::test]
    async fn generate_and_store_titles_once() {
        let dir = std::env::temp_dir().join(format!(
            "atom-title-test-{}-{}",
            std::process::id(),
            crate::session::store::new_session_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_in_dir(&dir).unwrap();
        let sess = store.create("m", "/tmp", vec![]);
        store.update(&sess.id, vec![msg("user", "add a retry loop")], "");

        let url = spawn_http_server(
            serde_json::json!({
                "choices": [{"message": {"content": "Add retry loop"}}]
            }),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        )
        .await;

        let got = generate_and_store_title(&store, &sess.id, &url, "k", TITLE_MODEL_ID).await;
        assert_eq!(got.as_deref(), Some("Add retry loop"));
        let after = store.get(&sess.id).unwrap();
        assert_eq!(after.title, "Add retry loop");
        assert!(after.title_generated);
        assert_eq!(after.messages.len(), 1);
        assert_eq!(after.messages[0].content, "add a retry loop");

        // Second call must not regenerate.
        assert!(
            generate_and_store_title(&store, &sess.id, &url, "k", TITLE_MODEL_ID)
                .await
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn constants_match_go() {
        assert_eq!(TITLE_MODEL_ID, COMPACTION_MODEL_ID);
        assert!(TITLE_SYSTEM_PROMPT.starts_with("You name coding-agent sessions."));
    }
}
