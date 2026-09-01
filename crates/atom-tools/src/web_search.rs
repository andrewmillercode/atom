//! web_search, ported from main.go webSearch/ollamaWebSearchKey.
//! Endpoint and client are injectable so tests run against a local
//! server (Go swapped package vars).

use crate::ToolOutcome;

const WEB_SEARCH_ENDPOINT: &str = "https://ollama.com/api/web_search";

/// ollamaWebSearchKey is the Ollama Cloud credential, resolved the same
/// way as the ollama provider in buildProviders (env, then auth store).
/// Chat may be on a different provider; search always uses this key.
pub fn ollama_web_search_key() -> String {
    resolve_ollama_web_search_key(
        std::env::var("OLLAMA_API_KEY").ok(),
        atom_core::providers::auth::lookup_auth_entry("ollama-cloud"),
        atom_core::providers::auth::legacy_provider_key("ollama-cloud"),
    )
}

fn resolve_ollama_web_search_key(
    env_key: Option<String>,
    auth: Option<atom_core::providers::auth::AuthEntry>,
    legacy_key: String,
) -> String {
    if let Some(key) = env_key.filter(|key| !key.trim().is_empty()) {
        return key;
    }
    if let Some(auth) = auth {
        let key = atom_core::providers::auth::auth_bearer("ollama-cloud", &auth);
        if !key.trim().is_empty() {
            return key;
        }
    }
    legacy_key.trim().to_string()
}

pub async fn web_search(query: &str, cwd: &std::path::Path) -> ToolOutcome {
    web_search_with_config(query, cwd, &atom_core::config::load()).await
}

async fn web_search_with_config(
    query: &str,
    cwd: &std::path::Path,
    config: &atom_core::config::AtomConfig,
) -> ToolOutcome {
    if query.trim().is_empty() {
        return ToolOutcome::from_text("search error: query is empty".into());
    }
    let selected = config.resolved_web_search();
    // Every bundled provider dispatches over direct HTTP (web_fetch
    // parity): the selected provider is tried first and the remaining
    // bundled ones follow as fallback on auth/quota rejections
    // (401/402/403/429). MCP is the last resort for user-added servers
    // only and stays single-shot: the user picked that server.
    if SEARCH_PROVIDERS.contains(&selected.server.trim()) {
        let order = search_provider_order(&selected.server);
        let (text, provider) = run_search_chain(&order, &SearchEndpoints::default(), query).await;
        return ToolOutcome {
            tool_provider: provider.into(),
            ..ToolOutcome::from_text(text)
        };
    }

    let text = mcp_web_search(&selected, query, cwd).await;
    ToolOutcome {
        tool_provider: format!("mcp:{}", selected.server.trim()),
        ..ToolOutcome::from_text(text)
    }
}

/// Bundled search providers in priority order (web_fetch parity).
const SEARCH_PROVIDERS: [&str; 4] = ["tinyfish", "parallel", "exa", "ollama"];

/// Per-provider search endpoints; injectable so tests run against
/// local servers. Defaults are the production endpoints.
#[derive(Clone, Debug)]
struct SearchEndpoints {
    tinyfish: String,
    parallel: String,
    exa: String,
    ollama: String,
}

impl Default for SearchEndpoints {
    fn default() -> Self {
        Self {
            tinyfish: TINYFISH_SEARCH_ENDPOINT.into(),
            parallel: PARALLEL_SEARCH_ENDPOINT.into(),
            exa: EXA_SEARCH_ENDPOINT.into(),
            ollama: WEB_SEARCH_ENDPOINT.into(),
        }
    }
}

/// The selected provider moves to the front of the bundled list.
/// Unknown/empty selections just get the base priority order.
fn search_provider_order(selected: &str) -> Vec<&'static str> {
    let selected = selected.trim();
    let matched = SEARCH_PROVIDERS.iter().find(|b| **b == selected).copied();
    let mut out: Vec<&'static str> = SEARCH_PROVIDERS
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
/// requires a key). Mirrors web_fetch's provider_available, except
/// ollama search uses the ollama-cloud key resolver.
fn search_provider_available(id: &str) -> bool {
    match id {
        "tinyfish" | "parallel" => true,
        "ollama" => !ollama_web_search_key().trim().is_empty(),
        "exa" => !crate::auth_keys::resolve_provider_key(id).trim().is_empty(),
        _ => false,
    }
}

