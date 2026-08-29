//! OAuth 2.1 authorization for remote (streamable HTTP) MCP servers,
//! following the MCP authorization spec: protected-resource metadata
//! discovery (RFC 9728), authorization-server metadata (RFC 8414),
//! dynamic client registration (RFC 7591), and authorization-code + PKCE
//! with a local loopback callback. Tokens persist in the shared auth
//! store (auth.json) under "mcp-<server>" and refresh transparently.

use atom_core::providers::auth::{load_auth_store, set_auth, AuthEntry};
use atom_core::providers::oauth::{
    open_browser_url, pkce_s256_challenge, random_raw_url_base64, OPENAI_OAUTH_FAIL_HTML,
    OPENAI_OAUTH_OK_HTML,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::mcp::MCPServerConfig;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
/// Refresh when the access token has less than this much life left.
const EXPIRY_SLACK_MS: i64 = 120_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// auth.json key for an MCP server's tokens.
pub fn auth_key(server: &str) -> String {
    format!("mcp-{server}")
}

// ---------------------------------------------------------------------------
// Metadata discovery.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthorizationServerMetadata {
    #[serde(default)]
    pub authorization_endpoint: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Discovered {
    pub auth: AuthorizationServerMetadata,
    pub scopes: Vec<String>,
}

/// Path-aware well-known URL (RFC 8414/9728): https://host/a/b and
/// suffix "x" -> https://host/.well-known/x/a/b.
fn well_known(url: &str, suffix: &str) -> String {
    let (origin, path) = match url.split_once("://") {
        Some((scheme, rest)) => match rest.find('/') {
            Some(i) => (
                format!("{scheme}://{}", &rest[..i]),
                rest[i..].trim_end_matches('/').to_string(),
            ),
            None => (format!("{scheme}://{rest}"), String::new()),
        },
        None => return format!("{url}/.well-known/{suffix}"),
    };
    format!("{origin}/.well-known/{suffix}{path}")
}

async fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    resp.json().await.map_err(|e| format!("bad json: {e}"))
}

/// Discover authorization-server metadata for an MCP server URL:
/// protected-resource metadata first, falling back to the server origin
/// as the issuer.
pub async fn discover(server_url: &str) -> Result<Discovered, String> {
    let mut scopes = Vec::new();
    let mut issuers: Vec<String> = Vec::new();
    if let Ok(v) = get_json(&well_known(server_url, "oauth-protected-resource")).await {
        let prm: ProtectedResourceMetadata =
            serde_json::from_value(v).map_err(|e| format!("bad resource metadata: {e}"))?;
        scopes = prm.scopes_supported;
        issuers.extend(prm.authorization_servers);
    }
    if issuers.is_empty() {
        // Fall back to the server origin as its own authorization server.
        let origin = server_url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[..i]))
            .unwrap_or("");
        if origin.is_empty() {
            return Err("cannot determine issuer for OAuth discovery".into());
        }
        let scheme = server_url.split("://").next().unwrap_or("https");
        issuers.push(format!("{scheme}://{origin}"));
    }
    let mut last = String::from("no authorization server metadata found");
    for issuer in &issuers {
        for candidate in [
            well_known(issuer, "oauth-authorization-server"),
            well_known(issuer, "openid-configuration"),
        ] {
            match get_json(&candidate).await {
                Ok(v) => {
                    let auth: AuthorizationServerMetadata = serde_json::from_value(v)
                        .map_err(|e| format!("bad server metadata: {e}"))?;
                    if auth.authorization_endpoint.is_empty() || auth.token_endpoint.is_empty() {
                        last = format!("metadata at {candidate} lacks endpoints");
                        continue;
                    }
                    if issuer != server_url {
                        if let Some(s) = &auth.scopes_supported {
                            if scopes.is_empty() {
                                scopes = s.clone();
                            }
                        }
                    }
                    return Ok(Discovered {
                        auth,
                        scopes: scopes.clone(),
                    });
                }
                Err(e) => last = format!("{candidate}: {e}"),
            }
        }
    }
    Err(last)
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
}

async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<(String, String), String> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "client_name": "atom",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("client registration: HTTP {status}: {}", {
            let t: String = text.chars().take(300).collect();
            t
        }));
    }
    let reg: RegistrationResponse =
        serde_json::from_str(&text).map_err(|e| format!("bad registration response: {e}"))?;
    if reg.client_id.is_empty() {
        return Err("client registration: missing client_id".into());
    }
    Ok((reg.client_id, reg.client_secret))
}

