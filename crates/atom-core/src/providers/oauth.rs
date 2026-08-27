//! OpenAI (ChatGPT) OAuth device-style sign-in flow, ported from
//! openai_oauth.go: PKCE authorize URL, local callback listener on
//! 127.0.0.1:1455, token exchange/refresh, and JWT claim extraction.

use crate::providers::auth::{auth_bearer, lookup_auth_entry, set_auth, AuthEntry};
use base64::Engine;
use once_cell::sync::Lazy;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_OAUTH_SCOPE: &str = "openid profile email offline_access";

pub const OPENAI_OAUTH_OK_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>atom</title></head><body><p>Signed in. You can close this window and return to atom.</p></body></html>";
pub const OPENAI_OAUTH_FAIL_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>atom</title></head><body><p>Sign-in failed. You can close this window and return to atom.</p></body></html>";

static AUTHORIZE_URL: Lazy<RwLock<String>> =
    Lazy::new(|| RwLock::new("https://auth.openai.com/oauth/authorize".to_string()));
static TOKEN_URL: Lazy<RwLock<String>> =
    Lazy::new(|| RwLock::new("https://auth.openai.com/oauth/token".to_string()));
pub const OPENAI_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const OPENAI_OAUTH_LISTEN_ADDR: &str = "127.0.0.1:1455";

/// Overridable for tests (Go swaps openaiTokenURL).
#[doc(hidden)]
pub fn set_token_url_for_test(url: &str) {
    *TOKEN_URL.write().unwrap() = url.to_string();
}

fn token_url() -> String {
    TOKEN_URL.read().unwrap().clone()
}

/// Browser opener hook so tests can capture the authorize URL instead
/// of launching a browser (Go's openaiOAuthOpenBrowser var).
pub type BrowserOpener = Box<dyn Fn(&str) + Send + Sync>;
static BROWSER_OPENER: Lazy<RwLock<Option<BrowserOpener>>> = Lazy::new(|| RwLock::new(None));

#[doc(hidden)]
pub fn set_browser_opener_for_test(f: Option<BrowserOpener>) {
    *BROWSER_OPENER.write().unwrap() = f;
}

/// Listen address hook so tests can bind an ephemeral port.
static LISTEN_ADDR: Lazy<RwLock<String>> =
    Lazy::new(|| RwLock::new(OPENAI_OAUTH_LISTEN_ADDR.to_string()));

#[doc(hidden)]
pub fn set_listen_addr_for_test(addr: &str) {
    *LISTEN_ADDR.write().unwrap() = addr.to_string();
}

#[derive(Debug, Clone)]
pub struct OpenAIOAuthFlow {
    pub state: String,
    pub verifier: String,
    pub url: String,
}

pub fn pkce_s256_challenge(verifier: &str) -> String {
    let sum = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sum)
}

pub fn random_raw_url_base64(n: usize) -> anyhow::Result<String> {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

pub fn new_openai_oauth_flow() -> anyhow::Result<OpenAIOAuthFlow> {
    let verifier = random_raw_url_base64(64)?;
    let state = random_raw_url_base64(32)?;
    let q = form_encode(&[
        ("response_type", "code"),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("redirect_uri", OPENAI_OAUTH_REDIRECT_URI),
        ("scope", OPENAI_OAUTH_SCOPE),
        ("code_challenge", &pkce_s256_challenge(&verifier)),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "atom"),
    ]);
    let url = format!("{}?{}", AUTHORIZE_URL.read().unwrap(), q);
    Ok(OpenAIOAuthFlow {
        state,
        verifier,
        url,
    })
}

// --- minimal application/x-www-form-urlencoded helpers ---

pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = hex {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct OpenAITokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub expires_in: f64,
}

async fn post_openai_token(form: &[(&str, &str)]) -> anyhow::Result<AuthEntry> {
    let client = super::retry::long_timeout_client();
    let body = form_encode(form);
    let resp = client
        .post(token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
        .body(body)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "openai token request: {}",
                super::oauth::redact_oauth_text(&e.to_string())
            )
        })?;
    let status = resp.status();
    let b = resp.bytes().await?;
    if status.as_u16() != 200 {
        anyhow::bail!(
            "openai token: {}: {}",
            status,
            redact_oauth_text(String::from_utf8_lossy(&b).trim())
        );
    }
    let tok: OpenAITokenResponse =
        serde_json::from_slice(&b).map_err(|_| anyhow::anyhow!("openai token: invalid json"))?;
    let fallback_refresh = form
        .iter()
        .find(|(k, _)| *k == "refresh_token")
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();
    auth_entry_from_openai_token(tok, fallback_refresh)
}

