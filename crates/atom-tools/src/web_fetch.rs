//! webfetch tool, ported from opencode's webfetch.ts: fetch a single
//! URL and return its content as markdown (default), text, or html.
//!
//! Same shape as opencode's tool:
//!   - Browser User-Agent by default so sites that block non-browser
//!     clients don't 403 us on first contact.
//!   - On a 403 with `cf-mitigated: challenge` (Cloudflare bot
//!     challenge), retry once with an honest `User-Agent: atom`.
//!     Sites running Anubis/Cloudflare actively object to UA spoofing
//!     (#2228); falling back to an honest UA when we know we've been
//!     challenged is the polite move.
//!   - 5MB response cap (mirrors opencode); default 30s timeout,
//!     capped at 120s.
//!   - Image responses return as a base64 attachment so vision
//!     models see them inline.
//!
//! distinct from web_search, which is an MCP-backed query API; this
//! tool takes a URL the model already has.

use crate::{ToolCtx, ToolOutcome};
use atom_core::types::ImageData;
use base64::Engine;

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const HONEST_UA: &str = "atom";

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Args {
    url: String,
    format: Option<String>,
    timeout: Option<u64>,
}

pub async fn web_fetch(arguments: &str, _ctx: &ToolCtx<'_>) -> ToolOutcome {
    if arguments.trim().is_empty() {
        return ToolOutcome::from_text(crate::exec::empty_arguments_msg("webfetch"));
    }
    let args: Args = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::from_text(format!("error parsing arguments: {e}")),
    };
    let format = match parse_format(args.format.as_deref()) {
        Ok(f) => f,
        Err(e) => return ToolOutcome::from_text(e),
    };

    let timeout_secs = args
        .timeout
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);

    match fetch(&args.url, format, timeout_secs).await {
        Ok(outcome) => outcome,
        Err(e) => ToolOutcome::from_text(format!("webfetch error: {e}")),
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Format {
    Markdown,
    Text,
    Html,
}

fn parse_format(value: Option<&str>) -> Result<Format, String> {
    match value
        .unwrap_or("markdown")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "markdown" | "md" => Ok(Format::Markdown),
        "text" | "txt" => Ok(Format::Text),
        "html" | "htm" => Ok(Format::Html),
        other => Err(format!(
            "error: invalid format \"{other}\" (expected markdown, text, or html)"
        )),
    }
}

async fn fetch(url: &str, format: Format, timeout_secs: u64) -> Result<ToolOutcome, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("URL must start with http:// or https://".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("build client: {e}"))?;

    let accept = match format {
        Format::Markdown => {
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, \
             text/html;q=0.7, */*;q=0.1"
        }
        Format::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        Format::Html => {
            "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, \
             text/markdown;q=0.7, */*;q=0.1"
        }
    };
    let base_headers = [
        ("User-Agent", BROWSER_UA),
        ("Accept", accept),
        ("Accept-Language", "en-US,en;q=0.9"),
    ];

    let resp = request_with_cf_retry(&client, &parsed, &base_headers).await?;

    // Content-length gate: bail before downloading the body.
    if let Some(cl) = resp.content_length() {
        if cl as usize > MAX_RESPONSE_SIZE {
            return Err(format!(
                "Response too large (content-length {} exceeds 5MB limit)",
                cl
            ));
        }
    }

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let bytes = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;
    if bytes.len() > MAX_RESPONSE_SIZE {
        return Err(format!(
            "Response too large ({} bytes exceeds 5MB limit)",
            bytes.len()
        ));
    }

    // Mirror opencode's "<url> (<content-type>)" header for the TUI block.
    let title = format!("{url} ({content_type})");

    if let Some(img_mime) = image_mime_from_content_type(&mime) {
        // 5MB cap already enforced above; image source limit is 20MB so
        // we're well inside both.
        if bytes.len() > atom_core::types::MAX_IMAGE_SOURCE_BYTES {
            return Err(format!(
                "image is {} bytes, larger than {}-byte source limit",
                bytes.len(),
                atom_core::types::MAX_IMAGE_SOURCE_BYTES
            ));
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok(ToolOutcome {
            text: format!("Image fetched successfully ({title})"),
            images: vec![ImageData {
                mime: img_mime.to_string(),
                data: b64,
            }],
            diff: String::new(),
        });
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();

    // Format dispatch: mirror opencode's switch (format × content-type).
    let output = match format {
        Format::Html => text,
        Format::Markdown => {
            if content_type.to_ascii_lowercase().contains("text/html") {
                convert_html_to_markdown(&text)
            } else {
                text
            }
        }
        Format::Text => {
            if content_type.to_ascii_lowercase().contains("text/html") {
                extract_text_from_html(&text)
            } else {
                text
            }
        }
    };

    if !status.is_success() {
        return Err(format!(
            "HTTP {} {}{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            if output.is_empty() {
                String::new()
            } else {
                format!("\n{output}")
            }
        ));
    }

    Ok(ToolOutcome {
        text: output,
        images: Vec::new(),
        diff: String::new(),
    })
}