// ---------------------------------------------------------------------------
// Token requests.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: serde_json::Value,
}

fn expires_ms(expires_in: &serde_json::Value) -> i64 {
    let secs = expires_in
        .as_f64()
        .or_else(|| expires_in.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0);
    if secs <= 0.0 {
        0
    } else {
        now_ms() + (secs * 1000.0) as i64
    }
}

fn token_entry(
    tok: TokenResponse,
    meta: std::collections::BTreeMap<String, String>,
) -> Result<AuthEntry, String> {
    if tok.access_token.is_empty() {
        return Err("token response missing access_token".into());
    }
    Ok(AuthEntry {
        r#type: "oauth".into(),
        access: tok.access_token,
        refresh: tok.refresh_token,
        expires: expires_ms(&tok.expires_in),
        metadata: Some(meta),
        ..Default::default()
    })
}

/// form_urlencoded percent encoding (space becomes %20 in this context;
/// + is reserved for a literal plus).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn post_token(endpoint: &str, form: &str) -> Result<TokenResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail: String = text.chars().take(300).collect();
        return Err(format!("token request: HTTP {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("bad token response: {e}"))
}

fn meta_of(entry: &AuthEntry) -> BTreeMap<String, String> {
    entry.metadata.clone().unwrap_or_default()
}

/// Refresh an expired token using metadata stored on the entry.
pub async fn refresh_entry(server: &str, entry: &AuthEntry) -> Result<AuthEntry, String> {
    let meta = meta_of(entry);
    let token_endpoint = meta
        .get("token_endpoint")
        .map(String::as_str)
        .unwrap_or_default();
    let client_id = meta
        .get("client_id")
        .map(String::as_str)
        .unwrap_or_default();
    if token_endpoint.is_empty() || entry.refresh.is_empty() {
        return Err("no refresh token or token endpoint".into());
    }
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(&entry.refresh),
        enc(client_id)
    );
    let tok = post_token(token_endpoint, &form).await?;
    let mut entry = token_entry(tok, meta)?;
    // Server may not return a new refresh token; keep the stored one.
    if entry.refresh.is_empty() {
        if let Some(prev) = load_auth_store().get(&auth_key(server)) {
            entry.refresh = prev.refresh.clone();
        }
    }
    set_auth(&auth_key(server), entry.clone()).map_err(|e| e.to_string())?;
    Ok(entry)
}

// ---------------------------------------------------------------------------
// Interactive login (authorization code + PKCE, loopback callback).
// ---------------------------------------------------------------------------