async fn run_search_chain(
    order: &[&'static str],
    endpoints: &SearchEndpoints,
    query: &str,
) -> (String, &'static str) {
    let tinyfish_ep = endpoints.tinyfish.clone();
    let parallel_ep = endpoints.parallel.clone();
    let exa_ep = endpoints.exa.clone();
    let ollama_ep = endpoints.ollama.clone();
    let query = query.to_string();
    match try_search_chain(order, move |id| {
        let (tinyfish_ep, parallel_ep, exa_ep, ollama_ep, query) = (
            tinyfish_ep.clone(),
            parallel_ep.clone(),
            exa_ep.clone(),
            ollama_ep.clone(),
            query.clone(),
        );
        async move {
            match id {
                "tinyfish" => {
                    let client = match provider_client() {
                        Ok(c) => c,
                        Err(e) => return Err(e),
                    };
                    tinyfish_search_result(
                        &query,
                        &tinyfish_ep,
                        &client,
                        &crate::auth_keys::resolve_provider_key("tinyfish"),
                    )
                    .await
                }
                "parallel" => {
                    parallel_search_result(
                        &parallel_ep,
                        &query,
                        &crate::auth_keys::resolve_provider_key("parallel"),
                    )
                    .await
                }
                "exa" => {
                    exa_search_result(
                        &exa_ep,
                        &query,
                        &crate::auth_keys::resolve_provider_key("exa"),
                    )
                    .await
                }
                "ollama" => {
                    let client = match provider_client() {
                        Ok(c) => c,
                        Err(e) => return Err(e),
                    };
                    ollama_search_result(&ollama_ep, &query, &client, &ollama_web_search_key())
                        .await
                }
                _ => Err(SearchError::Transport(format!("unknown provider {id}"))),
            }
        }
    })
    .await
    {
        ChainResult::Success(results, provider) => (results, provider),
        ChainResult::BadRequest(msg) | ChainResult::Transport(msg) => (
            format!("search error: {msg}"),
            "",
        ),
        ChainResult::Exhausted(fallbacks) => {
            if fallbacks.is_empty() {
                ("search error: no search provider configured".to_string(), "")
            } else {
                (
                    format!(
                        "search error: all search providers exhausted: {}",
                        fallbacks.join("; ")
                    ),
                    "",
                )
            }
        }
    }
}

/// MCP-routed search: bundled profiles build an HTTP-server override,
/// everything else resolves through the user's MCP configs. Results
/// are normalized into the search error / no-results vocabulary.
async fn mcp_web_search(
    selected: &atom_core::config::WebSearchConfig,
    query: &str,
    cwd: &std::path::Path,
) -> String {
    let profile = atom_core::config::bundled_web_search_profile(&selected.server);
    let (tool, args, override_config) = if let Some(profile) = profile {
        let mut headers = std::collections::BTreeMap::new();
        let key = crate::auth_keys::resolve_provider_key(&profile.id);
        if let Some((name, value)) = profile_auth_header(&profile.id, &key) {
            headers.insert(name, value);
        }
        let args = if profile.id == "parallel" {
            serde_json::json!({"objective": query, "search_queries": [query]})
        } else {
            serde_json::json!({profile.query_argument: query})
        };
        (
            if selected.tool.trim().is_empty() {
                profile.tool
            } else {
                selected.tool.clone()
            },
            args,
            Some(crate::mcp::MCPServerConfig {
                url: profile.url,
                headers,
                typ: "http".into(),
                ..Default::default()
            }),
        )
    } else {
        (
            selected.tool.clone(),
            serde_json::json!({"query": query}),
            None,
        )
    };

    let result =
        crate::mcp::execute_mcp_selection(&selected.server, &tool, args, cwd, override_config)
            .await;
    if let Some(error) = result.strip_prefix("error: ") {
        format!("search error: {error}")
    } else if result.trim().is_empty() {
        "no results found".into()
    } else {
        result
    }
}

/// Per-provider auth header for bundled web-search profiles: parallel
/// uses a bearer token, exa and tinyfish each use their own api-key
/// header. Returns None when no key is resolved (TinyFish then falls
/// back to unauthenticated calls at stricter rate limits).
fn profile_auth_header(provider: &str, key: &str) -> Option<(String, String)> {
    if key.trim().is_empty() {
        return None;
    }
    Some(match provider {
        "exa" => ("x-api-key".to_string(), key.to_string()),
        "tinyfish" => ("X-API-Key".to_string(), key.to_string()),
        // parallel (and any future bearer-style provider).
        _ => ("Authorization".to_string(), format!("Bearer {key}")),
    })
}

// ---------------------------------------------------------------------------
// Provider chain (web_fetch parity): shared error type, status
// classification, and one direct-HTTP adapter per bundled provider.
// ---------------------------------------------------------------------------

const PARALLEL_SEARCH_ENDPOINT: &str = "https://api.parallel.ai/v1beta/search";
const EXA_SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";

/// How a provider attempt failed, and whether that failure should
/// trigger a fallback or abort the chain. Same policy as web_fetch:
/// 401/402/403/429 walk down, 400/404/410/422 abort as likely our bug,
/// network/5xx abort as flakiness not worth hiding.
#[derive(Debug)]
enum SearchError {
    AuthOrQuota(String),
    BadRequest(String),
    Transport(String),
}

impl SearchError {
    /// Prepend the provider id so chain diagnostics read
    /// `tinyfish: HTTP 401: ...` without double prefixes.
    fn provider(self, provider: &str) -> Self {
        match self {
            SearchError::AuthOrQuota(m) => SearchError::AuthOrQuota(format!("{provider}: {m}")),
            SearchError::BadRequest(m) => SearchError::BadRequest(format!("{provider}: {m}")),
            SearchError::Transport(m) => SearchError::Transport(format!("{provider}: {m}")),
        }
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::AuthOrQuota(m)
            | SearchError::BadRequest(m)
            | SearchError::Transport(m) => write!(f, "{m}"),
        }
    }
}

