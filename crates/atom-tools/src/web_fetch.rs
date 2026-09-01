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
//!
//! Provider chain: the user-selected fetch provider (via
//! AtomConfig.resolved_web_fetch()) is tried first; on an auth/quota
//! rejection (401/403/402/429) the remaining bundled providers are
//! tried in priority order (tinyfish -> parallel -> exa -> ollama),
//! skipping paid providers with no key configured. A direct reqwest
//! fetch is the last resort.

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

    // Cloud fetch providers can't reach loopback/private targets, and
    // handing them one just burns latency; go straight to the direct
    // fetcher for local URLs (also keeps the local-server tests
    // hermetic).
    if !is_provider_fetchable(&args.url) {
        return run_direct_fetch(&args.url, format, timeout_secs).await;
    }

    let selected = atom_core::config::load().resolved_web_fetch().server;
    let order = provider_order(&selected);

    match try_provider_chain(&order, |id| {
        let url = args.url.clone();
        async move {
            let key = crate::auth_keys::resolve_provider_key(id);
            match id {
                "tinyfish" => fetch_tinyfish(&url, &key, format).await,
                "parallel" => fetch_parallel(&url, &key, format).await,
                "exa" => fetch_exa(&url, &key, format).await,
                "ollama" => fetch_ollama(&url, &key, format).await,
                _ => Err(FetchError::Transport(format!("unknown provider {id}"))),
            }
        }
    })
    .await
    {
        ChainResult::Success(outcome) => return outcome,
        ChainResult::BadRequest(msg) | ChainResult::Transport(msg) => {
            return ToolOutcome::from_text(format!("webfetch error: {msg}"));
        }
        ChainResult::Exhausted(fallbacks) => {
            if !fallbacks.is_empty() {
                eprintln!("webfetch: all providers exhausted; using direct fetch");
            }
        }
    }

    // Last resort: direct reqwest fetch (the pre-provider path).
    let url = args.url.clone();
    run_direct_fetch(&url, format, timeout_secs).await
}

async fn run_direct_fetch(url: &str, format: Format, timeout_secs: u64) -> ToolOutcome {
    let mut outcome = match fetch(url, format, timeout_secs).await {
        Ok(outcome) => outcome,
        Err(e) => ToolOutcome::from_text(format!("webfetch error: {e}")),
    };
    outcome.tool_provider = "direct".into();
    outcome
}

/// True when the URL can be handed to a cloud fetch provider: an
/// http(s) URL that is not a loopback target (providers can't reach
/// localhost/127.0.0.1/::1, and non-http schemes just confuse them).
/// Non-http URLs and loopback targets fall through to the direct
/// fetcher (which also keeps the local-server tests hermetic).
fn is_provider_fetchable(url: &str) -> bool {
    let parsed = match reqwest::Url::parse(url) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    !matches!(
        parsed.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("[::ffff:127.0.0.1]")
    )
}

// ---------------------------------------------------------------------------
// Provider chain
// ---------------------------------------------------------------------------

const TINYFISH_ENDPOINT: &str = "https://api.fetch.tinyfish.ai";
const PARALLEL_ENDPOINT: &str = "https://api.parallel.ai/v1/extract";
const EXA_ENDPOINT: &str = "https://api.exa.ai/contents";
const OLLAMA_ENDPOINT: &str = "https://ollama.com/api/web_fetch";

/// Bundled fetch providers in priority order, with the user-selected
/// one moved to the front. Unknown/empty selections just get the base
/// order.
fn provider_order(selected: &str) -> Vec<&'static str> {
    let base: [&'static str; 4] = ["tinyfish", "parallel", "exa", "ollama"];
    let selected = selected.trim();
    let matched = base.iter().find(|b| **b == selected).copied();
    let mut out: Vec<&'static str> = base
        .iter()
        .copied()
        .filter(|p| Some(*p) != matched)
        .collect();
    if let Some(m) = matched {
        out.insert(0, m);
    }
    out
}