/// First try with the browser UA. On a 403 with the Cloudflare
/// `cf-mitigated: challenge` header (active bot challenge), retry
/// once with an honest `User-Agent: atom`.
///
/// Identical in shape to opencode's `Effect.catchIf` block. Sites
/// running Anubis have asked for the honest fallback; we comply when
/// we know we've been challenged.
async fn request_with_cf_retry(
    client: &reqwest::Client,
    url: &reqwest::Url,
    headers: &[(&str, &str); 3],
) -> Result<reqwest::Response, String> {
    let resp = client
        .get(url.as_str())
        .headers(headers_to_map(headers))
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;
    if resp.status().as_u16() != 403 {
        return Ok(resp);
    }
    let cf = resp
        .headers()
        .get("cf-mitigated")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if cf != "challenge" {
        return Ok(resp);
    }
    // Drop the failed body so reqwest can reuse the connection.
    drop(resp);
    let mut honest = *headers;
    honest[0].1 = HONEST_UA;
    client
        .get(url.as_str())
        .headers(headers_to_map(&honest))
        .send()
        .await
        .map_err(|e| format!("request (honest UA retry): {e}"))
}

fn headers_to_map(headers: &[(&str, &str)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            map.insert(name, value);
        }
    }
    map
}

fn image_mime_from_content_type(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/bmp" => Some("image/bmp"),
        "image/svg+xml" => Some("image/svg+xml"),
        "image/avif" => Some("image/avif"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// HTML → Markdown (via htmd).
//
// htmd already strips <script>, <style>, and similar inert content via
// its own defaults; that mirrors opencode's `turndown.remove(...)` list
// (script, style, meta, link). One crate, no second parser to maintain.
// ---------------------------------------------------------------------------

fn convert_html_to_markdown(html: &str) -> String {
    match htmd::convert(html) {
        Ok(md) => md,
        Err(_) => html.to_string(), // fall back to raw html on parse error
    }
}

// ---------------------------------------------------------------------------
// HTML → plain text.
//
// Streaming tag walk: emit text outside `<script>`/`<style>`/
// `<noscript>`/`<iframe>`/`<object>`/`<embed>`/`<head>`. Mirrors
// opencode's htmlparser2-based extractor (skipDepth counter).
// Best-effort on malformed HTML — text mode is already a coarse view
// of the page.
// ---------------------------------------------------------------------------

const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "object", "embed", "head",
];

fn extract_text_from_html(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len / 3);
    let mut skip_depth: usize = 0;
    let mut text_start: usize = 0;
    let mut i: usize = 0;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Emit text from [text_start, i) if we're not in a skipped subtree.
        if skip_depth == 0 && i > text_start {
            push_decoded(&mut out, &html[text_start..i]);
        }
        // Find the tag's closing '>'. html5 allows '>' inside quoted
        // attrs, but for text extraction that's noise — accept the
        // occasional false break (we'd just emit a tag fragment).
        let tag_open = i + 1;
        let mut tag_end = tag_open;
        while tag_end < len && bytes[tag_end] != b'>' {
            tag_end += 1;
        }
        if tag_end >= len {
            // Unterminated tag; stop walking.
            break;
        }
        let tag_inner = &html[tag_open..tag_end];
        let (is_close, name) = parse_tag(tag_inner);
        if let Some(name) = name {
            let skip = SKIP_TAGS.contains(&name);
            if skip {
                if is_close {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
            }
        }
        i = tag_end + 1;
        text_start = i;
    }
    if skip_depth == 0 && text_start < len {
        push_decoded(&mut out, &html[text_start..]);
    }
    collapse_whitespace(&out).trim().to_string()
}