fn classify_status(status: reqwest::StatusCode, body: &[u8]) -> Result<(), SearchError> {
    let code = status.as_u16();
    let preview = String::from_utf8_lossy(&body[..body.len().min(200)])
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    match code {
        200..=299 => Ok(()),
        400 | 404 | 410 | 422 => Err(SearchError::BadRequest(format!("HTTP {code}: {preview}"))),
        401 | 402 | 403 | 429 => Err(SearchError::AuthOrQuota(format!("HTTP {code}: {preview}"))),
        _ => Err(SearchError::Transport(format!("HTTP {code}"))),
    }
}

fn provider_client() -> Result<reqwest::Client, SearchError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| SearchError::Transport(format!("build client: {e}")))
}

/// Outcome of walking the provider chain.
enum ChainResult {
    Success(String, &'static str),
    BadRequest(String),
    Transport(String),
    /// Every available provider rejected us with AuthOrQuota.
    Exhausted(Vec<String>),
}

/// Try each available provider in `order`, walking down on
/// AuthOrQuota errors only. BadRequest and Transport abort the chain
/// immediately.
async fn try_search_chain<F, Fut>(order: &[&'static str], mut attempt: F) -> ChainResult
where
    F: FnMut(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<String, SearchError>>,
{
    let mut fallbacks: Vec<String> = Vec::new();
    for id in order {
        if !search_provider_available(id) {
            continue;
        }
        match attempt(id).await {
            Ok(results) => return ChainResult::Success(results, id),
            Err(SearchError::AuthOrQuota(msg)) => {
                eprintln!("websearch: {id} -> {msg}; falling back");
                fallbacks.push(msg);
            }
            Err(SearchError::BadRequest(msg)) => return ChainResult::BadRequest(msg),
            Err(SearchError::Transport(msg)) => return ChainResult::Transport(msg),
        }
    }
    ChainResult::Exhausted(fallbacks)
}

/// One normalized search hit; every adapter maps its response shape
/// onto this so the model sees one output format for all providers.
struct SearchHit {
    title: String,
    url: String,
    body: String,
}

/// Standard output shape, shared with the TinyFish/Ollama adapters and
/// the MCP path: `N. title\n   url\n   body` blocks.
fn format_search_results(hits: &[SearchHit]) -> String {
    let mut sb = String::new();
    for (i, r) in hits.iter().enumerate() {
        sb.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            r.title,
            r.url,
            r.body
        ));
    }
    if sb.is_empty() {
        return "no results found".to_string();
    }
    sb.trim().to_string()
}

/// Shared adapter plumbing for POST-JSON search providers: send a
/// prepared request, classify the HTTP status, parse the JSON body,
/// and hand control to `extract` to pull hits out of the
/// provider-specific shape.
async fn search_post_json(
    req: reqwest::RequestBuilder,
    extract: impl Fn(&serde_json::Value) -> Result<Vec<SearchHit>, SearchError>,
) -> Result<String, SearchError> {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Err(SearchError::Transport(format!("{e}"))),
    };
    let status = resp.status();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Err(SearchError::Transport(format!("read body: {e}"))),
    };
    classify_status(status, &bytes)?;
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| SearchError::BadRequest(format!("{e}")))?;
    let hits = extract(&parsed)?;
    Ok(format_search_results(&hits))
}