/// A provider joins the chain iff its credentials can be resolved
/// (env > auth.json > legacy), except tinyfish and parallel which are
/// always available (free tiers, key optional; exa is paid, ollama
/// requires a key).
fn provider_available(id: &str) -> bool {
    match id {
        "tinyfish" | "parallel" => true,
        "exa" | "ollama" => !crate::auth_keys::resolve_provider_key(id).trim().is_empty(),
        _ => false,
    }
}

/// How a provider attempt failed, and whether that failure should
/// trigger a fallback or abort the chain.
#[derive(Debug)]
enum FetchError {
    /// 401/402/403/429 — the provider rejected our credentials or
    /// quota; walk down the chain.
    AuthOrQuota(String),
    /// 400/404/410/422 — likely our request is wrong; don't retry
    /// against other providers with the same bug.
    BadRequest(String),
    /// Network failure or 5xx — the provider is flaky/unreachable;
    /// surface it rather than silently hiding it.
    Transport(String),
}

fn classify_status(status: reqwest::StatusCode, body: &[u8]) -> Result<(), FetchError> {
    let code = status.as_u16();
    let preview = |body: &[u8]| -> String {
        String::from_utf8_lossy(&body[..body.len().min(200)])
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    };
    match code {
        200..=299 => Ok(()),
        400 | 404 | 410 | 422 => Err(FetchError::BadRequest(format!(
            "HTTP {code}: {}",
            preview(body)
        ))),
        401 | 402 | 403 | 429 => Err(FetchError::AuthOrQuota(format!(
            "HTTP {code}: {}",
            preview(body)
        ))),
        _ => Err(FetchError::Transport(format!("HTTP {code}"))),
    }
}

fn format_label(format: Format) -> &'static str {
    match format {
        Format::Markdown => "markdown",
        Format::Text => "text",
        Format::Html => "html",
    }
}

fn content_type_label(format: Format) -> &'static str {
    match format {
        Format::Markdown => "text/markdown",
        Format::Text => "text/plain",
        Format::Html => "text/html",
    }
}

fn provider_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| FetchError::Transport(format!("build client: {e}")))
}

/// Outcome of walking the provider chain.
enum ChainResult {
    Success(ToolOutcome),
    BadRequest(String),
    Transport(String),
    /// Every available provider rejected us with AuthOrQuota.
    Exhausted(Vec<String>),
}

/// Try each available provider in `order`, walking down on
/// AuthOrQuota errors only. BadRequest and Transport abort the chain
/// immediately.
async fn try_provider_chain<F, Fut>(order: &[&'static str], mut attempt: F) -> ChainResult
where
    F: FnMut(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<ToolOutcome, FetchError>>,
{
    let mut fallbacks: Vec<String> = Vec::new();
    for id in order {
        if !provider_available(id) {
            continue;
        }
        match attempt(id).await {
            Ok(mut outcome) => {
                outcome.tool_provider = (*id).to_string();
                return ChainResult::Success(outcome);
            }
            Err(FetchError::AuthOrQuota(msg)) => {
                eprintln!("webfetch: {id} -> {msg}; falling back");
                fallbacks.push(format!("{id}: {msg}"));
            }
            Err(FetchError::BadRequest(msg)) => return ChainResult::BadRequest(msg),
            Err(FetchError::Transport(msg)) => return ChainResult::Transport(msg),
        }
    }
    ChainResult::Exhausted(fallbacks)
}

// ---------------------------------------------------------------------------
// Per-provider adapters. Each owns one cloud fetch API and normalizes
// its response to a plain ToolOutcome text blob.
// ---------------------------------------------------------------------------

async fn fetch_tinyfish(url: &str, key: &str, format: Format) -> Result<ToolOutcome, FetchError> {
    fetch_tinyfish_at(TINYFISH_ENDPOINT, url, key, format).await
}

/// Endpoint-parameterized variant so tests can point the adapter at a
/// local mock server.
async fn fetch_tinyfish_at(
    endpoint: &str,
    url: &str,
    key: &str,
    format: Format,
) -> Result<ToolOutcome, FetchError> {
    let ct = content_type_label(format);
    let client = provider_client()?;
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"urls": [url], "format": format_label(format)}));
    if !key.trim().is_empty() {
        req = req.header("X-API-Key", key.trim());
    }
    fetch_json_extract(endpoint, ct, req, |parsed| {
        let results = parsed.get("results").and_then(|r| r.as_array());
        let text = results
            .and_then(|arr| arr.first())
            .and_then(|r| r.get("text"))
            .and_then(|t| t.as_str());
        match text {
            Some(text) => {
                let title = results
                    .and_then(|arr| arr.first())
                    .and_then(|r| r.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(url);
                Ok((title.to_string(), text.to_string()))
            }
            // An errors[] entry for our URL is also a hard failure of
            // this provider; treat as transport so the caller sees it.
            None => Err(FetchError::Transport(
                "tinyfish: no results in response".into(),
            )),
        }
    })
    .await
}

async fn fetch_parallel(url: &str, key: &str, format: Format) -> Result<ToolOutcome, FetchError> {
    fetch_parallel_at(PARALLEL_ENDPOINT, url, key, format).await
}

async fn fetch_parallel_at(
    endpoint: &str,
    url: &str,
    key: &str,
    _format: Format,
) -> Result<ToolOutcome, FetchError> {
    let client = provider_client()?;
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"url": url, "objective": ""}));
    if !key.trim().is_empty() {
        req = req.header("Authorization", format!("Bearer {}", key.trim()));
    }
    fetch_json_extract(endpoint, "text/markdown", req, |parsed| {
        // Response shape varies; defensively probe content[0].text,
        // then top-level text/markdown, then any string field.
        chain_text_extraction(parsed).ok_or_else(|| {
            FetchError::Transport(format!(
                "parallel: unrecognized response shape: {}",
                truncate_for_error(parsed)
            ))
        })
    })
    .await
}

