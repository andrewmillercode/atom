//! HTTP retry helper for provider calls, ported from retry.go.
//! Async reqwest-based: requests are rebuilt per attempt by a closure,
//! matching Go's newReq pattern.

use crate::types::StreamResult;
use std::fmt;
use std::time::Duration;

pub const PROVIDER_RETRY_DELAYS_MS: &[u64] =
    &[670, 1200, 1400, 2000, 2400, 2600, 3000, 5000, 10000, 15000];

pub fn provider_retry_delays() -> &'static [Duration] {
    static DELAYS: once_cell::sync::Lazy<Vec<Duration>> = once_cell::sync::Lazy::new(|| {
        PROVIDER_RETRY_DELAYS_MS
            .iter()
            .map(|ms| Duration::from_millis(*ms))
            .collect()
    });
    &DELAYS
}

/// Empty successful streams are generally a transient provider/gateway
/// failure. Retrying the same round is safe because no assistant output or
/// tool call was emitted.
pub const MAX_EMPTY_RESPONSE_RETRIES: usize = 3;

/// maxReasoningNudges is how many times a turn will continue after a
/// stream that died during thinking (no content, no tool calls). Same
/// delay table as HTTP retries, but capped so a model that only thinks
/// cannot burn ten full rounds.
pub const MAX_REASONING_NUDGES: usize = 3;

pub const REASONING_NUDGE_TEXT: &str = "Continue. Your previous reply was cut off during reasoning. If work remains, call a tool. If the task is done, reply with the answer.";

/// incompleteReasoningStream reports a truncated thinking-only reply:
/// the model streamed reasoning, then the SSE ended before content or
/// tools. Missing usage is the usual signal; finish_reason=length is
/// the explicit one. A clean stop with usage is a real (if lazy) end
/// of turn and is not nudged.
pub fn incomplete_reasoning_stream(r: &StreamResult) -> bool {
    if !r.content.is_empty() || !r.tool_calls.is_empty() {
        return false;
    }
    if r.reasoning.trim().is_empty() {
        return false;
    }
    match &r.usage {
        None => true,
        Some(_) => r.finish_reason == "length",
    }
}

pub fn should_nudge_incomplete_reasoning(r: &StreamResult, attempt: usize) -> bool {
    attempt < MAX_REASONING_NUDGES && incomplete_reasoning_stream(r)
}

/// providerHTTPError carries the non-retryable failure back to callers
/// (Display is "Status: Body", like Go's Error()).
#[derive(Debug, Clone)]
pub struct ProviderHTTPError {
    pub status: String,
    pub status_code: u16,
    pub body: String,
}

impl fmt::Display for ProviderHTTPError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status, self.body)
    }
}

impl std::error::Error for ProviderHTTPError {}

pub fn is_retryable_provider_error(status: u16, body: &str) -> bool {
    // Authentication, authorization, and routing failures are permanent.
    // A bare 400 is normally permanent too, but several OpenAI-compatible
    // gateways wrap an upstream capacity/5xx failure in a 400 response, so
    // inspect its body before deciding.
    if matches!(status, 401 | 403 | 404) {
        return false;
    }
    if matches!(status, 408 | 425 | 429 | 502 | 503 | 504 | 529)
        || (500..=599).contains(&status) && !matches!(status, 501 | 505)
    {
        return true;
    }

    let lower = body.to_lowercase();
    let transient_text = [
        "service unavailable",
        "temporarily unavailable",
        "temporary failure",
        "try again",
        "rate limit",
        "rate-limited",
        "ratelimit",
        "overloaded",
        "over capacity",
        "capacity",
        "upstream timeout",
        "upstream error",
        "provider returned error",
        "provider error",
        "error code 429",
        "error code 500",
        "error code 502",
        "error code 503",
        "error code 504",
        "\"code\":429",
        "\"code\":500",
        "\"code\":502",
        "\"code\":503",
        "\"code\":504",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    // Body-based detection is intentionally allowed for 400: routers such
    // as model aggregators frequently translate an upstream failure to 400.
    // Do not retry other client errors (for example 409/422) based on vague
    // text because the request itself generally needs to change.
    transient_text && (status == 400 || status >= 500)
}

/// Client with Go's 10-minute provider timeout. Shared so connections
/// pool across rounds and retries.
pub fn long_timeout_client() -> &'static reqwest::Client {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(10 * 60))
            .build()
            .expect("reqwest client")
    });
    &CLIENT
}