/// Defensive hit extraction for loosely-modeled providers: probe the
/// given container field names for the result array (in order), and
/// read each entry's title/url/body tolerantly (body falls back
/// through text > content > snippet > excerpts[]). A present-but-empty
/// array is a genuine empty result; a missing one is an unrecognized
/// shape.
fn extract_search_hits(
    parsed: &serde_json::Value,
    containers: &[&str],
) -> Result<Vec<SearchHit>, SearchError> {
    let field = |entry: &serde_json::Value, k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    for container in containers {
        if let Some(arr) = parsed.get(*container).and_then(|v| v.as_array()) {
            let hits = arr
                .iter()
                .map(|entry| {
                    let body = ["text", "content", "snippet"]
                        .iter()
                        .find_map(|k| {
                            let v = entry.get(*k).and_then(|v| v.as_str());
                            v.filter(|s| !s.trim().is_empty())
                        })
                        .map(|s| s.to_string())
                        .or_else(|| {
                            let joined = entry
                                .get("excerpts")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|e| e.as_str())
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                })
                                .unwrap_or_default();
                            (!joined.trim().is_empty()).then_some(joined)
                        })
                        .unwrap_or_default();
                    SearchHit {
                        title: field(entry, "title"),
                        url: field(entry, "url"),
                        body,
                    }
                })
                .collect();
            return Ok(hits);
        }
    }
    Err(SearchError::Transport(format!(
        "unrecognized response shape: {}",
        truncate_for_error(parsed)
    )))
}

fn truncate_for_error(v: &serde_json::Value) -> String {
    let s = v.to_string();
    s.chars().take(200).collect()
}

const TINYFISH_SEARCH_ENDPOINT: &str = "https://api.search.tinyfish.ai";

/// Fully-built request: URL with the urlencoded query and the optional
/// (header name, value) auth header.
type TinyfishRequest = (String, Option<(&'static str, String)>);

/// Pure request construction for the TinyFish REST adapter. The
/// endpoint is injectable so tests run against a local server.
fn tinyfish_request_parts(
    endpoint: &str,
    query: &str,
    key: &str,
) -> Result<TinyfishRequest, String> {
    let url = reqwest::Url::parse_with_params(endpoint, &[("query", query)])
        .map_err(|e| e.to_string())?
        .to_string();
    let auth = if key.trim().is_empty() {
        None
    } else {
        Some(("X-API-Key", key.trim().to_string()))
    };
    Ok((url, auth))
}

/// TinyFish Search REST adapter (no MCP endpoint). GET {endpoint}?query=...
/// with X-API-Key; results are normalized to the same search text format
/// the MCP path prints, so the model sees one shape for all providers.
pub async fn tinyfish_search(query: &str, key: &str) -> String {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => return format!("search error: tinyfish: {error}"),
    };
    tinyfish_search_with(query, TINYFISH_SEARCH_ENDPOINT, &client, key).await
}

pub async fn tinyfish_search_with(
    query: &str,
    endpoint: &str,
    client: &reqwest::Client,
    key: &str,
) -> String {
    match tinyfish_search_result(query, endpoint, client, key).await {
        Ok(results) => results,
        // The adapter prefixes errors with "tinyfish: "; the chain reuses it.
        Err(error) => format!("search error: {error}"),
    }
}

/// Endpoint-parameterized TinyFish core so the chain and tests can
/// point it at a local mock server.
async fn tinyfish_search_result(
    query: &str,
    endpoint: &str,
    client: &reqwest::Client,
    key: &str,
) -> Result<String, SearchError> {
    let (url, auth) = tinyfish_request_parts(endpoint, query, key)
        .map_err(|e| SearchError::BadRequest(format!("tinyfish: {e}")))?;
    let mut request = client.get(&url);
    if let Some((name, value)) = auth {
        request = request.header(name, value);
    }
    let resp = match request.send().await {
        Ok(r) => r,
        Err(e) => return Err(SearchError::Transport(format!("tinyfish: {e}"))),
    };
    let status = resp.status();
    let raw = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => return Err(SearchError::Transport(format!("tinyfish: read body: {e}"))),
    };
    if let Err(e) = classify_status(status, &raw) {
        return Err(e.provider("tinyfish"));
    }

    #[derive(serde::Deserialize, Default)]
    struct Result_ {
        #[serde(default)]
        title: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        snippet: String,
        #[serde(default)]
        content: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct Payload {
        #[serde(default)]
        results: Vec<Result_>,
    }
    let data: Payload = serde_json::from_slice(&raw)
        .map_err(|e| SearchError::BadRequest(format!("tinyfish: {e}")))?;

    let hits: Vec<SearchHit> = data
        .results
        .iter()
        .map(|r| SearchHit {
            title: r.title.clone(),
            url: r.url.clone(),
            body: if r.content.is_empty() {
                r.snippet.clone()
            } else {
                r.content.clone()
            },
        })
        .collect();
    Ok(format_search_results(&hits))
}

async fn parallel_search_result(
    endpoint: &str,
    query: &str,
    key: &str,
) -> Result<String, SearchError> {
    let client = provider_client()?;
    let mut req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"objective": query, "search_queries": [query]}));
    if !key.trim().is_empty() {
        req = req.header("Authorization", format!("Bearer {}", key.trim()));
    }
    search_post_json(req, |parsed| {
        extract_search_hits(parsed, &["search_results", "results"])
    })
    .await
    .map_err(|e| e.provider("parallel"))
}

