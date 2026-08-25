//! web_search, ported from main.go webSearch/ollamaWebSearchKey.
//! Endpoint and client are injectable so tests run against a local
//! server (Go swapped package vars).

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

pub async fn web_search(query: &str, cwd: &std::path::Path) -> String {
    web_search_with_config(query, cwd, &atom_core::config::load()).await
}

async fn web_search_with_config(
    query: &str,
    cwd: &std::path::Path,
    config: &atom_core::config::AtomConfig,
) -> String {
    if query.trim().is_empty() {
        return "search error: query is empty".into();
    }
    let selected = config.resolved_web_search();
    if selected.server == "ollama" {
        return ollama_web_search(query).await;
    }

    let profile = atom_core::config::bundled_web_search_profile(&selected.server);
    let (tool, args, override_config) = if let Some(profile) = profile {
        let mut headers = std::collections::BTreeMap::new();
        match profile.id.as_str() {
            "parallel" => {
                if let Ok(key) = std::env::var("PARALLEL_API_KEY") {
                    if !key.trim().is_empty() {
                        headers.insert("Authorization".into(), format!("Bearer {key}"));
                    }
                }
            }
            "exa" => {
                if let Ok(key) = std::env::var("EXA_API_KEY") {
                    if !key.trim().is_empty() {
                        headers.insert("x-api-key".into(), key);
                    }
                }
            }
            _ => {}
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

async fn ollama_web_search(query: &str) -> String {
    let key = ollama_web_search_key();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => return format!("search error: {error}"),
    };
    web_search_with(query, WEB_SEARCH_ENDPOINT, &client, &key).await
}

pub async fn web_search_with(
    query: &str,
    endpoint: &str,
    client: &reqwest::Client,
    key: &str,
) -> String {
    if key.trim().is_empty() {
        return "search error: web_search needs an Ollama API key from https://ollama.com/settings/keys (export OLLAMA_API_KEY or save it under providers/ollama-cloud). Local Ollama sign-in is enough for chat, not for search.".to_string();
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
        Err(e) => return format!("search error: {e}"),
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
        Err(e) => return format!("search error: {e}"),
    };
    if status != reqwest::StatusCode::OK {
        let mut msg = String::from_utf8_lossy(&raw).trim().to_string();
        if msg.is_empty() {
            msg = format!(
                "{} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            );
        }
        if msg.len() > 500 {
            msg = msg[..500].to_string();
        }
        return format!("search error: HTTP {}: {}", status.as_u16(), msg);
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
    let data: Payload = match serde_json::from_slice(&raw) {
        Ok(p) => p,
        Err(e) => return format!("search error: {e}"),
    };

    let mut sb = String::new();
    for (i, r) in data.results.iter().enumerate() {
        sb.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            r.title,
            r.url,
            r.content
        ));
    }
    if sb.is_empty() {
        return "no results found".to_string();
    }
    sb.trim().to_string()
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
}