async fn fetch_exa(url: &str, key: &str, format: Format) -> Result<ToolOutcome, FetchError> {
    fetch_exa_at(EXA_ENDPOINT, url, key, format).await
}

async fn fetch_exa_at(
    endpoint: &str,
    url: &str,
    key: &str,
    _format: Format,
) -> Result<ToolOutcome, FetchError> {
    // Exa is paid; guard anyway in case of dispatch mistakes.
    if key.trim().is_empty() {
        return Err(FetchError::AuthOrQuota("exa: no key configured".into()));
    }
    let client = provider_client()?;
    let req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("x-api-key", key.trim())
        .json(&serde_json::json!({
            "urls": [url],
            "text": {"maxCharacters": 10000}
        }));
    fetch_json_extract(endpoint, "text/markdown", req, |parsed| {
        let text = parsed
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first())
            .and_then(|r| r.get("text"))
            .and_then(|t| t.as_str());
        match text {
            Some(text) => {
                let title = parsed
                    .get("results")
                    .and_then(|r| r.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|r| r.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(url);
                Ok((title.to_string(), text.to_string()))
            }
            None => Err(FetchError::Transport("exa: missing results[0].text".into())),
        }
    })
    .await
}

async fn fetch_ollama(url: &str, key: &str, format: Format) -> Result<ToolOutcome, FetchError> {
    fetch_ollama_at(OLLAMA_ENDPOINT, url, key, format).await
}

async fn fetch_ollama_at(
    endpoint: &str,
    url: &str,
    key: &str,
    _format: Format,
) -> Result<ToolOutcome, FetchError> {
    if key.trim().is_empty() {
        return Err(FetchError::AuthOrQuota("ollama: no key configured".into()));
    }
    let client = provider_client()?;
    let req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", key.trim()))
        .json(&serde_json::json!({"url": url}));
    fetch_json_extract(endpoint, "text/plain", req, |parsed| {
        let content = parsed.get("content").and_then(|c| c.as_str());
        match content {
            Some(content) => {
                let title = parsed.get("title").and_then(|t| t.as_str()).unwrap_or(url);
                Ok((title.to_string(), content.to_string()))
            }
            None => Err(FetchError::Transport(
                "ollama: missing content in response".into(),
            )),
        }
    })
    .await
}