/// Runs the full browser sign-in for `server` and stores the tokens.
/// `static_client_id` (from mcp.json) skips dynamic registration.
pub async fn run_login(
    server: &str,
    server_url: &str,
    static_client_id: &str,
) -> Result<AuthEntry, String> {
    let disc = discover(server_url).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind loopback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let (client_id, client_secret) = if !static_client_id.is_empty() {
        (static_client_id.to_string(), String::new())
    } else {
        match &disc.auth.registration_endpoint {
            Some(ep) if !ep.is_empty() => register_client(ep, &redirect_uri).await?,
            _ => return Err("no registration endpoint and no client_id configured".into()),
        }
    };

    let verifier = random_raw_url_base64(64).map_err(|e| e.to_string())?;
    let state = random_raw_url_base64(32).map_err(|e| e.to_string())?;
    let mut q = format!(
        "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        enc(&client_id),
        enc(&redirect_uri),
        enc(&pkce_s256_challenge(&verifier)),
        enc(&state)
    );
    if !disc.scopes.is_empty() {
        q.push_str(&format!("&scope={}", enc(&disc.scopes.join(" "))));
    }
    let authorize_url = format!("{}?{}", disc.auth.authorization_endpoint, q);

    // Accept one loopback callback carrying the authorization code.
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let expected_state = state.clone();
    let accept = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        let head = loop {
            match stream.read(&mut chunk).await {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return,
            }
            if let Some(pos) = find_head_end(&buf) {
                break String::from_utf8_lossy(&buf[..pos]).into_owned();
            }
            if buf.len() > 64 * 1024 {
                return;
            }
        };
        let target = head.split_whitespace().nth(1).unwrap_or("");
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let get = |k: &str| {
            query.split('&').find_map(|p| {
                let (kk, vv) = p.split_once('=').unwrap_or((p, ""));
                (kk == k).then(|| vv.to_string())
            })
        };
        let result = if let Some(err) = get("error") {
            Err(format!("oauth error: {err}"))
        } else if get("state").as_deref() != Some(expected_state.as_str()) {
            Err("oauth state mismatch".into())
        } else {
            get("code")
                .filter(|c| !c.is_empty())
                .ok_or_else(|| "oauth missing code".to_string())
        };
        let ok = result.is_ok();
        let html = if ok {
            OPENAI_OAUTH_OK_HTML
        } else {
            OPENAI_OAUTH_FAIL_HTML
        };
        let body = format!(
            "HTTP/1.1 {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            if ok { "200 OK" } else { "400 Bad Request" },
            html.len(),
            html
        );
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.shutdown().await;
        let _ = tx.send(result);
    });

    open_browser_url(&authorize_url);

    let code = match tokio::time::timeout(LOGIN_TIMEOUT, rx).await {
        Ok(Ok(res)) => match res {
            Ok(c) => c,
            Err(e) => return Err(e),
        },
        Ok(Err(_)) => return Err("oauth callback channel closed".into()),
        Err(_) => {
            accept.abort();
            return Err("oauth sign-in timed out".into());
        }
    };
    accept.abort();

    let mut form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        enc(&code),
        enc(&redirect_uri),
        enc(&client_id),
        enc(&verifier)
    );
    if !client_secret.is_empty() {
        form.push_str(&format!("&client_secret={}", enc(&client_secret)));
    }
    let tok = post_token(&disc.auth.token_endpoint, &form).await?;
    let mut meta = std::collections::BTreeMap::new();
    meta.insert("token_endpoint".into(), disc.auth.token_endpoint.clone());
    meta.insert(
        "authorization_endpoint".into(),
        disc.auth.authorization_endpoint.clone(),
    );
    meta.insert("client_id".into(), client_id);
    if !client_secret.is_empty() {
        meta.insert("client_secret".into(), client_secret);
    }
    let entry = token_entry(tok, meta)?;
    set_auth(&auth_key(server), entry.clone()).map_err(|e| e.to_string())?;
    Ok(entry)
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Token resolution for connections.
// ---------------------------------------------------------------------------