pub fn auth_entry_from_openai_token(
    tok: OpenAITokenResponse,
    fallback_refresh: String,
) -> anyhow::Result<AuthEntry> {
    if tok.access_token.is_empty() {
        anyhow::bail!("openai token: missing access_token");
    }
    let refresh = if !tok.refresh_token.is_empty() {
        tok.refresh_token.clone()
    } else {
        fallback_refresh
    };
    if refresh.is_empty() {
        anyhow::bail!("openai token: missing refresh_token");
    }
    if tok.expires_in <= 0.0 {
        anyhow::bail!("openai token: missing expires_in");
    }
    let jwt = if !tok.id_token.is_empty() {
        tok.id_token.clone()
    } else {
        tok.access_token.clone()
    };
    let (account_id, plan, email) = decode_openai_jwt_claims(&jwt);
    let mut meta = std::collections::BTreeMap::new();
    if !account_id.is_empty() {
        meta.insert("account_id".into(), account_id);
    }
    if !plan.is_empty() {
        meta.insert("plan".into(), plan);
    }
    if !email.is_empty() {
        meta.insert("email".into(), email);
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    Ok(AuthEntry {
        r#type: "oauth".into(),
        key: String::new(),
        access: tok.access_token,
        refresh,
        expires: now_ms + (tok.expires_in as i64) * 1000,
        metadata: if meta.is_empty() { None } else { Some(meta) },
    })
}

pub async fn exchange_openai_code(code: &str, verifier: &str) -> anyhow::Result<AuthEntry> {
    post_openai_token(&[
        ("grant_type", "authorization_code"),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", OPENAI_OAUTH_REDIRECT_URI),
    ])
    .await
}

pub async fn refresh_openai_token(refresh_token: &str) -> anyhow::Result<AuthEntry> {
    post_openai_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ("refresh_token", refresh_token),
        ("scope", OPENAI_OAUTH_SCOPE),
    ])
    .await
}