/// Shared adapter plumbing: send a prepared POST, classify the HTTP
/// status, parse the JSON body, and hand control to `extract` to pull
/// title/text out of the provider-specific shape.
async fn fetch_json_extract(
    provider: &str,
    content_type: &'static str,
    req: reqwest::RequestBuilder,
    extract: impl Fn(&serde_json::Value) -> Result<(String, String), FetchError>,
) -> Result<ToolOutcome, FetchError> {
    let resp = req
        .send()
        .await
        .map_err(|e| FetchError::Transport(format!("{provider}: {e}")))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| FetchError::Transport(format!("{provider}: read body: {e}")))?;
    classify_status(status, &bytes).map_err(|e| match e {
        FetchError::AuthOrQuota(m) => FetchError::AuthOrQuota(format!("{provider}: {m}")),
        FetchError::BadRequest(m) => FetchError::BadRequest(format!("{provider}: {m}")),
        FetchError::Transport(m) => FetchError::Transport(format!("{provider}: {m}")),
    })?;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| FetchError::BadRequest(format!("{provider}: {e}")))?;
    let (title, text) = extract(&parsed)?;
    Ok(ToolOutcome::from_text(format!(
        "{title} ({content_type})\n\n{text}"
    )))
}

/// Defensive text extraction for providers whose response shape we
/// model loosely: probe content[0].text, then top-level text /
/// markdown / content strings.
fn chain_text_extraction(parsed: &serde_json::Value) -> Option<(String, String)> {
    let from_content_array = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find_map(|item| item.get("text").and_then(|t| t.as_str()))
        });
    let top_level = ["text", "markdown", "content"]
        .iter()
        .find_map(|k| parsed.get(*k).and_then(|v| v.as_str()));
    let (title, text) = match (from_content_array, top_level) {
        (Some(text), _) => (None, text),
        (_, Some(text)) => (None, text),
        _ => return None,
    };
    let title = title.unwrap_or("result");
    Some((title.to_string(), text.to_string()))
}