/// Bearer token for `server`: cached access token, then refresh, then
/// (when `interactive`) a browser sign-in. None means no credentials
/// are available and the caller should proceed unauthenticated.
pub async fn bearer_token(
    server: &str,
    server_url: &str,
    static_client_id: &str,
    interactive: bool,
) -> Result<Option<String>, String> {
    let key = auth_key(server);
    if let Some(entry) = load_auth_store().get(&key) {
        if entry.r#type == "oauth" {
            let fresh = entry.expires <= 0 || entry.expires > now_ms() + EXPIRY_SLACK_MS;
            if fresh && !entry.access.is_empty() {
                return Ok(Some(entry.access.clone()));
            }
            if !entry.refresh.is_empty() {
                match refresh_entry(server, entry).await {
                    Ok(live) => return Ok(Some(live.access)),
                    // A failed refresh must not fall into an interactive
                    // browser flow during a normal turn unless allowed.
                    Err(e) => {
                        if !interactive {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
    if !interactive {
        return Ok(None);
    }
    let entry = run_login(server, server_url, static_client_id).await?;
    Ok(Some(entry.access))
}

/// Removes stored tokens for `server` (logout).
pub fn forget(server: &str) {
    let _ = atom_core::providers::auth::remove_auth(&auth_key(server));
}

/// Returns the human-readable auth state for an OAuth-configured MCP
/// server, or `None` when `cfg` does not opt into OAuth — callers
/// should then fall back to displaying the command or URL.
///
/// States returned for OAuth servers:
/// - `"auth required"` — no usable entry in the auth store
/// - `"auth expired"` — entry exists, but the access token is past
///   expiry and there is no refresh token to recover
/// - `"authenticated"` — fresh access token, or a refresh token that
///   can recover an expired access token
pub fn mcp_auth_display(cfg: &MCPServerConfig, server: &str) -> Option<String> {
    if !cfg.auth.eq_ignore_ascii_case("oauth") {
        return None;
    }
    let store = load_auth_store();
    let entry = match store.get(&auth_key(server)) {
        Some(e) => e,
        None => return Some("auth required".into()),
    };
    if entry.r#type != "oauth" {
        // Non-OAuth entry under an OAuth key is a misconfiguration; treat
        // it as not logged in so the user re-runs the flow.
        return Some("auth required".into());
    }
    if entry.access.is_empty() && entry.refresh.is_empty() {
        return Some("auth required".into());
    }
    let expired = entry.expires > 0 && entry.expires <= now_ms() + EXPIRY_SLACK_MS;
    if expired && entry.refresh.is_empty() {
        return Some("auth expired".into());
    }
    Some("authenticated".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn well_known_urls_are_path_aware() {
        assert_eq!(
            well_known("https://mcp.facebook.com/ads", "oauth-protected-resource"),
            "https://mcp.facebook.com/.well-known/oauth-protected-resource/ads"
        );
        assert_eq!(
            well_known("https://host.example", "oauth-authorization-server"),
            "https://host.example/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            well_known("https://host.example/mcp/", "oauth-protected-resource"),
            "https://host.example/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn form_encoding_escapes_reserved_bytes() {
        assert_eq!(enc("abc-._~123"), "abc-._~123");
        assert_eq!(enc("a b"), "a%20b");
        assert_eq!(enc("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn auth_keys_are_namespaced_per_server() {
        assert_eq!(auth_key("meta-ads"), "mcp-meta-ads");
    }

    #[test]
    fn mcp_auth_display_reflects_oauth_state() {
        use atom_core::providers::auth::{set_auth, AuthEntry};

        // Use an isolated key so the test cannot clobber a real entry.
        let server = "auth-display-isolated";
        let key = auth_key(server);
        let prior = atom_core::providers::auth::load_auth_store()
            .get(&key)
            .cloned();
        struct Restore {
            key: String,
            prior: Option<AuthEntry>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.prior.take() {
                    Some(e) => {
                        set_auth(&self.key, e).ok();
                    }
                    None => {
                        atom_core::providers::auth::remove_auth(&self.key).ok();
                    }
                }
            }
        }
        let _restore = Restore {
            key: key.clone(),
            prior,
        };

        let oauth_cfg = MCPServerConfig {
            auth: "oauth".into(),
            url: "https://mcp.facebook.com/ads".into(),
            ..Default::default()
        };
        let stdio_cfg = MCPServerConfig {
            command: "npx".into(),
            ..Default::default()
        };

        // No entry yet: auth required.
        atom_core::providers::auth::remove_auth(&key).ok();
        assert_eq!(
            mcp_auth_display(&oauth_cfg, server).as_deref(),
            Some("auth required")
        );

        // Empty oauth entry: auth required.
        let empty = AuthEntry {
            r#type: "oauth".into(),
            ..Default::default()
        };
        set_auth(&key, empty).unwrap();
        assert_eq!(
            mcp_auth_display(&oauth_cfg, server).as_deref(),
            Some("auth required")
        );

        // Fresh access + refresh: authenticated.
        let fresh = AuthEntry {
            r#type: "oauth".into(),
            access: "tok".into(),
            refresh: "ref".into(),
            ..Default::default()
        };
        set_auth(&key, fresh).unwrap();
        assert_eq!(
            mcp_auth_display(&oauth_cfg, server).as_deref(),
            Some("authenticated")
        );

        // Expired access, no refresh: auth expired.
        let stale = AuthEntry {
            r#type: "oauth".into(),
            access: "tok".into(),
            expires: 1,
            ..Default::default()
        };
        set_auth(&key, stale).unwrap();
        assert_eq!(
            mcp_auth_display(&oauth_cfg, server).as_deref(),
            Some("auth expired")
        );

        // Expired access, but refresh present: still authenticated
        // because the refresh path will recover.
        let refreshable = AuthEntry {
            r#type: "oauth".into(),
            access: "tok".into(),
            refresh: "ref".into(),
            expires: 1,
            ..Default::default()
        };
        set_auth(&key, refreshable).unwrap();
        assert_eq!(
            mcp_auth_display(&oauth_cfg, server).as_deref(),
            Some("authenticated")
        );

        // Non-OAuth config: helper returns None (caller shows URL).
        assert_eq!(mcp_auth_display(&stdio_cfg, server), None);
    }

    /// Fake authorization server exercising the full login: discovery,
    /// dynamic registration, PKCE code exchange. Uses the real auth
    /// store (env mutation is blocked in the test sandbox) and removes
    /// its key afterwards.
    #[ignore = "needs loopback TCP binds, which dev sandboxes deny"]
    #[tokio::test]
    async fn login_flow_against_fake_authorization_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                atom_core::providers::auth::remove_auth(&auth_key("fakeserver")).ok();
            }
        }
        let _cleanup = Cleanup;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured_token_form: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let form_for_server = captured_token_form.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 2048];
                let head_end = loop {
                    if stream.read(&mut chunk).await.unwrap_or(0) == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(p);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let reqline = head.lines().next().unwrap_or("").to_string();
                // Drain the request body (Content-Length) if any.
                let mut clen = 0usize;
                for line in head.lines() {
                    if let Some(v) = line
                        .to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                    {
                        clen = v;
                    }
                }
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < clen {
                    if stream.read(&mut chunk).await.unwrap_or(0) == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                let (status, payload) = if reqline.contains("oauth-protected-resource") {
                    ("404 Not Found", "{}".to_string())
                } else if reqline.contains("oauth-authorization-server")
                    || reqline.contains("openid-configuration")
                {
                    (
                        "200 OK",
                        serde_json::json!({
                            "issuer": format!("http://{addr}"),
                            "authorization_endpoint": "http://auth.example/authorize",
                            "token_endpoint": format!("http://{addr}/token"),
                            "registration_endpoint": format!("http://{addr}/register"),
                        })
                        .to_string(),
                    )
                } else if reqline.contains("POST /register") {
                    (
                        "200 OK",
                        serde_json::json!({
                            "client_id": "cli_123",
                            "client_secret": "sec_1",
                            "redirect_uris": [],
                        })
                        .to_string(),
                    )
                } else if reqline.contains("POST /token") {
                    *form_for_server.lock().unwrap() = String::from_utf8_lossy(&body).into_owned();
                    (
                        "200 OK",
                        serde_json::json!({
                            "access_token": "at_1",
                            "refresh_token": "rt_1",
                            "expires_in": 3600,
                        })
                        .to_string(),
                    )
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let server_url = format!("http://{addr}/mcp");
        let captured_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        atom_core::providers::oauth::set_browser_opener_for_test(Some(Box::new({
            let captured_url = captured_url.clone();
            move |u| *captured_url.lock().unwrap() = Some(u.to_string())
        })));

        let login_server_url = server_url.clone();
        let login =
            tokio::spawn(async move { run_login("fakeserver", &login_server_url, "").await });
        let authorize_url = loop {
            if let Some(u) = captured_url.lock().unwrap().as_ref() {
                break u.clone();
            }
            assert!(!login.is_finished(), "login finished before browser open");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // The authorize URL must carry PKCE and our loopback redirect.
        let param = |key: &str| {
            authorize_url
                .split_once('?')
                .unwrap_or(("", ""))
                .1
                .split('&')
                .find_map(|p| {
                    p.split_once('=')
                        .filter(|(k, _)| *k == key)
                        .map(|(_, v)| v.to_string())
                })
                .unwrap_or_default()
        };
        assert!(param("code_challenge").len() == 43, "{authorize_url}");
        assert_eq!(param("code_challenge_method"), "S256");
        let state = param("state");
        assert!(!state.is_empty());
        // redirect_uri is percent-encoded: http%3A%2F%2F127.0.0.1%3A<port>%2Fcallback
        let redirect = param("redirect_uri");
        let port: u16 = redirect
            .split("127.0.0.1%3A")
            .nth(1)
            .and_then(|r| r.split('%').next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("redirect_uri = {redirect}"));

        // Play the browser: hit the loopback callback with code + state.
        let cb = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=abc&state={state}"
        ))
        .await
        .unwrap();
        assert_eq!(cb.status(), 200);

        let entry = tokio::time::timeout(Duration::from_secs(10), login)
            .await
            .expect("login timed out")
            .expect("join")
            .expect("login ok");
        assert_eq!(entry.access, "at_1");
        assert_eq!(entry.refresh, "rt_1");
        assert!(entry.expires > now_ms());

        // Tokens persist in the shared auth store with refresh metadata.
        let stored = load_auth_store()
            .get(&auth_key("fakeserver"))
            .cloned()
            .unwrap();
        assert_eq!(stored.access, "at_1");
        let meta = stored.metadata.unwrap();
        assert_eq!(meta.get("client_id").map(String::as_str), Some("cli_123"));
        assert!(meta.contains_key("token_endpoint"));

        // The code exchange used the registered client and the callback code.
        let form = captured_token_form.lock().unwrap().clone();
        assert!(form.contains("grant_type=authorization_code"), "{form}");
        assert!(form.contains("code=abc"), "{form}");
        assert!(form.contains("client_id=cli_123"), "{form}");
        assert!(form.contains("code_verifier="), "{form}");

        atom_core::providers::oauth::set_browser_opener_for_test(None);
        server.abort();
    }
}