/// doHTTPWithRetry runs the request, retrying retryable failures with
/// the fixed delay table. The request closure is re-invoked on every
/// attempt (Go's newReq). Transport errors and non-2xx non-retryable
/// responses surface immediately as errors; non-2xx retryable ones
/// become ProviderHTTPError after the delays run out.
///
/// Unlike Go there is no explicit ctx parameter; callers cancel by
/// dropping the future or racing it against a cancellation signal.
pub async fn do_http_with_retry<F, Fut>(mut new_req: F) -> anyhow::Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt: usize = 0;
    loop {
        let resp = match new_req().await {
            Ok(resp) => resp,
            Err(err) => {
                // Request-builder/configuration errors are deterministic.
                // Connection, timeout, and body I/O errors are transient and
                // safe to retry because provider requests are rebuilt and a
                // failed send produced no usable streaming response.
                let retryable = err.is_connect() || err.is_timeout() || err.is_body();
                if retryable && attempt < provider_retry_delays().len() {
                    tokio::time::sleep(provider_retry_delays()[attempt]).await;
                    attempt += 1;
                    continue;
                }
                return Err(err.into());
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        // Honor a provider's Retry-After seconds when present, while capping
        // it so a bad gateway cannot wedge a turn indefinitely.
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds.min(60)));
        // Keep at most 4096 bytes of the error body, like Go's
        // io.LimitReader(resp.Body, 4096).
        let mut snippet: Vec<u8> = Vec::new();
        let mut resp = resp;
        while snippet.len() < 4096 {
            match resp.chunk().await {
                Ok(Some(b)) => snippet.extend_from_slice(&b),
                Ok(None) | Err(_) => break,
            }
        }
        snippet.truncate(4096);
        let body = String::from_utf8_lossy(&snippet).trim().to_string();
        if is_retryable_provider_error(status.as_u16(), &body)
            && attempt < provider_retry_delays().len()
        {
            let delay = retry_after.unwrap_or(provider_retry_delays()[attempt]);
            tokio::time::sleep(delay).await;
            attempt += 1;
            continue;
        }
        return Err(anyhow::Error::new(ProviderHTTPError {
            status: status.to_string(),
            status_code: status.as_u16(),
            body,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{test_lock, StubServer};
    use std::sync::{Arc, Mutex};

    #[test]
    fn is_retryable_provider_error_table() {
        let cases: &[(u16, &str, bool)] = &[
            (503, "", true),
            (429, "", true),
            (502, "", true),
            (504, "", true),
            (529, "", true),
            (
                500,
                r#"{"error":{"message":"Provider returned error","code":503}}"#,
                true,
            ),
            (
                400,
                r#"{"error":{"message":"Provider returned error","code":503}}"#,
                true,
            ),
            (500, "upstream provider rate limit", true),
            (500, "SERVICE UNAVAILABLE", true),
            (500, "internal", true),
            (408, "", true),
            (425, "", true),
            (400, "service unavailable", true),
            (400, "invalid tool schema", false),
            (401, "rate limit", false),
            (404, "", false),
            (422, "capacity", false),
        ];
        for (status, body, want) in cases {
            assert_eq!(
                is_retryable_provider_error(*status, body),
                *want,
                "status={} body={:?}",
                status,
                body
            );
        }
    }

    #[test]
    fn incomplete_reasoning_stream_cases() {
        let mk =
            |reasoning: &str, content: &str, tools: bool, usage: bool, finish: &str| StreamResult {
                content: content.into(),
                reasoning: reasoning.into(),
                tool_calls: if tools {
                    vec![crate::types::ToolCall {
                        id: "1".into(),
                        ..Default::default()
                    }]
                } else {
                    vec![]
                },
                usage: if usage {
                    Some(crate::types::StreamUsage {
                        total_tokens: 10,
                        ..Default::default()
                    })
                } else {
                    None
                },
                finish_reason: finish.into(),
                ..Default::default()
            };
        assert!(incomplete_reasoning_stream(&mk(
            "plan", "", false, false, ""
        )));
        assert!(incomplete_reasoning_stream(&mk(
            "plan", "", false, true, "length"
        )));
        assert!(!incomplete_reasoning_stream(&mk(
            "plan", "", false, true, "stop"
        )));
        assert!(!incomplete_reasoning_stream(&mk(
            "plan", "ok", false, false, ""
        )));
        assert!(!incomplete_reasoning_stream(&mk(
            "plan", "", true, false, ""
        )));
        assert!(!incomplete_reasoning_stream(&mk("", "", false, false, "")));

        assert!(should_nudge_incomplete_reasoning(
            &mk("plan", "", false, false, ""),
            0
        ));
        assert!(!should_nudge_incomplete_reasoning(
            &mk("plan", "", false, false, ""),
            MAX_REASONING_NUDGES
        ));
    }

    #[test]
    fn retry_delay_table_matches_go() {
        let ms: Vec<u64> = PROVIDER_RETRY_DELAYS_MS.to_vec();
        assert_eq!(
            ms,
            vec![670, 1200, 1400, 2000, 2400, 2600, 3000, 5000, 10000, 15000]
        );
    }

    #[tokio::test]
    async fn do_http_with_retry_503_then_200() {
        let _g = test_lock();
        let n = Arc::new(Mutex::new(0usize));
        let n2 = n.clone();
        let srv = StubServer::spawn(2, move |_i, _req| {
            let mut c = n2.lock().unwrap();
            *c += 1;
            if *c == 1 {
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 19\r\nConnection: close\r\n\r\nService Unavailable\n".to_string()
            } else {
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string()
            }
        });
        let url = format!("http://{}/x", srv.addr);
        let resp = do_http_with_retry(|| {
            let url = url.clone();
            long_timeout_client()
                .post(url)
                .body("{\"n\":1}".to_string())
                .send()
        })
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(*n.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn do_http_with_retry_400_no_retry() {
        let _g = test_lock();
        let n = Arc::new(Mutex::new(0usize));
        let n2 = n.clone();
        let srv = StubServer::spawn(1, move |_i, _req| {
            *n2.lock().unwrap() += 1;
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nbad request".to_string()
        });
        let url = format!("http://{}/x", srv.addr);
        let err = do_http_with_retry(|| {
            let url = url.clone();
            long_timeout_client().post(url).body("{}").send()
        })
        .await
        .unwrap_err();
        assert_eq!(*n.lock().unwrap(), 1);
        let pe = err
            .downcast_ref::<ProviderHTTPError>()
            .expect("provider http error");
        assert_eq!(pe.status_code, 400);
        assert!(err.to_string().starts_with("400 Bad Request: "));
    }

    #[tokio::test]
    async fn do_http_with_retry_retries_transport_error() {
        let _g = test_lock();
        // Bind a listener, learn its port, close it: nothing will answer.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let url = format!("http://{}", addr);
        let attempts = Arc::new(Mutex::new(0usize));
        let attempts2 = attempts.clone();
        let future = do_http_with_retry(|| {
            *attempts2.lock().unwrap() += 1;
            let url = url.clone();
            long_timeout_client().post(url).body("{}").send()
        });
        // The first backoff is 670ms. Cancel after observing the second
        // attempt rather than making this test wait through the full table.
        let res = tokio::time::timeout(Duration::from_millis(900), future).await;
        assert!(res.is_err(), "retry loop unexpectedly finished");
        assert!(*attempts.lock().unwrap() >= 2);
    }

    #[test]
    fn provider_http_error_display() {
        let e = ProviderHTTPError {
            status: "503 Service Unavailable".into(),
            status_code: 503,
            body: "down".into(),
        };
        assert_eq!(e.to_string(), "503 Service Unavailable: down");
    }
}