fn truncate_for_error(v: &serde_json::Value) -> String {
    let s = v.to_string();
    s.chars().take(200).collect()
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
            ..Default::default()
        });
    }

    // Refuse binary responses before converting them to text. Without
    // this gate, `String::from_utf8_lossy` would happily produce a
    // 100KB+ garbled blob for a PDF/ZIP/etc. that pollutes the model
    // context and reliably crashes the TUI renderer (which has byte
    // offsets in linkify / line-width that don't tolerate arbitrary
    // control bytes). Prefer a clean error the model can act on.
    if !is_text_like_response(&mime, &bytes) {
        let preview = mime_label(&mime);
        return Err(format!(
            "webfetch only handles text and image responses; this \
             {preview} body is {} bytes. Use `curl`/`wget` to download \
             it and convert locally (e.g. `pdftotext` for PDFs).",
            bytes.len()
        ));
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
        ..Default::default()
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

/// Returns true when the response looks like text we can render and
/// linkify safely. Two-pronged check: an allow-list of MIME prefixes
/// covers the common case, and a printability sniff on the first
/// non-empty chunk catches content-types that lie (e.g. a CDN serving
/// `text/html` but actually returning a binary blob) or omit the
/// content-type header entirely.
fn is_text_like_response(mime: &str, bytes: &[u8]) -> bool {
    // Common binary content-types: refuse even when the file looks
    // small. These never produce a useful text result via lossy
    // UTF-8 conversion and historically crash the TUI renderer.
    const BINARY_MIMES: &[&str] = &[
        "application/pdf",
        "application/zip",
        "application/x-tar",
        "application/gzip",
        "application/x-gzip",
        "application/x-bzip2",
        "application/x-7z-compressed",
        "application/x-rar-compressed",
        "application/octet-stream",
        "application/x-msdownload",
        "application/vnd.android.package-archive",
        "application/vnd.apple.installer+xml",
        "application/vnd.debian.binary-package",
        "application/vnd.rar",
        "application/x-executable",
        "application/x-sharedlib",
        "application/x-mach-binary",
        "application/wasm",
        "audio/",
        "video/",
        "font/",
    ];
    if BINARY_MIMES.iter().any(|m| mime.starts_with(m)) {
        return false;
    }
    // Known text-like: text/*, JSON/XML/JS/YAML/SVG-as-text. These are
    // *not* blindly trusted — a CDN can serve `text/plain` with a binary
    // payload — so every response still goes through the printability
    // sniff below.
    // Heuristic: a real text response has almost entirely printable
    // ASCII/Unicode characters in its first KB. PDFs / images / archives
    // have long runs of NULs and high bytes. We scan up to 1KB to keep
    // the cost bounded.
    let sample_len = bytes.len().min(1024);
    if sample_len == 0 {
        return true; // empty body is harmless
    }
    let sample = &bytes[..sample_len];
    let printable = sample
        .iter()
        .filter(|b| {
            b.is_ascii_graphic() || matches!(**b, b' ' | b'\n' | b'\r' | b'\t') || (**b >= 0x80)
        })
        .count();
    let ratio = printable as f64 / sample_len as f64;
    // 0.95 catches binary headers (PDF "%PDF-" is printable but the
    // body isn't) while leaving room for source code with the odd
    // tab/CR. Real text sits >0.99; binary sits <0.5.
    ratio >= 0.95
}

/// Pretty label for the "this is a binary response" error message.
fn mime_label(mime: &str) -> String {
    if mime.is_empty() {
        "untyped".to_string()
    } else {
        format!("{mime:?}")
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

    // -----------------------------------------------------------------
    // Binary-response guard (regression: PDF webfetch stuffed 119KB of
    // raw bytes into the model context and crashed the TUI renderer).
    // -----------------------------------------------------------------

    #[test]
    fn text_like_allows_text_and_known_text_types() {
        assert!(is_text_like_response("text/plain", b"hi"));
        assert!(is_text_like_response("text/html", b"<p>x</p>"));
        assert!(is_text_like_response("text/markdown", b"# x"));
        assert!(is_text_like_response("application/json", b"{\"a\":1}"));
        assert!(is_text_like_response("application/xml", b"<x/>"));
        assert!(is_text_like_response(
            "application/javascript",
            b"function f() {}"
        ));
        assert!(is_text_like_response("application/ld+json", b"{}"));
        // Empty body is harmless even with unknown content-type.
        assert!(is_text_like_response("", b""));
    }

    #[test]
    fn text_like_rejects_known_binary_types() {
        // The smoking-gun case from the m3.1 webfetch session.
        assert!(!is_text_like_response("application/pdf", b"%PDF-1.4..."));
        assert!(!is_text_like_response("application/pdf", b""));
        assert!(!is_text_like_response("application/zip", b""));
        assert!(!is_text_like_response(
            "application/octet-stream",
            b"\x00\x01\x02"
        ));
        assert!(!is_text_like_response("audio/mpeg", b""));
        assert!(!is_text_like_response("video/mp4", b""));
        assert!(!is_text_like_response("application/wasm", b""));
    }

    #[test]
    fn text_like_sniffs_untyped_or_unusual_bodies() {
        // 100KB of NULs + high bytes — clearly binary, even with no
        // content-type header.
        let mut binary = vec![0u8; 1024];
        binary.extend_from_slice(&[0xff, 0xfe, 0xfd, 0xfc, 0xfb]);
        assert!(!is_text_like_response("", &binary));

        // Real text with an unusual content-type still passes by sniff.
        let text = b"<html><body>hello world this is some text</body></html>";
        assert!(is_text_like_response("", text));
        assert!(is_text_like_response("application/x-httpd-php", text));

        // Mostly printable but sprinkled with control bytes — still
        // considered text if >= 95% printable (handles source code with
        // stray tabs/CRs).
        let source = b"fn main() {\n    println!(\"hi\");\n}\n";
        assert!(is_text_like_response("", source));
    }

    #[test]
    fn text_like_real_pdf_body_fails_sniff() {
        // Mimic the actual bytes from the m3.1 webfetch session: a
        // PDF that starts with the printable "%PDF-1.4" header and
        // then dives into binary streams. The 1KB sample has the
        // header chars plus enough binary to push printability below
        // the threshold.
        let mut pdf = b"%PDF-1.4\n".to_vec();
        pdf.extend(std::iter::repeat(0u8).take(700));
        pdf.extend_from_slice(&[0xffu8; 300]);
        assert!(!is_text_like_response("", &pdf));
    }

    #[tokio::test]
    async fn webfetch_rejects_pdf_with_actionable_error() {
        let url = serve(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/pdf\r\n\
             Content-Length: 11\r\nConnection: close\r\n\r\n\
             %PDF-1.4 hi",
            |_| {},
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}"}}"#), &ctx).await;
        assert!(
            out.text.contains("webfetch only handles text and image"),
            "{}",
            out.text
        );
        assert!(out.text.contains("application/pdf"), "{}", out.text);
        assert!(out.text.contains("curl"), "{}", out.text);
        assert!(out.text.contains("pdftotext"), "{}", out.text);
        assert!(out.images.is_empty());
    }

    #[tokio::test]
    async fn webfetch_rejects_octet_stream_via_sniff() {
        // Binary body with a lying "text/plain" content-type still gets
        // caught by the byte sniff.
        let url = serve(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: 16\r\nConnection: close\r\n\r\n\
             \x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
            |_| {},
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = web_fetch(&format!(r#"{{"url":"{url}"}}"#), &ctx).await;
        assert!(
            out.text.contains("webfetch only handles text and image"),
            "{}",
            out.text
        );
    }

    // -----------------------------------------------------------------
    // Provider chain (tiered fallback).
    // -----------------------------------------------------------------

    #[test]
    fn provider_order_moves_selected_to_front() {
        assert_eq!(
            provider_order("tinyfish"),
            vec!["tinyfish", "parallel", "exa", "ollama"]
        );
        assert_eq!(
            provider_order("exa"),
            vec!["exa", "tinyfish", "parallel", "ollama"]
        );
        // Unknown/empty selection keeps the base priority order.
        assert_eq!(
            provider_order(""),
            vec!["tinyfish", "parallel", "exa", "ollama"]
        );
        assert_eq!(
            provider_order("not-a-provider"),
            vec!["tinyfish", "parallel", "exa", "ollama"]
        );
    }

    #[test]
    fn provider_available_skips_exa_without_key() {
        // If a key is configured in the local auth store this test
        // can't clear it (auth_keys.rs owns the store), so degrade to
        // asserting only the env-var path.
        let locally_configured = atom_core::providers::auth::lookup_auth_entry("exa").is_some()
            || !atom_core::providers::auth::legacy_provider_key("exa").is_empty();

        let old = std::env::var("EXA_API_KEY").ok();
        std::env::remove_var("EXA_API_KEY");
        let without = provider_available("exa");
        std::env::set_var("EXA_API_KEY", "test-key-123");
        let with = provider_available("exa");
        // Restore regardless of assertion outcome.
        match old {
            Some(v) => std::env::set_var("EXA_API_KEY", v),
            None => std::env::remove_var("EXA_API_KEY"),
        }

        if locally_configured {
            eprintln!("skipping unavailable-assert: exa key in local auth store");
        } else {
            assert!(!without, "exa must be skipped with no key configured");
        }
        assert!(with, "exa must be available with EXA_API_KEY set");
        // Free tiers are always available.
        assert!(provider_available("tinyfish"));
        assert!(provider_available("parallel"));
    }

    #[test]
    fn classify_status_401_is_auth() {
        let err = classify_status(reqwest::StatusCode::UNAUTHORIZED, b"denied").unwrap_err();
        assert!(matches!(err, FetchError::AuthOrQuota(_)), "{err:?}");
    }

    #[test]
    fn classify_status_429_is_auth() {
        let err =
            classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, b"slow down").unwrap_err();
        assert!(matches!(err, FetchError::AuthOrQuota(_)), "{err:?}");
    }

    #[test]
    fn classify_status_500_is_transport() {
        let err = classify_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, b"boom").unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[test]
    fn classify_status_400_is_bad_request() {
        let err = classify_status(reqwest::StatusCode::BAD_REQUEST, b"bad").unwrap_err();
        assert!(matches!(err, FetchError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn classify_status_200_is_ok() {
        assert!(classify_status(reqwest::StatusCode::OK, b"{}").is_ok());
    }

    /// Stub dispatch test: a 401 from the first provider walks down to
    /// the second, whose success wins.
    #[tokio::test]
    async fn chain_falls_back_to_next_provider_on_auth_error() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let calls_c = calls.clone();
        let result = try_provider_chain(&["tinyfish", "parallel"], move |id| {
            let calls = calls_c.clone();
            async move {
                calls.lock().unwrap().push(id);
                match id {
                    "tinyfish" => Err(FetchError::AuthOrQuota("HTTP 401: denied".into())),
                    _ => Ok(ToolOutcome::from_text("second provider won".into())),
                }
            }
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), vec!["tinyfish", "parallel"]);
        let ChainResult::Success(outcome) = result else {
            panic!("expected the second provider to win");
        };
        assert_eq!(outcome.text, "second provider won");
    }

    #[tokio::test]
    async fn chain_transport_error_aborts_immediately() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let calls_c = calls.clone();
        let result = try_provider_chain(&["tinyfish", "parallel"], move |id| {
            let calls = calls_c.clone();
            async move {
                calls.lock().unwrap().push(id);
                Err(FetchError::Transport("HTTP 503: down".into()))
            }
        })
        .await;
        // No silent recovery: a single transport error ends the walk.
        assert_eq!(*calls.lock().unwrap(), vec!["tinyfish"]);
        assert!(
            matches!(result, ChainResult::Transport(_)),
            "expected transport"
        );
    }

    #[tokio::test]
    async fn chain_exhausted_when_all_providers_auth_fail() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let calls_c = calls.clone();
        // "mystery" is not a bundled provider and must be skipped
        // without calling the adapter.
        let result = try_provider_chain(&["mystery", "tinyfish", "parallel"], move |id| {
            let calls = calls_c.clone();
            async move {
                calls.lock().unwrap().push(id);
                Err(FetchError::AuthOrQuota("HTTP 429".into()))
            }
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), vec!["tinyfish", "parallel"]);
        let ChainResult::Exhausted(fallbacks) = result else {
            panic!("expected exhaustion");
        };
        assert_eq!(fallbacks.len(), 2);
    }

    fn leaked_response(status_line: &str, content_type: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn tinyfish_adapter_maps_401_to_auth_error() {
        let endpoint = serve(
            leaked_response(
                "HTTP/1.1 401 Unauthorized",
                "application/json",
                r#"{"error":"invalid key"}"#,
            ),
            |_| {},
        )
        .await;
        let err = fetch_tinyfish_at(
            &endpoint,
            "https://example.com/x",
            "bad-key",
            Format::Markdown,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FetchError::AuthOrQuota(_)), "{err:?}");
    }

    #[tokio::test]
    async fn tinyfish_adapter_parses_results_shape() {
        let body = r#"{"results":[{"url":"https://example.com","title":"Example","text":"hello there","format":"markdown"}]}"#;
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", body),
            |req| {
                let low = String::from_utf8_lossy(req).to_lowercase();
                assert!(low.contains("post /"), "{low}");
                assert!(low.contains("x-api-key:"), "{low}");
            },
        )
        .await;
        let out = fetch_tinyfish_at(
            &endpoint,
            "https://example.com",
            "test-key",
            Format::Markdown,
        )
        .await
        .unwrap();
        assert!(out.text.contains("Example (text/markdown)"), "{}", out.text);
        assert!(out.text.contains("hello there"), "{}", out.text);
    }

    #[tokio::test]
    async fn tinyfish_adapter_without_key_omits_header() {
        let body = r#"{"results":[{"url":"https://example.com","title":"T","text":"anon"}]}"#;
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", body),
            |req| {
                let low = String::from_utf8_lossy(req).to_lowercase();
                assert!(!low.contains("x-api-key:"), "{low}");
            },
        )
        .await;
        let out = fetch_tinyfish_at(&endpoint, "https://example.com", "", Format::Markdown)
            .await
            .unwrap();
        assert!(out.text.contains("anon"), "{}", out.text);
    }

    #[tokio::test]
    async fn parallel_adapter_parses_content_array() {
        let body = r#"{"content":[{"url":"https://example.com","title":"P","text":"parallel body text"}],"job_id":"abc"}"#;
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", body),
            |req| {
                let low = String::from_utf8_lossy(req).to_lowercase();
                assert!(low.contains("bearer test-key"), "{low}");
            },
        )
        .await;
        let out = fetch_parallel_at(
            &endpoint,
            "https://example.com",
            "test-key",
            Format::Markdown,
        )
        .await
        .unwrap();
        assert!(out.text.contains("parallel body text"), "{}", out.text);
    }

    #[tokio::test]
    async fn parallel_adapter_accepts_top_level_text() {
        let endpoint = serve(
            // Free tier without a key; loose shape variant.
            leaked_response(
                "HTTP/1.1 200 OK",
                "application/json",
                r#"{"text":"top-level body"}"#,
            ),
            |_| {},
        )
        .await;
        let out = fetch_parallel_at(&endpoint, "https://example.com", "", Format::Markdown)
            .await
            .unwrap();
        assert!(out.text.contains("top-level body"), "{}", out.text);
    }

    #[tokio::test]
    async fn parallel_adapter_unrecognized_shape_is_transport() {
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", r#"{"unexpected":1}"#),
            |_| {},
        )
        .await;
        let err = fetch_parallel_at(&endpoint, "https://example.com", "", Format::Markdown)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn exa_adapter_parses_results_text() {
        let body = r#"{"results":[{"title":"Exa Title","url":"https://example.com","text":"exa body text"}]}"#;
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", body),
            |req| {
                let low = String::from_utf8_lossy(req).to_lowercase();
                assert!(low.contains("x-api-key:"), "{low}");
            },
        )
        .await;
        let out = fetch_exa_at(
            &endpoint,
            "https://example.com",
            "exa-key",
            Format::Markdown,
        )
        .await
        .unwrap();
        assert!(out.text.contains("exa body text"), "{}", out.text);
        assert!(out.text.contains("Exa Title"), "{}", out.text);
    }

    #[tokio::test]
    async fn exa_adapter_missing_text_is_transport() {
        let endpoint = serve(
            leaked_response(
                "HTTP/1.1 200 OK",
                "application/json",
                r#"{"results":[{"/url":"x"}]}"#,
            ),
            |_| {},
        )
        .await;
        let err = fetch_exa_at(&endpoint, "https://example.com", "k", Format::Markdown)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn exa_adapter_requires_key() {
        // No key -> AuthOrQuota without touching the network.
        let endpoint = "http://127.0.0.1:1"; // nothing listening
        let err = fetch_exa_at(&endpoint, "https://example.com", "", Format::Markdown)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::AuthOrQuota(_)), "{err:?}");
    }

    #[tokio::test]
    async fn ollama_adapter_parses_content() {
        let body =
            r#"{"title":"Ollama Page","content":"ollama body text","links":["https://x.dev"]}"#;
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", body),
            |_| {},
        )
        .await;
        let out = fetch_ollama_at(
            &endpoint,
            "https://example.com",
            "test-key",
            Format::Markdown,
        )
        .await
        .unwrap();
        assert!(
            out.text.contains("Ollama Page (text/plain)"),
            "{}",
            out.text
        );
        assert!(out.text.contains("ollama body text"), "{}", out.text);
    }

    #[tokio::test]
    async fn ollama_adapter_requires_key() {
        let endpoint = serve(
            leaked_response("HTTP/1.1 200 OK", "application/json", r#"{"content":"x"}"#),
            |_| panic!("ollama must not be contacted without a key"),
        )
        .await;
        let err = fetch_ollama_at(&endpoint, "https://example.com", "", Format::Markdown)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::AuthOrQuota(_)), "{err:?}");
    }
}