pub fn decode_openai_jwt_claims(tok: &str) -> (String, String, String) {
    let parts: Vec<&str> = tok.split('.').collect();
    if parts.len() < 2 {
        return (String::new(), String::new(), String::new());
    }
    let payload = match decode_jwt_payload(parts[1]) {
        Ok(p) => p,
        Err(_) => return (String::new(), String::new(), String::new()),
    };
    let claims: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(c) => c,
        Err(_) => return (String::new(), String::new(), String::new()),
    };
    let mut account_id = String::new();
    let mut plan = String::new();
    let mut email = String::new();
    if let Some(auth) = claims.get("https://api.openai.com/auth") {
        account_id = auth
            .get("chatgpt_account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        plan = auth
            .get("chatgpt_plan_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    if let Some(profile) = claims.get("https://api.openai.com/profile") {
        email = profile
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    if email.is_empty() {
        email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    (account_id, plan, email)
}

fn decode_jwt_payload(seg: &str) -> anyhow::Result<Vec<u8>> {
    if let Ok(b) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(seg) {
        return Ok(b);
    }
    Ok(base64::engine::general_purpose::URL_SAFE.decode(seg)?)
}

/// authExpiresAt normalizes expires (unix ms or seconds) to unix ms.
pub fn auth_expires_at(expires: i64) -> i64 {
    if expires > 1_000_000_000_000 {
        expires
    } else {
        expires * 1000
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn ensure_openai_auth() -> anyhow::Result<AuthEntry> {
    ensure_openai_auth_opt(false).await
}

pub async fn ensure_openai_auth_opt(force_refresh: bool) -> anyhow::Result<AuthEntry> {
    // Serializes concurrent refreshes (Go holds openaiAuthMu across the
    // whole call; a tokio Mutex is await-safe).
    static AUTH_MU: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
    let _g = AUTH_MU.lock().await;
    let e = lookup_auth_entry("openai")
        .ok_or_else(|| anyhow::anyhow!("no openai oauth credentials"))?;
    if e.r#type != "oauth" {
        anyhow::bail!("no openai oauth credentials");
    }
    if e.access.is_empty() && e.refresh.is_empty() {
        anyhow::bail!("openai oauth credentials are empty");
    }
    let soon = e.expires > 0 && auth_expires_at(e.expires) <= now_ms() + 120_000;
    if !force_refresh && !soon {
        return Ok(e);
    }
    if e.refresh.is_empty() {
        anyhow::bail!("openai oauth token expired");
    }
    let live = refresh_openai_token(&e.refresh).await?;
    set_auth("openai", live.clone())?;
    Ok(live)
}

fn equal_oauth_state(got: &str, want: &str) -> bool {
    // Constant-time compare (Go uses crypto/subtle).
    let (a, b) = (got.as_bytes(), want.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Outcome of the OAuth callback handler: the HTTP status/HTML to write
/// and the code-or-error to hand back to the waiting flow.
#[derive(Debug, Clone, PartialEq)]
pub struct CallbackOutcome {
    pub status: u16,
    pub html: String,
    pub result: Result<String, String>,
}

pub fn handle_openai_callback(raw_query: &str, expected_state: &str) -> CallbackOutcome {
    let q = parse_query(raw_query);
    let get = |k: &str| {
        q.iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let err_str = get("error");
    if !err_str.is_empty() {
        let desc = get("error_description");
        let msg = if !desc.is_empty() {
            format!("oauth error: {}: {}", err_str, redact_oauth_text(&desc))
        } else {
            format!("oauth error: {}", err_str)
        };
        return CallbackOutcome {
            status: 400,
            html: OPENAI_OAUTH_FAIL_HTML.to_string(),
            result: Err(msg),
        };
    }
    if !equal_oauth_state(&get("state"), expected_state) {
        return CallbackOutcome {
            status: 400,
            html: OPENAI_OAUTH_FAIL_HTML.to_string(),
            result: Err("oauth state mismatch".to_string()),
        };
    }
    let code = get("code");
    if code.is_empty() {
        return CallbackOutcome {
            status: 400,
            html: OPENAI_OAUTH_FAIL_HTML.to_string(),
            result: Err("oauth missing code".to_string()),
        };
    }
    CallbackOutcome {
        status: 200,
        html: OPENAI_OAUTH_OK_HTML.to_string(),
        result: Ok(code),
    }
}

pub fn open_browser_url(raw_url: &str) {
    if let Some(opener) = BROWSER_OPENER.read().unwrap().as_ref() {
        opener(raw_url);
        return;
    }
    let cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(raw_url).spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(raw_url).spawn()
    } else {
        eprintln!("Open this URL to sign in:\n{}", raw_url);
        return;
    };
    if cmd.is_err() {
        eprintln!("Open this URL to sign in:\n{}", raw_url);
    }
}

/// runOpenAIOAuth builds a fresh flow and runs it to completion.
pub async fn run_openai_oauth() -> anyhow::Result<AuthEntry> {
    let flow = new_openai_oauth_flow()?;
    run_openai_oauth_flow(&flow).await
}

/// runOpenAIOAuthFlow serves /auth/callback locally until the browser
/// redirect arrives, then exchanges the authorization code. Cancellation
/// is up to the caller (wrap in tokio::time::timeout).
pub async fn run_openai_oauth_flow(flow: &OpenAIOAuthFlow) -> anyhow::Result<AuthEntry> {
    let addr = LISTEN_ADDR.read().unwrap().clone();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackOutcome>(1);

    let expected_state = flow.state.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            let tx = tx.clone();
            let expected_state = expected_state.clone();
            tokio::spawn(async move {
                let outcome = serve_callback_conn(stream, &expected_state).await;
                if let Some(outcome) = outcome {
                    // First result wins (Go's sync.Once).
                    let _ = tx.send(outcome).await;
                }
            });
        }
    });

    open_browser_url(&flow.url);

    let res = rx.recv().await;
    server.abort();
    let outcome = res.ok_or_else(|| anyhow::anyhow!("oauth callback channel closed"))?;
    let code = outcome.result.map_err(anyhow::Error::msg)?;
    exchange_openai_code(&code, &flow.verifier).await
}

/// Reads one HTTP request from the connection, dispatches the callback
/// handler, writes the HTML response, and returns the outcome.
async fn serve_callback_conn(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> Option<CallbackOutcome> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];
    let head_end;
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            head_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let first_line = head.lines().next()?;
    let target = first_line.split_whitespace().nth(1)?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    let outcome = if path == "/auth/callback" {
        handle_openai_callback(query, expected_state)
    } else {
        CallbackOutcome {
            status: 404,
            html: "not found".to_string(),
            result: Err("not found".to_string()),
        }
    };
    let reason = match outcome.status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        outcome.status,
        reason,
        outcome.html.len(),
        outcome.html
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
    Some(outcome)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

static REDACT_BEARER_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)Bearer\s+\S+").unwrap());
static REDACT_TOKEN_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r#"(?i)(access_token|refresh_token)(=|":\s*")[^&"\s]+"#).unwrap()
});