async fn exa_search_result(endpoint: &str, query: &str, key: &str) -> Result<String, SearchError> {
    // Exa is paid; guard anyway in case of dispatch mistakes.
    if key.trim().is_empty() {
        return Err(SearchError::AuthOrQuota("exa: no key configured".into()));
    }
    let client = provider_client()?;
    let req = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("x-api-key", key.trim())
        .json(&serde_json::json!({
            "query": query,
            "numResults": 5,
            "contents": {"text": {"maxCharacters": 1000}}
        }));
    search_post_json(req, |parsed| extract_search_hits(parsed, &["results"]))
        .await
        .map_err(|e| e.provider("exa"))
}

pub async fn web_search_with(
    query: &str,
    endpoint: &str,
    client: &reqwest::Client,
    key: &str,
) -> String {
    match ollama_search_result(query, endpoint, client, key).await {
        Ok(results) => results,
        // The adapter prefixes errors with "ollama: "; the chain reuses it.
        Err(error) => format!("search error: {error}"),
    }
}

/// Endpoint-parameterized Ollama Cloud search core so the chain and
/// tests can point it at a local mock server.
async fn ollama_search_result(
    query: &str,
    endpoint: &str,
    client: &reqwest::Client,
    key: &str,
) -> Result<String, SearchError> {
    if key.trim().is_empty() {
        return Err(SearchError::AuthOrQuota(
            "ollama: web_search needs an Ollama API key from https://ollama.com/settings/keys (export OLLAMA_API_KEY or save it under providers/ollama-cloud). Local Ollama sign-in is enough for chat, not for search."
                .to_string(),
        ));
    }

    let body = serde_json::json!({"query": query, "max_results": 5});
    let resp = match client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {key}"))
        .body(body.to_string())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(SearchError::Transport(format!("ollama: {e}"))),
    };

    let status = resp.status();
    // Go reads at most 1MB of the body.
    let raw = match resp.bytes().await {
        Ok(b) => {
            let slice = if b.len() > 1 << 20 {
                &b[..1 << 20]
            } else {
                &b[..]
            };
            slice.to_vec()
        }
        Err(e) => return Err(SearchError::Transport(format!("ollama: read body: {e}"))),
    };
    if let Err(e) = classify_status(status, &raw) {
        return Err(e.provider("ollama"));
    }

    #[derive(serde::Deserialize, Default)]
    struct Result_ {
        #[serde(default)]
        title: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        content: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct Payload {
        #[serde(default)]
        results: Vec<Result_>,
    }
    let data: Payload = serde_json::from_slice(&raw)
        .map_err(|e| SearchError::BadRequest(format!("ollama: {e}")))?;

    let hits: Vec<SearchHit> = data
        .results
        .iter()
        .map(|r| SearchHit {
            title: r.title.clone(),
            url: r.url.clone(),
            body: r.content.clone(),
        })
        .collect();
    Ok(format_search_results(&hits))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal one-shot HTTP server for tests: runs `handler` with the
    /// raw request bytes and returns the canned response.
    async fn serve(response: &'static str, check: impl Fn(String) + Send + 'static) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            check(String::from_utf8_lossy(&buf[..n]).to_string());
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn http200(body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn surfaces_unauthorized_with_body() {
        let endpoint = serve(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"error\":\"Unauthorized\"}",
            |req| {
                assert!(
                    req.to_lowercase()
                        .contains("authorization: bearer test-key"),
                    "{req}"
                )
            },
        )
        .await;
        let got = web_search_with("Ness app startup", &endpoint, &client(), "test-key").await;
        assert!(
            got.contains("search error:") && got.contains("401") && got.contains("Unauthorized"),
            "{got}"
        );
        assert_ne!(got, "no results found");
    }

    #[tokio::test]
    async fn formats_results_and_request_body() {
        let endpoint = serve(
            http200(
                "{\"results\":[{\"title\":\"Ollama\",\"url\":\"https://ollama.com/\",\"content\":\"Run models.\"}]}",
            ),
            move |req| {
                let low = req.to_lowercase();
                assert!(low.contains("authorization: bearer provider-key"), "{req}");
                assert!(req.contains("\"query\":\"what is ollama?\""), "{req}");
                assert!(req.contains("\"max_results\":5"));
            },
        )
        .await;
        let got = web_search_with("what is ollama?", &endpoint, &client(), "provider-key").await;
        assert_eq!(got, "1. Ollama\n   https://ollama.com/\n   Run models.");
    }

    #[tokio::test]
    async fn empty_results_message() {
        let endpoint = serve(http200("{\"results\":[]}"), |_| {}).await;
        let got = web_search_with("zzzz", &endpoint, &client(), "k").await;
        assert_eq!(got, "no results found");
    }

    #[tokio::test]
    async fn missing_key_error_mentions_docs_url() {
        let got =
            web_search_with("anything", "http://127.0.0.1:9/hit-nothing", &client(), "").await;
        assert!(
            got.contains("search error:") && got.contains("Ollama API key"),
            "{got}"
        );
    }

    #[tokio::test]
    async fn custom_selection_errors_without_configured_server() {
        let mut config = atom_core::config::AtomConfig::default();
        config.web_search = Some(atom_core::config::WebSearchConfig {
            server: "not-configured".into(),
            tool: "search".into(),
        });
        let cwd = tempfile::tempdir().unwrap();
        let result = web_search_with_config("latest atom release", cwd.path(), &config).await;
        assert_eq!(
            result,
            "search error: unknown MCP server \"not-configured\""
        );
    }

    #[test]
    fn bundled_profiles_resolve_expected_tools() {
        let mut config = atom_core::config::AtomConfig::default();
        for (server, tool) in [
            ("tinyfish", "web_search"),
            ("parallel", "web_search"),
            ("exa", "web_search_exa"),
            ("ollama", "web_search"),
        ] {
            config.web_search = Some(atom_core::config::WebSearchConfig {
                server: server.into(),
                tool: String::new(),
            });
            assert_eq!(config.resolved_web_search().tool, tool);
        }
    }

    #[test]
    fn bundled_search_profiles_include_tinyfish() {
        let profiles = atom_core::config::bundled_web_search_profiles();
        assert!(profiles.iter().any(|p| p.id == "tinyfish"));
    }

    #[test]
    fn stored_ollama_cloud_key_is_used() {
        let auth = atom_core::providers::auth::AuthEntry {
            r#type: "api".into(),
            key: "stored-key".into(),
            ..Default::default()
        };

        assert_eq!(
            resolve_ollama_web_search_key(None, Some(auth.clone()), String::new()),
            "stored-key"
        );
        assert_eq!(
            resolve_ollama_web_search_key(Some("env-key".into()), Some(auth), "legacy-key".into(),),
            "env-key"
        );
    }

    #[test]
    fn tinyfish_search_builds_url_and_headers() {
        let (url, auth) =
            tinyfish_request_parts("https://api.search.tinyfish.ai", "rust async", "tf-key")
                .unwrap();
        assert_eq!(url, "https://api.search.tinyfish.ai/?query=rust+async");
        assert_eq!(auth, Some(("X-API-Key", "tf-key".to_string())));

        // No key -> no auth header (stricter unauthenticated rate limits).
        let (_, auth) =
            tinyfish_request_parts("https://api.search.tinyfish.ai", "query", "   ").unwrap();
        assert_eq!(auth, None);

        // Header style is per-provider, driven by resolve_provider_key.
        assert_eq!(
            profile_auth_header("tinyfish", "tf"),
            Some(("X-API-Key".into(), "tf".into()))
        );
        assert_eq!(
            profile_auth_header("exa", "ex"),
            Some(("x-api-key".into(), "ex".into()))
        );
    }

    #[tokio::test]
    async fn tinyfish_search_formats_results_and_headers() {
        let endpoint = serve(
            http200(
                "{\"query\":\"tinyfish\",\"total_results\":3,\"page\":0,\"results\":[{\"position\":1,\"site_name\":\"ollama.com\",\"title\":\"Ollama\",\"snippet\":\"Run models locally.\",\"url\":\"https://ollama.com/\"},{\"title\":\"Exa\",\"url\":\"https://exa.ai/\",\"snippet\":\"ignored, content wins\",\"content\":\"Search API.\"}]}",
            ),
            |req| {
                let low = req.to_lowercase();
                assert!(low.starts_with("get /?query="), "{req}");
                assert!(low.contains("x-api-key: tf-key"), "{req}");
            },
        )
        .await;
        let got =
            tinyfish_search_with("rust async & futures", &endpoint, &client(), "tf-key").await;
        assert!(
            got.contains("1. Ollama")
                && got.contains("https://ollama.com/")
                && got.contains("Search API."),
            "{got}"
        );
    }

    #[tokio::test]
    async fn tinyfish_search_surfaces_http_status() {
        let endpoint = serve(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            |_| {},
        )
        .await;
        let got = tinyfish_search_with("q", &endpoint, &client(), "k").await;
        // Errors carry the provider name and the classified status
        // ("HTTP 429: {body preview}"), matching the web_fetch adapters.
        assert!(
            got.starts_with("search error: tinyfish:") && got.contains("HTTP 429"),
            "{got}"
        );
    }

    fn status_response(status_line: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    // -----------------------------------------------------------------
    // Provider chain (tiered fallback, web_fetch parity).
    // -----------------------------------------------------------------

    #[test]
    fn search_provider_order_moves_selection_to_front() {
        assert_eq!(
            search_provider_order("tinyfish"),
            vec!["tinyfish", "parallel", "exa", "ollama"]
        );
        assert_eq!(
            search_provider_order("exa"),
            vec!["exa", "tinyfish", "parallel", "ollama"]
        );
        assert_eq!(
            search_provider_order("parallel"),
            vec!["parallel", "tinyfish", "exa", "ollama"]
        );
        assert_eq!(
            search_provider_order("ollama"),
            vec!["ollama", "tinyfish", "parallel", "exa"]
        );
        // Unknown/empty selection keeps the base priority order.
        assert_eq!(
            search_provider_order(""),
            vec!["tinyfish", "parallel", "exa", "ollama"]
        );
    }

    #[test]
    fn resolve_provider_key_used_for_parallel_and_exa() {
        // Serialized against any future env-mutating tests in this crate.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        std::env::set_var("PARALLEL_API_KEY", "parallel-env-key");
        std::env::set_var("EXA_API_KEY", "exa-env-key");
        assert_eq!(
            crate::auth_keys::resolve_provider_key("parallel"),
            "parallel-env-key"
        );
        assert_eq!(crate::auth_keys::resolve_provider_key("exa"), "exa-env-key");
        assert_eq!(
            profile_auth_header(
                "parallel",
                &crate::auth_keys::resolve_provider_key("parallel")
            ),
            Some(("Authorization".into(), "Bearer parallel-env-key".into()))
        );
        assert_eq!(
            profile_auth_header("exa", &crate::auth_keys::resolve_provider_key("exa")),
            Some(("x-api-key".into(), "exa-env-key".into()))
        );
        std::env::remove_var("PARALLEL_API_KEY");
        std::env::remove_var("EXA_API_KEY");
    }

    fn chain_endpoints(tinyfish: &str, parallel: &str) -> SearchEndpoints {
        SearchEndpoints {
            tinyfish: tinyfish.to_string(),
            parallel: parallel.to_string(),
            // Never reached when tinyfish and parallel are exercised;
            // point them at a closed port so any mistake fails fast.
            exa: "http://127.0.0.1:1".to_string(),
            ollama: "http://127.0.0.1:1".to_string(),
        }
    }

    /// The bug this file exists to prevent: TinyFish 401 must walk down
    /// the chain instead of stopping with `search error: tinyfish: HTTP 401`.
    #[tokio::test]
    async fn tinyfish_401_falls_back_to_parallel() {
        let tinyfish_endpoint = serve(
            status_response("HTTP/1.1 401 Unauthorized", r#"{"error":"invalid key"}"#),
            |_| {},
        )
        .await;
        let parallel_endpoint = serve(
            http200(
                r#"{"search_results":[{"title":"Parallel","url":"https://parallel.ai/","excerpts":["The search API.","Beta."]}]}"#,
            ),
            |req| {
                assert!(req.to_lowercase().starts_with("post /"), "{req}");
                assert!(
                    req.contains("\"objective\":\"what is ollama?\""),
                    "{req}"
                );
            },
        )
        .await;
        let got = run_search_chain(
            &search_provider_order("tinyfish"),
            &chain_endpoints(&tinyfish_endpoint, &parallel_endpoint),
            "what is ollama?",
        )
        .await;
        assert_eq!(
            got,
            "1. Parallel\n   https://parallel.ai/\n   The search API. Beta."
        );
    }

    #[tokio::test]
    async fn chain_exhausted_when_all_providers_auth_fail() {
        // Stub dispatch (web_fetch parity): a stubbed attempt lets the
        // test exercise the walk without depending on which keys exist
        // in the local auth store (exa/ollama availability varies).
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let calls_c = calls.clone();
        let result = try_search_chain(&["mystery", "tinyfish", "parallel"], move |id| {
            let calls = calls_c.clone();
            async move {
                calls.lock().unwrap().push(id);
                match id {
                    "tinyfish" => Err(SearchError::AuthOrQuota(
                        "tinyfish: HTTP 401: denied".into(),
                    )),
                    _ => Err(SearchError::AuthOrQuota(
                        "parallel: HTTP 429: slow down".into(),
                    )),
                }
            }
        })
        .await;
        // "mystery" is not a bundled provider and must be skipped
        // without calling the adapter.
        assert_eq!(*calls.lock().unwrap(), vec!["tinyfish", "parallel"]);
        let ChainResult::Exhausted(fallbacks) = result else {
            panic!("expected exhaustion");
        };
        assert_eq!(
            fallbacks,
            vec![
                "tinyfish: HTTP 401: denied".to_string(),
                "parallel: HTTP 429: slow down".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn parallel_search_adapter_parses_results_and_request() {
        let endpoint = serve(
            http200(
                r#"{"search_results":[{"title":"Parallel","url":"https://parallel.ai/","excerpts":["Search API body."]},{"title":"Second","url":"https://example.com/","snippet":"snippet wins when no excerpts"}]}"#,
            ),
            |req| {
                let low = req.to_lowercase();
                assert!(low.starts_with("post /"), "{req}");
                assert!(low.contains("authorization: bearer parallel-key"), "{req}");
                assert!(req.contains("\"objective\":\"rust async\""), "{req}");
                assert!(req.contains("\"search_queries\":[\"rust async\"]"), "{req}");
            },
        )
        .await;
        let got = parallel_search_result(&endpoint, "rust async", "parallel-key")
            .await
            .unwrap();
        assert!(
            got.contains("1. Parallel")
                && got.contains("https://parallel.ai/")
                && got.contains("Search API body.")
                && got.contains("2. Second")
                && got.contains("snippet wins when no excerpts"),
            "{got}"
        );
    }

    #[tokio::test]
    async fn parallel_search_adapter_maps_401_to_auth_error() {
        let endpoint = serve(
            status_response("HTTP/1.1 401 Unauthorized", r#"{"code":16}"#),
            |_| {},
        )
        .await;
        let err = parallel_search_result(&endpoint, "q", "bad-key")
            .await
            .unwrap_err();
        assert!(
            matches!(err, SearchError::AuthOrQuota(ref m) if m.starts_with("parallel: HTTP 401")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn exa_search_adapter_parses_results_and_request() {
        let endpoint = serve(
            http200(
                r#"{"requestId":"abc","results":[{"title":"Exa","url":"https://exa.ai/","text":"Semantic search body."}]}"#,
            ),
            |req| {
                let low = req.to_lowercase();
                assert!(low.starts_with("post /"), "{req}");
                assert!(low.contains("x-api-key: exa-key"), "{req}");
                assert!(req.contains("\"query\":\"what is exa?\""), "{req}");
                assert!(req.contains("\"numResults\":5"), "{req}");
            },
        )
        .await;
        let got = exa_search_result(&endpoint, "what is exa?", "exa-key")
            .await
            .unwrap();
        assert_eq!(got, "1. Exa\n   https://exa.ai/\n   Semantic search body.");
    }

    #[tokio::test]
    async fn exa_search_adapter_requires_key() {
        // No key -> AuthOrQuota without touching the network (nothing
        // is listening on port 1 anyway).
        let err = exa_search_result("http://127.0.0.1:1", "q", "")
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::AuthOrQuota(_)), "{err:?}");
    }
}