fn parse_tag(inner: &str) -> (bool, Option<&str>) {
    let trimmed = inner.trim_start();
    let is_close = trimmed.starts_with('/');
    let after = if is_close { &trimmed[1..] } else { trimmed };
    let bytes = after.as_bytes();
    let mut j = 0;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
        j += 1;
    }
    if j == 0 {
        // comment `<!--`, doctype, processing instruction, etc.
        (is_close, None)
    } else {
        (is_close, Some(&after[..j]))
    }
}

fn push_decoded(out: &mut String, s: &str) {
    // Inline entity decoder for the small set that matters. Full
    // markup5ever decode would be overkill — text mode is already a
    // lossy view.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some((consumed, ch)) = decode_entity(&s[i..]) {
                out.push(ch);
                i += consumed;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn decode_entity(s: &str) -> Option<(usize, char)> {
    let rest = s.strip_prefix('&')?;
    let semi = rest.find(';')?;
    let entity = &rest[..semi];
    let total = 1 + semi + 1; // & + body + ;
    let ch = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)?
            } else {
                let dec = entity.strip_prefix('#')?;
                dec.parse::<u32>().ok().and_then(char::from_u32)?
            }
        }
    };
    Some((total, ch))
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::test_support::test_ctx;

    /// Minimal one-shot HTTP server for tests: handles one request,
    /// returns the canned response, and forwards the raw request to a
    /// verifier so tests can assert headers.
    async fn serve(response: &'static str, check: impl Fn(&[u8]) + Send + 'static) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            check(&buf[..n]);
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn ok_html(body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    fn ok_text(body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn markdown_default_for_html() {
        let html = "<h1>Hi</h1><p>Hello <strong>world</strong></p>";
        let url = serve(ok_html(html), |req| {
            let low = String::from_utf8_lossy(req).to_lowercase();
            assert!(low.contains("user-agent: mozilla"), "{low}");
            assert!(
                low.contains("accept:") && low.contains("text/markdown;q=1.0"),
                "{low}"
            );
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}"}}"#), &ctx).await;
        assert!(out.text.contains("# Hi"), "{}", out.text);
        assert!(out.text.contains("**world**"), "{}", out.text);
        assert!(out.images.is_empty());
    }

    #[tokio::test]
    async fn text_format_strips_html() {
        let html = "<p>Hello</p><script>alert(1)</script>\
                    <style>body{}</style><h1>Title</h1>";
        let url = serve(ok_html(html), |_| {}).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}","format":"text"}}"#), &ctx).await;
        assert!(out.text.contains("Hello"), "{}", out.text);
        assert!(out.text.contains("Title"), "{}", out.text);
        assert!(!out.text.contains("alert"), "{}", out.text);
        assert!(!out.text.contains("body{}"), "{}", out.text);
    }

    #[tokio::test]
    async fn html_format_passes_through() {
        let html = "<p>raw &amp; unchanged</p>";
        let url = serve(ok_html(html), |_| {}).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}","format":"html"}}"#), &ctx).await;
        assert!(
            out.text.contains("<p>raw &amp; unchanged</p>"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn non_html_response_passes_through_for_markdown() {
        let url = serve(ok_text("just plain text"), |_| {}).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}"}}"#), &ctx).await;
        assert_eq!(out.text, "just plain text");
    }

    #[tokio::test]
    async fn cloudflare_challenge_falls_back_to_honest_ua() {
        // Two-stage server: first request gets a 403 cf-mitigated
        // challenge, second gets 200. The verifier confirms the second
        // request's UA switched to "atom".
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let saw_honest = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_honest_clone = saw_honest.clone();
        tokio::spawn(async move {
            // First connection: 403 challenge.
            let (mut s1, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let _ = s1.read(&mut buf).await.unwrap_or(0);
            let _ = s1
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\n\
                      cf-mitigated: challenge\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = s1.shutdown().await;

            // Second connection: success, and assert the UA is honest.
            let (mut s2, _) = listener.accept().await.unwrap();
            let n = s2.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            if req.contains("user-agent: atom") {
                *saw_honest_clone.lock().unwrap() = true;
            }
            let body = b"hello";
            let _ = s2
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            let _ = s2.write_all(body).await;
            let _ = s2.shutdown().await;
        });
        let url = format!("http://{addr}");
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}"}}"#), &ctx).await;
        assert_eq!(out.text, "hello");
        assert!(*saw_honest.lock().unwrap(), "expected retry with honest UA");
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(r#"{"url":"file:///etc/passwd"}"#, &ctx).await;
        assert!(
            out.text.contains("must start with http:// or https://"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn rejects_invalid_format() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(r#"{"url":"http://example.com","format":"xml"}"#, &ctx).await;
        assert!(out.text.contains("invalid format"), "{}", out.text);
    }

    #[tokio::test]
    async fn empty_arguments_friendly_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch("", &ctx).await;
        assert!(
            out.text.starts_with("error parsing arguments:"),
            "{}",
            out.text
        );
        assert!(out.text.contains("\"webfetch\""), "{}", out.text);
    }

    #[tokio::test]
    async fn timeout_is_capped_at_max() {
        // timeout > MAX should be clamped; we don't actually wait,
        // we just assert parse + dispatch don't reject a large value.
        let url = serve(ok_text("ok"), |_| {}).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}","timeout":999}}"#), &ctx).await;
        assert_eq!(out.text, "ok");
    }

    #[test]
    fn parse_format_aliases() {
        assert_eq!(parse_format(None).unwrap(), Format::Markdown);
        assert_eq!(parse_format(Some("markdown")).unwrap(), Format::Markdown);
        assert_eq!(parse_format(Some("md")).unwrap(), Format::Markdown);
        assert_eq!(parse_format(Some("text")).unwrap(), Format::Text);
        assert_eq!(parse_format(Some("txt")).unwrap(), Format::Text);
        assert_eq!(parse_format(Some("html")).unwrap(), Format::Html);
        assert_eq!(parse_format(Some("htm")).unwrap(), Format::Html);
        assert!(parse_format(Some("xml")).is_err());
    }

    #[test]
    fn extract_text_drops_script_style() {
        let html = "<p>before</p>\
                    <script>var s=1;</script>\
                    <p>after<script>nested</script>end</p>\
                    <style>body{}</style>";
        let out = extract_text_from_html(html);
        assert!(out.contains("before"), "{out}");
        assert!(out.contains("after"), "{out}");
        assert!(out.contains("end"), "{out}");
        assert!(!out.contains("var s"), "{out}");
        assert!(!out.contains("nested"), "{out}");
        assert!(!out.contains("body{}"), "{out}");
    }

    #[test]
    fn extract_text_decodes_named_entities() {
        let html = "<p>Tom &amp; Jerry &lt;3 &quot;cheese&quot;</p>";
        let out = extract_text_from_html(html);
        assert!(out.contains("Tom & Jerry <3 \"cheese\""), "{out}");
    }

    #[test]
    fn extract_text_decodes_numeric_entities() {
        let html = "<p>&#x2603; &#9731; &#34;</p>";
        let out = extract_text_from_html(html);
        assert!(out.contains("☃"), "{out}");
        assert!(out.contains("☃"), "{out}"); // snowman in both forms
        assert!(out.contains('"'), "{out}");
    }

    #[test]
    fn extract_text_collapses_whitespace() {
        let html = "<p>  hello   \n\n  world  </p>";
        let out = extract_text_from_html(html);
        assert_eq!(out, "hello world");
    }

    #[test]
    fn extract_text_handles_malformed_unterminated() {
        let html = "<p>first</p><p>second <unterminated";
        let out = extract_text_from_html(html);
        assert!(out.contains("first"), "{out}");
        assert!(out.contains("second"), "{out}");
    }
}