pub fn redact_oauth_text(s: &str) -> String {
    let s = REDACT_BEARER_RE.replace_all(s, "Bearer [redacted]");
    let s = REDACT_TOKEN_RE.replace_all(&s, "${1}${2}[redacted]");
    s.into_owned()
}

/// Bearer token for the stored openai entry (helper mirroring Go call
/// sites that do authBearer("openai", e)).
pub fn openai_bearer(e: &AuthEntry) -> String {
    auth_bearer("openai", e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned_jwt(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{}.{}.sig", header, body)
    }

    #[test]
    fn pkce_s256_challenge_rfc7636() {
        assert_eq!(
            pkce_s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn new_flow_authorize_url() {
        let flow = new_openai_oauth_flow().unwrap();
        let qpos = flow.url.find('?').expect("query string");
        let q = parse_query(&flow.url[qpos + 1..]);
        let get = |k: &str| {
            q.iter()
                .find(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert!(flow
            .url
            .starts_with("https://auth.openai.com/oauth/authorize?"));
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("client_id"), OPENAI_OAUTH_CLIENT_ID);
        assert_eq!(get("redirect_uri"), OPENAI_OAUTH_REDIRECT_URI);
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("codex_cli_simplified_flow"), "true");
        assert_eq!(get("originator"), "atom");
        assert_eq!(get("id_token_add_organizations"), "true");
        assert_eq!(get("code_challenge"), pkce_s256_challenge(&flow.verifier));
        assert_eq!(get("scope"), OPENAI_OAUTH_SCOPE);
        assert!(!flow.state.is_empty() && !flow.verifier.is_empty());
    }

    #[test]
    fn decode_jwt_claims_profile_and_root_email() {
        let tok = unsigned_jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_1",
                "chatgpt_plan_type": "plus",
            },
            "https://api.openai.com/profile": {"email": "from-profile@example.com"},
            "email": "from-root@example.com",
        }));
        let (id, plan, email) = decode_openai_jwt_claims(&tok);
        assert_eq!(
            (id.as_str(), plan.as_str(), email.as_str()),
            ("acct_1", "plus", "from-profile@example.com")
        );

        let tok = unsigned_jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_2",
                "chatgpt_plan_type": "free",
            },
            "email": "root@example.com",
        }));
        let (id, plan, email) = decode_openai_jwt_claims(&tok);
        assert_eq!(
            (id.as_str(), plan.as_str(), email.as_str()),
            ("acct_2", "free", "root@example.com")
        );
    }

    #[tokio::test]
    async fn exchange_and_refresh_openai_token() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("oauth-exchange");

        let id_token = unsigned_jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_x",
                "chatgpt_plan_type": "plus",
            },
            "email": "u@example.com",
        }));
        let id2 = id_token.clone();
        let saw_grant = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sg = saw_grant.clone();
        let srv = crate::providers::StubServer::spawn(2, move |_i, req| {
            let grant = extract_form_value(req, "grant_type").unwrap_or_default();
            *sg.lock().unwrap() = grant.clone();
            assert_eq!(
                extract_form_value(req, "client_id").as_deref(),
                Some(OPENAI_OAUTH_CLIENT_ID)
            );
            match grant.as_str() {
                "authorization_code" => {
                    assert_eq!(extract_form_value(req, "code").as_deref(), Some("the-code"));
                    assert_eq!(
                        extract_form_value(req, "code_verifier").as_deref(),
                        Some("ver")
                    );
                    let body = format!("{{\"access_token\":\"access-1\",\"refresh_token\":\"refresh-1\",\"expires_in\":3600,\"id_token\":\"{}\"}}", id2);
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
                "refresh_token" => {
                    assert_eq!(
                        extract_form_value(req, "refresh_token").as_deref(),
                        Some("refresh-1")
                    );
                    let body = "{\"access_token\":\"access-2\",\"expires_in\":1800}";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
                _ => "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            }
        });
        set_token_url_for_test(&format!("http://{}/token", srv.addr));

        let e = exchange_openai_code("the-code", "ver").await.unwrap();
        assert_eq!(*saw_grant.lock().unwrap(), "authorization_code");
        assert_eq!(e.r#type, "oauth");
        assert_eq!(e.access, "access-1");
        assert_eq!(e.refresh, "refresh-1");
        let now = now_ms();
        assert!(e.expires >= now, "expires should look like unix ms");
        let meta = e.metadata.as_ref().unwrap();
        assert_eq!(meta.get("account_id").map(String::as_str), Some("acct_x"));
        assert_eq!(meta.get("plan").map(String::as_str), Some("plus"));
        assert_eq!(meta.get("email").map(String::as_str), Some("u@example.com"));

        let e2 = refresh_openai_token("refresh-1").await.unwrap();
        assert_eq!(*saw_grant.lock().unwrap(), "refresh_token");
        assert_eq!(e2.access, "access-2");
        // No refresh_token in the response keeps the old one via the
        // form fallback (Go test: "refresh should keep old refresh").
        assert_eq!(e2.refresh, "refresh-1");
    }

    /// Extracts a value from the x-www-form-urlencoded body of a recorded
    /// raw HTTP request (test helper).
    fn extract_form_value(req: &str, key: &str) -> Option<String> {
        let body = req.split("\r\n\r\n").nth(1)?;
        parse_query(body)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    #[tokio::test]
    async fn ensure_openai_auth_refreshes_soon_expiry() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("oauth-refresh");

        let srv = crate::providers::StubServer::spawn(1, move |_i, _req| {
            let body = "{\"access_token\":\"fresh\",\"refresh_token\":\"refresh-new\",\"expires_in\":3600}";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        set_token_url_for_test(&format!("http://{}/token", srv.addr));

        set_auth(
            "openai",
            AuthEntry {
                r#type: "oauth".into(),
                access: "stale".into(),
                refresh: "refresh-old".into(),
                expires: now_ms() + 30_000,
                ..Default::default()
            },
        )
        .unwrap();
        let e = ensure_openai_auth().await.unwrap();
        assert_eq!(e.access, "fresh");
        assert_eq!(
            crate::providers::auth::load_provider_key("openai").await,
            "fresh"
        );
    }

    #[tokio::test]
    async fn ensure_openai_auth_empty_errors() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("oauth-empty");

        set_auth(
            "openai",
            AuthEntry {
                r#type: "oauth".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(ensure_openai_auth().await.is_err());
    }

    #[test]
    fn callback_state_mismatch() {
        let o = handle_openai_callback("code=abc&state=other-state", "want-state");
        assert_eq!(o.status, 400);
        assert!(o.html.contains("Sign-in failed"));
        assert_eq!(o.result.unwrap_err(), "oauth state mismatch");
    }

    #[test]
    fn callback_success_does_not_interpolate_code() {
        let o = handle_openai_callback("code=thecode&state=st", "st");
        assert_eq!(o.status, 200);
        assert!(!o.html.contains("thecode"));
        assert_eq!(o.result.unwrap(), "thecode");
    }

    #[test]
    fn callback_error_param_redacts_description() {
        let o = handle_openai_callback(
            "error=bad&error_description=Bearer%20sk-secret&state=st",
            "st",
        );
        assert_eq!(o.status, 400);
        let msg = o.result.unwrap_err();
        assert!(msg.starts_with("oauth error: bad: "));
        assert!(!msg.contains("sk-secret"), "msg: {}", msg);
    }

    #[test]
    fn redact_oauth_text_covers_bearer_and_tokens() {
        let s = redact_oauth_text(
            "Bearer sk-secret access_token=tok123 refresh_token=ref456 \"access_token\":\"aaa\"",
        );
        assert!(!s.contains("sk-secret"), "{}", s);
        assert!(!s.contains("tok123"), "{}", s);
        assert!(!s.contains("ref456"), "{}", s);
        assert!(!s.contains("\"aaa\""), "{}", s);
    }

    #[tokio::test]
    async fn run_openai_flow_uses_stubbed_browser_and_times_out() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("oauth-run");

        let opened = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let opened2 = opened.clone();
        set_browser_opener_for_test(Some(Box::new(move |u| {
            *opened2.lock().unwrap() = u.to_string();
        })));
        set_listen_addr_for_test("127.0.0.1:0");

        let flow = new_openai_oauth_flow().unwrap();
        let res =
            tokio::time::timeout(Duration::from_millis(50), run_openai_oauth_flow(&flow)).await;
        assert!(res.is_err(), "expected timeout");
        assert!(
            !opened.lock().unwrap().is_empty(),
            "browser opener not called"
        );

        set_listen_addr_for_test(OPENAI_OAUTH_LISTEN_ADDR);
        set_browser_opener_for_test(None);
    }

    #[test]
    fn auth_expires_normalizes_seconds_and_ms() {
        assert_eq!(auth_expires_at(1234567890), 1234567890000);
        assert_eq!(auth_expires_at(1234567890123), 1234567890123);
    }

    #[test]
    fn percent_round_trip() {
        assert_eq!(percent_encode("a b&c=d"), "a+b%26c%3Dd");
        assert_eq!(percent_decode("a+b%26c%3Dd"), "a b&c=d");
    }
}
