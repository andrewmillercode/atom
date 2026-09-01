//! Model selection and provider management, ported from models.go plus
//! the SSE streaming client extracted from server.go's
//! streamModelToClient/main.go sseData. Providers are OpenAI-compatible
//! backends (Ollama Cloud, Ollama Local, OpenCode Go, ...) exposing
//! GET /v1/models to list available models.

use crate::types::{self, ChatRequest};
use futures::{FutureExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// provider is one model backend: its display name, OpenAI-compatible base
/// URL, and API key (empty when no key is needed, e.g. local Ollama).
/// reasoning_field names the streaming delta field that carries thinking
/// text — Ollama uses "reasoning", OpenCode Go uses "reasoning_content".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Provider {
    pub name: String,
    /// models.dev provider id; empty for ollama-local
    pub id: String,
    pub base_url: String,
    pub key: String,
    pub reasoning_field: String,
}

/// modelsResponse is the OpenAI-compatible GET /v1/models payload. Both
/// Ollama and OpenCode Go return this shape.
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    data: Vec<ModelsResponseEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponseEntry {
    #[serde(default)]
    id: String,
}

/// modelEntry is one selectable model in the selector, pairing a model ID
/// with the provider that hosts it.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub provider: Provider,
    pub model: String,
}

/// providerListEntry is one row in the /providers overlay.
#[derive(Debug, Clone, Default)]
pub struct ProviderListEntry {
    pub id: String,
    pub label: String,
    pub status: String,
    pub connected: bool,
    /// auth.json or legacy file; disconnectable with d
    pub stored: bool,
    /// Capabilities surfaced in the providers overlay's fixed badge
    /// column: "Models", "Web Search", "Web Fetch".
    pub caps: Vec<&'static str>,
}

/// webToolProviders are non-model backends that share the provider
/// list and auth store but expose no /v1/models: pure web search /
/// fetch services, keyed by their own API keys.
pub const WEB_TOOL_PROVIDERS: [(&str, &str); 3] = [
    ("tinyfish", "TinyFish"),
    ("parallel", "Parallel"),
    ("exa", "Exa"),
];

pub fn is_web_tool_provider(id: &str) -> bool {
    WEB_TOOL_PROVIDERS.iter().any(|(wid, _)| *wid == id)
}

/// providerCaps reports what a provider id can be used for, read off
/// the bundled capability tables: every models.dev provider lists
/// models; the web-tool providers (and ollama-cloud, whose API hosts
/// both) list search and fetch.
pub fn provider_caps(id: &str) -> Vec<&'static str> {
    let mut caps = Vec::new();
    if !is_web_tool_provider(id) {
        caps.push("Models");
    }
    if is_web_tool_provider(id) || matches!(id, "ollama-cloud" | "ollama") {
        caps.push("Web Search");
        caps.push("Web Fetch");
    }
    caps
}

/// providerByName returns the provider with the given display name, or
/// None if no configured provider has that name.
pub fn provider_by_name(providers: &[Provider], name: &str) -> Option<Provider> {
    providers.iter().find(|p| p.name == name).cloned()
}

/// findProviderForModel searches all providers' /v1/models lists for the
/// given model ID. It returns the first provider that lists it, or None.
/// All lookups run concurrently like Go's goroutines; results are scanned
/// in provider order.
pub async fn find_provider_for_model(providers: &[Provider], model: &str) -> Option<Provider> {
    let futures = providers.iter().map(|p| {
        let p = p.clone();
        let model = model.to_string();
        async move {
            let models = fetch_models(&p).await.unwrap_or_default();
            let found = models.contains(&model);
            (p, found)
        }
        .boxed()
    });
    let results = futures::future::join_all(futures).await;
    for (p, found) in results {
        if found {
            return Some(p);
        }
    }
    None
}

/// providerNameForURL returns a human-readable provider name for a base URL.
/// Used when -url is passed explicitly and the provider can't be auto-detected.
/// OpenCode Zen and Go are distinguished so a -url session saves the right
/// provider for the next launch's default-model lookup.
pub fn provider_name_for_url(url: &str) -> String {
    if url.contains("opencode.ai/zen/go") {
        return "opencode-go".into();
    }
    if url.contains("opencode.ai/zen") {
        return "opencode-zen".into();
    }
    if url.contains("opencode.ai") {
        return "opencode-go".into();
    }
    if url.contains("ollama.com") {
        return "ollama".into();
    }
    if url.contains("localhost") || url.contains("127.0.0.1") {
        return "ollama-local".into();
    }
    let want = url.trim_end_matches('/');
    for id in super::modelsdev::models_dev_provider_ids() {
        let api = super::modelsdev::models_dev_base_url(&id);
        if !api.is_empty() && api == want {
            return id;
        }
    }
    "custom".into()
}

/// reasoningFieldForURL returns the streaming delta field that carries
/// thinking text for the provider behind a base URL. Ollama endpoints
/// use "reasoning"; OpenCode Go and Zen use "reasoning_content".
pub fn reasoning_field_for_url(url: &str) -> String {
    if url.contains("opencode.ai") {
        "reasoning_content".into()
    } else {
        "reasoning".into()
    }
}

fn get_env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// ambientAwsRegion returns the ambient AWS region (AWS_REGION, then
/// AWS_DEFAULT_REGION) when set. Used by the bedrock dialect to pick the
/// ConverseStream endpoint host.
pub fn ambient_aws_region() -> Option<String> {
    for name in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        let v = get_env(name);
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// buildProviders discovers providers whose credentials are available.
/// OpenCode Zen's zero-cost public tier is always included; other remote
/// providers require credentials from env or auth storage.
pub async fn build_providers() -> Vec<Provider> {
    let mut providers: Vec<Provider> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Ollama Cloud (https://ollama.com/v1)
    let mut ollama_key = get_env("OLLAMA_API_KEY");
    if ollama_key.is_empty() {
        ollama_key = super::auth::load_provider_key("ollama-cloud").await;
    }
    if !ollama_key.is_empty() {
        providers.push(Provider {
            name: "ollama".into(),
            id: "ollama-cloud".into(),
            base_url: "https://ollama.com/v1".into(),
            key: ollama_key,
            reasoning_field: "reasoning".into(),
        });
        seen.insert("ollama-cloud".into());
        seen.insert("ollama".into());
    }

    // OpenCode Go (https://opencode.ai/zen/go/v1)
    let mut go_key = get_env("OPENCODE_GO_API_KEY");
    if go_key.is_empty() {
        go_key = super::auth::load_provider_key("opencode-go").await;
    }
    if !go_key.is_empty() {
        providers.push(Provider {
            name: "opencode-go".into(),
            id: "opencode-go".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            key: go_key,
            reasoning_field: "reasoning_content".into(),
        });
        seen.insert("opencode-go".into());
    }

    // OpenCode Zen (https://opencode.ai/zen/v1). OpenCode exposes catalog
    // models with zero input/output cost as a keyless public tier. Without
    // an account credential, OpenCode's own client authenticates as
    // `Bearer public` (packages/core/src/plugin/provider/opencode.ts), so
    // we do the same; fetch_models then limits discovery to the zero-cost
    // catalog entries.
    let mut zen_key = get_env("OPENCODE_ZEN_API_KEY");
    if zen_key.is_empty() {
        zen_key = super::auth::load_provider_key("opencode-zen").await;
    }
    if zen_key.is_empty() {
        zen_key = "public".into();
    }
    providers.push(Provider {
        name: "opencode-zen".into(),
        id: "opencode".into(),
        base_url: "https://opencode.ai/zen/v1".into(),
        key: zen_key,
        reasoning_field: "reasoning_content".into(),
    });
    seen.insert("opencode".into());
    seen.insert("opencode-zen".into());

    for id in super::modelsdev::models_dev_provider_ids() {
        if id == "ollama-local" || seen.contains(&id) {
            continue;
        }
        let p = super::modelsdev::models_dev_provider(&id).unwrap_or_default();
        if !super::modelsdev::is_addable_models_dev_provider(&id, &p) {
            continue;
        }
        let mut key = catalog_env_key(&id, &p);
        if key.is_empty() {
            key = super::auth::load_provider_key(&id).await;
        }
        if key.is_empty() {
            continue;
        }
        let base = super::modelsdev::models_dev_base_url(&id);
        providers.push(Provider {
            name: id.clone(),
            id: id.clone(),
            base_url: base.clone(),
            key,
            reasoning_field: reasoning_field_for_url(&base),
        });
        seen.insert(id);
    }

    providers
}

pub fn catalog_env_key(id: &str, p: &super::modelsdev::ModelsDevProvider) -> String {
    for name in &p.env {
        if name.is_empty() {
            continue;
        }
        // Bedrock's catalog env list includes AWS_ACCESS_KEY_ID /
        // AWS_SECRET_ACCESS_KEY, but those are SigV4 halves, not bearer
        // tokens: a bare access key id cannot authenticate anything.
        // Only the bearer token (or a stored provider key) counts.
        if super::modelsdev::provider_is_bedrock(id) && name != "AWS_BEARER_TOKEN_BEDROCK" {
            continue;
        }
        let v = get_env(name);
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

/// fetchModels lists the model IDs available from a provider's /v1/models
/// endpoint, falling back to the models.dev catalog. Returns an error if
/// neither source works.
pub async fn fetch_models(p: &Provider) -> anyhow::Result<Vec<String>> {
    if p.id == "openai" || p.name == "openai" {
        if let Some(e) = super::auth::lookup_auth_entry("openai") {
            if e.r#type == "oauth" {
                // ChatGPT OAuth tokens are not accepted by
                // api.openai.com/v1/models. Use the catalog list.
                let cat_id = super::modelsdev::provider_catalog_id(p);
                if let Some(fallback) = super::modelsdev::catalog_model_ids(&cat_id) {
                    if !fallback.is_empty() {
                        return Ok(fallback);
                    }
                }
                anyhow::bail!("openai oauth: catalog models unavailable");
            }
        }
    }
    let http = fetch_models_http(p).await;
    match http {
        Ok(ids) if !ids.is_empty() => {
            if p.id == "opencode" && (p.key.is_empty() || p.key == "public") {
                let free = super::modelsdev::catalog_free_model_ids("opencode")
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<HashSet<_>>();
                Ok(ids.into_iter().filter(|id| free.contains(id)).collect())
            } else {
                Ok(ids)
            }
        }
        other => {
            if p.name == "ollama-local" {
                return other;
            }
            let cat_id = super::modelsdev::provider_catalog_id(p);
            let fallback = if cat_id == "opencode" && (p.key.is_empty() || p.key == "public") {
                super::modelsdev::catalog_free_model_ids(&cat_id)
            } else {
                super::modelsdev::catalog_model_ids(&cat_id)
            };
            if let Some(fallback) = fallback {
                if !fallback.is_empty() {
                    return Ok(fallback);
                }
            }
            other
        }
    }
}

async fn fetch_models_http(p: &Provider) -> anyhow::Result<Vec<String>> {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    });
    let timeout = if p.name == "ollama-local" {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(5)
    };
    let mut req = CLIENT
        .get(format!("{}/models", p.base_url))
        .timeout(timeout);
    if !p.key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", p.key));
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status.as_u16() != 200 {
        anyhow::bail!("{}", status);
    }
    let mr: ModelsResponse = resp.json().await?;
    Ok(mr.data.into_iter().map(|m| m.id).collect())
}

pub fn builtin_env_key(id: &str) -> String {
    match id {
        "ollama" | "ollama-cloud" => get_env("OLLAMA_API_KEY"),
        "opencode-go" => get_env("OPENCODE_GO_API_KEY"),
        "opencode-zen" | "opencode" => get_env("OPENCODE_ZEN_API_KEY"),
        _ => String::new(),
    }
}

pub fn list_addable_providers() -> Vec<ProviderListEntry> {
    let store = super::auth::load_auth_store();
    let mut entries: Vec<ProviderListEntry> = Vec::new();
    for id in super::modelsdev::models_dev_provider_ids() {
        if id == "ollama-local" {
            continue;
        }
        let p = super::modelsdev::models_dev_provider(&id).unwrap_or_default();
        if !super::modelsdev::is_addable_models_dev_provider(&id, &p) {
            continue;
        }
        entries.push(make_provider_list_entry(&id, &p, &store));
    }
    // Web-tool providers (search/fetch only, no models) share the
    // list so their API keys are manageable in the same place.
    for (id, label) in WEB_TOOL_PROVIDERS {
        let mut e = ProviderListEntry {
            id: id.to_string(),
            label: label.to_string(),
            caps: provider_caps(id),
            ..Default::default()
        };
        fill_connection_status(&mut e, &store);
        entries.push(e);
    }
    entries.sort_by(|a, b| {
        if a.connected != b.connected {
            return b.connected.cmp(&a.connected);
        }
        let al = a.label.to_lowercase();
        let bl = b.label.to_lowercase();
        if al != bl {
            return al.cmp(&bl);
        }
        a.id.cmp(&b.id)
    });
    entries
}

/// makeProviderListEntry builds one overlay row for a models.dev
/// provider: display name, connection status, and capability badges.
fn make_provider_list_entry(
    id: &str,
    p: &super::modelsdev::ModelsDevProvider,
    store: &HashMap<String, super::auth::AuthEntry>,
) -> ProviderListEntry {
    let mut e = ProviderListEntry {
        id: id.to_string(),
        label: if p.name.is_empty() {
            id.to_string()
        } else {
            p.name.clone()
        },
        caps: provider_caps(id),
        ..Default::default()
    };
    fill_connection_status(&mut e, store);
    e
}

/// fillConnectionStatus sets connected/stored/status on an entry by
/// resolving the provider's credential: auth.json (including builtin
/// aliases like ollama-cloud <-> ollama), a legacy flat file, or an
/// env var from the models.dev catalog.
fn fill_connection_status(
    e: &mut ProviderListEntry,
    store: &HashMap<String, super::auth::AuthEntry>,
) {
    let id = e.id.as_str();
    let mut found: Option<super::auth::AuthEntry> = None;
    for k in super::auth::auth_ids_for(id) {
        if let Some(ae) = store.get(&k) {
            found = Some(ae.clone());
            break;
        }
    }
    if let Some(found) = found {
        let kind = if found.r#type.is_empty() {
            "api"
        } else {
            found.r#type.as_str()
        };
        e.connected = true;
        e.stored = true;
        e.status = format!("connected ({})", kind);
        return;
    }
    if !super::auth::legacy_provider_key(id).is_empty() {
        e.connected = true;
        e.stored = true;
        e.status = "connected (api)".into();
        return;
    }
    if is_web_tool_provider(id) {
        // Web-tool providers have no models.dev catalog entry, so no
        // env-var list and no keyless public tier. The bundled
        // hosted-MCP routes still work keylessly at the tool level,
        // but the provider row reflects the keyed REST tier only.
        e.status = "not connected".into();
        return;
    }
    let p = super::modelsdev::models_dev_provider(id).unwrap_or_default();
    if !catalog_env_key(id, &p).is_empty() || !builtin_env_key(id).is_empty() {
        e.connected = true;
        e.stored = false;
        e.status = "connected (api)".into();
        return;
    }
    if id == "opencode" {
        e.connected = true;
        e.stored = false;
        e.status = "connected (public)".into();
        return;
    }
    e.status = "not connected".into();
}

pub fn filter_provider_entries(
    entries: &[ProviderListEntry],
    query: &str,
) -> Vec<ProviderListEntry> {
    let q = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            entry_matches_query(&q, &e.id.to_lowercase(), &e.label.to_lowercase())
                // Capability badges are searchable too, so "web fetch",
                // "web search", and "models" find the right rows.
                || {
                    let caps = e.caps.join(" ").to_lowercase();
                    !caps.is_empty() && entry_matches_query(&q, "", &caps)
                }
        })
        .cloned()
        .collect()
}

/// filterEntries returns the entries matching the query. An empty query
/// returns all entries. Matching is case-insensitive and checks the model
/// ID, the provider name, and the provider+model concatenation, so a query
/// like "ollamaminimax" finds "minimax-01" on provider "ollama". For
/// multi-word queries, every word must match somewhere in the entry, so
/// "minimax ollama" finds the same model.
pub fn filter_entries(entries: &[ModelEntry], query: &str) -> Vec<ModelEntry> {
    let q = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            entry_matches_query(&q, &e.model.to_lowercase(), &e.provider.name.to_lowercase())
        })
        .cloned()
        .collect()
}

/// entryMatchesQuery reports whether query q matches an entry described by
/// its lowercase model ID and lowercase provider name. Every space-separated
/// word in q must appear in the model ID, the provider name, or the
/// provider+model concatenation (which lets "ollamaminimax" match without
/// a separator).
pub fn entry_matches_query(q: &str, model: &str, provider: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let combined = format!("{}{}", provider, model);
    q.split_whitespace()
        .all(|word| model.contains(word) || provider.contains(word) || combined.contains(word))
}

/// sseData extracts the value of a data: SSE line, or None. Leading and
/// trailing whitespace on the line and after the colon is ignored, so
/// both "data: x" and "data:x" work; comment (":...") and field lines
/// ("event:", "id:") yield None.
pub fn sse_data(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("data:")?;
    Some(rest.trim())
}

// --- shared SSE line reader over a byte stream ---

/// Buffers partial lines from a byte stream and yields whole SSE lines,
/// mirroring bufio.Reader.ReadString('\n') semantics: the final
/// unterminated line is delivered before EOF, then a transport error
/// (if any) surfaces once.
pub(crate) struct SseLineReader<S> {
    buf: String,
    inner: S,
    finished: bool,
    pending_err: Option<anyhow::Error>,
}

impl<S> SseLineReader<S>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        SseLineReader {
            buf: String::new(),
            inner,
            finished: false,
            pending_err: None,
        }
    }
}

impl<S> futures::Stream for SseLineReader<S>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    type Item = anyhow::Result<String>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if let Some(pos) = this.buf.find('\n') {
                let line: String = this.buf.drain(..=pos).collect();
                return Poll::Ready(Some(Ok(line.trim_end_matches('\n').to_string())));
            }
            if this.finished {
                if !this.buf.is_empty() {
                    let rest = std::mem::take(&mut this.buf);
                    return Poll::Ready(Some(Ok(rest)));
                }
                if let Some(err) = this.pending_err.take() {
                    return Poll::Ready(Some(Err(err)));
                }
                return Poll::Ready(None);
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buf.push_str(&String::from_utf8_lossy(&bytes));
                }
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    this.pending_err = Some(anyhow::anyhow!(e));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Remove atom-internal fields from a serialized request body and rename
/// the reasoning key to match what the upstream provider expects.
/// DeepSeek / OpenCode Go use `reasoning_content`; Ollama / vLLM use
/// `reasoning`.
fn strip_internal_fields(body: &mut serde_json::Value, reasoning_field: &str) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        obj.remove("reasoning_signature");
        obj.remove("reasoning_ms");
        obj.remove("diff");
        obj.remove("provider");
        obj.remove("model");
        obj.remove("duration_ms");
        obj.remove("usage");
        if reasoning_field != "reasoning" {
            if let Some(r) = obj.remove("reasoning") {
                if r.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                    obj.insert(reasoning_field.into(), r);
                }
            }
        }
    }
}

/// SSE "data:" lines are decoded; empty payloads, comments, and [DONE]
/// terminate/skip per main.go's sseData + streamModelToClient semantics;
/// undecodable JSON lines are skipped.
pub async fn stream_chat(
    base_url: &str,
    api_key: &str,
    req: ChatRequest,
    reasoning_field: &str,
) -> anyhow::Result<impl futures::Stream<Item = anyhow::Result<types::StreamChunk>>> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body_value = serde_json::to_value(&req)?;
    strip_internal_fields(&mut body_value, reasoning_field);
    let body = serde_json::to_vec(&body_value)?;
    let resp = super::retry::do_http_with_retry(|| {
        let mut builder = super::retry::long_timeout_client()
            .post(url.clone())
            .header("Content-Type", "application/json");
        if !api_key.is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", api_key));
        }
        builder.body(body.clone()).send()
    })
    .await?;
    let byte_stream = Box::pin(resp.bytes_stream());
    Ok(futures::stream::unfold(
        SseLineReader::new(byte_stream),
        |mut reader| async move {
            loop {
                let line = match reader.next().await {
                    Some(Ok(l)) => l,
                    Some(Err(e)) => return Some((Err(e), reader)),
                    None => return None,
                };
                let trim = line.trim();
                if trim.is_empty() || trim.starts_with(':') {
                    continue;
                }
                let data = match sse_data(trim) {
                    Some(d) => d,
                    None => continue,
                };
                if data == "[DONE]" {
                    return None;
                }
                match serde_json::from_str::<types::StreamChunk>(data) {
                    Ok(chunk) => return Some((Ok(chunk), reader)),
                    Err(_) => continue,
                }
            }
        },
    ))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Shared test helpers for the providers modules: a global lock that
    //! serializes tests touching process-wide state (env vars, the
    //! models.dev catalog), an XDG_DATA_HOME isolator, and a minimal TCP
    //! HTTP stub server.

    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Opaque guard for [`test_lock`]. A newtype over the std guard:
    /// the lock is deliberately held across `.await`s (it serializes
    /// tests that mutate process-global state), which the std guard
    /// would flag as `clippy::await_holding_lock` — here that is the
    /// intended semantics, not a hazard.
    pub struct TestLockGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    pub fn test_lock() -> TestLockGuard {
        let g = match TEST_LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        TestLockGuard(g)
    }

    /// Sets XDG_DATA_HOME to a fresh temp directory for the duration of
    /// the test (Go settings_test.go isolateDataDir).
    pub struct DataDirGuard {
        prev: Option<std::ffi::OsString>,
        #[allow(dead_code)]
        path: PathBuf,
    }

    impl Drop for DataDirGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    pub fn isolate_data_dir(tag: &str) -> DataDirGuard {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("atom-core-test-{}-{}-{}", tag, nanos, c));
        std::fs::create_dir_all(&path).unwrap();
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &path);
        DataDirGuard { prev, path }
    }

    /// Sets an env var, restoring the previous value on drop.
    pub struct EnvVarGuard {
        name: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    pub fn set_env(name: &'static str, value: &str) -> EnvVarGuard {
        let prev = std::env::var_os(name);
        std::env::set_var(name, value);
        EnvVarGuard { name, prev }
    }

    pub fn remove_env(name: &'static str) -> EnvVarGuard {
        let prev = std::env::var_os(name);
        std::env::remove_var(name);
        EnvVarGuard { name, prev }
    }

    pub fn clear_builtin_provider_env() -> Vec<EnvVarGuard> {
        vec![
            remove_env("OLLAMA_API_KEY"),
            remove_env("OPENCODE_GO_API_KEY"),
            remove_env("OPENCODE_ZEN_API_KEY"),
        ]
    }

    /// Installs a models.dev catalog, restoring the prior global on drop
    /// (Go injectModelsDev).
    pub struct CatalogGuard;

    impl Drop for CatalogGuard {
        fn drop(&mut self) {
            super::super::modelsdev::set_models_dev_catalog_for_test(None);
        }
    }

    pub fn inject_models_dev(cat: crate::providers::modelsdev::ModelsDevCatalog) -> CatalogGuard {
        super::super::modelsdev::set_models_dev_catalog_for_test(Some(cat));
        CatalogGuard
    }

    /// Minimal raw-HTTP stub server serving a fixed number of connections.
    /// Each accepted connection is read to the end of its request body and
    /// answered with responder(index, raw_request).
    pub struct StubServer {
        pub addr: String,
    }

    impl StubServer {
        pub fn spawn<F>(conns: usize, responder: F) -> StubServer
        where
            F: Fn(usize, &str) -> String + Send + 'static,
        {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            std::thread::spawn(move || {
                for i in 0..conns {
                    let (mut stream, _) = match listener.accept() {
                        Ok(x) => x,
                        Err(_) => return,
                    };
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(p) = find(&buf, b"\r\n\r\n") {
                            break p;
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) => break usize::MAX,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => break usize::MAX,
                        }
                    };
                    if head_end != usize::MAX {
                        // Read the body per Content-Length so the client's
                        // write side completes cleanly.
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                        let clen: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let want = head_end + 4 + clen;
                        while buf.len() < want {
                            match stream.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                Err(_) => break,
                            }
                        }
                    }
                    let req_text = String::from_utf8_lossy(&buf).into_owned();
                    let resp = responder(i, &req_text);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });
            StubServer { addr }
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    pub fn catalog_one(
        id: &str,
        name: &str,
        api: &str,
        env: &[&str],
        models: &[&str],
    ) -> crate::providers::modelsdev::ModelsDevCatalog {
        use crate::providers::modelsdev::{ModelsDevModel, ModelsDevProvider};
        let mut cat = crate::providers::modelsdev::ModelsDevCatalog::new();
        cat.insert(
            id.to_string(),
            ModelsDevProvider {
                name: name.to_string(),
                api: api.to_string(),
                env: env.iter().map(|s| s.to_string()).collect(),
                models: models
                    .iter()
                    .map(|m| (m.to_string(), ModelsDevModel::default()))
                    .collect(),
                ..Default::default()
            },
        );
        cat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::modelsdev::ModelsDevCatalog;
    use crate::providers::testutil::*;
    use std::sync::{Arc, Mutex};

    fn me(provider: &str, model: &str) -> ModelEntry {
        ModelEntry {
            provider: Provider {
                name: provider.into(),
                ..Default::default()
            },
            model: model.into(),
        }
    }

    #[test]
    fn filter_entries_table() {
        let entries = vec![
            me("ollama", "minimax-01"),
            me("ollama", "llama3.2"),
            me("opencode-go", "claude-sonnet-4"),
        ];
        let cases: &[(&str, usize)] = &[
            ("", 3),
            ("minimax", 1),
            ("opencode", 1),
            ("llama3", 1),
            ("llama", 2), // model "llama3.2" + provider "ollama" contains "llama"
            ("ollamaminimax", 1), // provider+model concatenation, no separator
            ("OllamaMiniMax", 1), // case-insensitive concatenation
            ("ollama minimax", 1), // multi-word query, both words match
            ("minimax ollama", 1), // word order does not matter
            ("llama ollama", 2), // words can match different fields
            ("ollama", 2),
            ("nope", 0),
            ("ollama nope", 0),
        ];
        for (query, want) in cases {
            assert_eq!(
                filter_entries(&entries, query).len(),
                *want,
                "filterEntries({:?})",
                query
            );
        }
    }

    #[test]
    fn filter_provider_entries_table() {
        let entries = vec![
            ProviderListEntry {
                id: "openai".into(),
                label: "OpenAI".into(),
                ..Default::default()
            },
            ProviderListEntry {
                id: "openrouter".into(),
                label: "OpenRouter".into(),
                ..Default::default()
            },
            ProviderListEntry {
                id: "ollama-cloud".into(),
                label: "Ollama".into(),
                ..Default::default()
            },
        ];
        assert_eq!(filter_provider_entries(&entries, "open").len(), 2);
        let got = filter_provider_entries(&entries, "OLLAMA");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ollama-cloud");
        assert_eq!(filter_provider_entries(&entries, "").len(), 3);
    }

    #[test]
    fn provider_name_for_url_builtins_and_catalog() {
        assert_eq!(
            provider_name_for_url("https://opencode.ai/zen/go/v1"),
            "opencode-go"
        );
        assert_eq!(
            provider_name_for_url("https://opencode.ai/zen/v1"),
            "opencode-zen"
        );
        assert_eq!(
            provider_name_for_url("https://opencode.ai/other"),
            "opencode-go"
        );
        assert_eq!(provider_name_for_url("https://ollama.com/v1"), "ollama");
        assert_eq!(
            provider_name_for_url("http://localhost:11434/v1"),
            "ollama-local"
        );
        assert_eq!(
            provider_name_for_url("http://127.0.0.1:8080/v1"),
            "ollama-local"
        );

        let _g = test_lock();
        let cat: ModelsDevCatalog =
            catalog_one("openrouter", "", "https://openrouter.ai/api/v1", &[], &[]);
        let _cg = inject_models_dev(cat);
        assert_eq!(
            provider_name_for_url("https://openrouter.ai/api/v1"),
            "openrouter"
        );
        assert_eq!(provider_name_for_url("https://example.com/v1"), "custom");
    }

    #[test]
    fn reasoning_field_mapping() {
        assert_eq!(
            reasoning_field_for_url("https://opencode.ai/zen/go/v1"),
            "reasoning_content"
        );
        assert_eq!(
            reasoning_field_for_url("https://opencode.ai/zen/v1"),
            "reasoning_content"
        );
        assert_eq!(
            reasoning_field_for_url("https://ollama.com/v1"),
            "reasoning"
        );
        assert_eq!(
            reasoning_field_for_url("http://localhost:11434/v1"),
            "reasoning"
        );
    }

    #[tokio::test]
    async fn build_providers_from_auth_json() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-auth-json");
        let _e = clear_builtin_provider_env();
        let _c = inject_models_dev(catalog_one(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            &["OPENROUTER_API_KEY"],
            &["x"],
        ));
        let _er = remove_env("OPENROUTER_API_KEY");
        crate::providers::auth::set_auth(
            "openrouter",
            crate::providers::auth::AuthEntry {
                r#type: "api".into(),
                key: "sk-or".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let ps = build_providers().await;
        let p = provider_by_name(&ps, "openrouter").expect("missing openrouter");
        assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(p.key, "sk-or");
        assert_eq!(p.id, "openrouter");
        assert_eq!(p.reasoning_field, "reasoning");
    }

    #[tokio::test]
    async fn build_providers_oauth_bearer() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-oauth-bearer");
        let _e = clear_builtin_provider_env();
        let _c = inject_models_dev(catalog_one(
            "github-copilot",
            "",
            "https://api.githubcopilot.com",
            &[],
            &["gpt-4"],
        ));
        crate::providers::auth::set_auth(
            "github-copilot",
            crate::providers::auth::AuthEntry {
                r#type: "oauth".into(),
                access: "tok-access".into(),
                refresh: "tok-refresh".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let ps = build_providers().await;
        let p = provider_by_name(&ps, "github-copilot").expect("missing github-copilot");
        assert_eq!(p.key, "tok-access");
    }

    #[tokio::test]
    async fn build_providers_env_only_catalog() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-env-only");
        let _e = clear_builtin_provider_env();
        let _c = inject_models_dev(catalog_one(
            "openai",
            "",
            "https://api.openai.com/v1",
            &["OPENAI_API_KEY"],
            &["gpt-5"],
        ));
        let _eo = set_env("OPENAI_API_KEY", "sk-env");
        let ps = build_providers().await;
        let p = provider_by_name(&ps, "openai").expect("env-only openai missing");
        assert_eq!(p.key, "sk-env");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
    }

    #[tokio::test]
    async fn build_providers_does_not_duplicate_ollama_cloud() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-dup");
        let _e = clear_builtin_provider_env();
        let _c = inject_models_dev(catalog_one(
            "ollama-cloud",
            "",
            "https://ollama.com/v1",
            &[],
            &["m"],
        ));
        let _eo = set_env("OLLAMA_API_KEY", "ollama-sk");
        let ps = build_providers().await;
        let n = ps
            .iter()
            .filter(|p| p.name == "ollama" || p.name == "ollama-cloud" || p.id == "ollama-cloud")
            .count();
        assert_eq!(n, 1, "expected one ollama cloud provider, got {:?}", ps);
    }

    #[tokio::test]
    async fn build_providers_includes_public_zen_but_not_ollama_local() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-public-zen");
        let _e = clear_builtin_provider_env();
        crate::providers::modelsdev::set_models_dev_catalog_for_test(None);
        let ps = build_providers().await;
        let p = provider_by_name(&ps, "opencode-zen").expect("public Zen always available");
        assert_eq!(p.id, "opencode");
        assert_eq!(p.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(p.key, "public", "no key -> OpenCode's public bearer token");
        assert_eq!(p.reasoning_field, "reasoning_content");
        assert!(
            provider_by_name(&ps, "ollama-local").is_none(),
            "ollama-local must not be included automatically"
        );
    }

    #[tokio::test]
    async fn build_providers_openai_oauth_from_auth_json() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-openai-oauth");
        let _e = clear_builtin_provider_env();
        let _c = inject_models_dev(catalog_one(
            "openai",
            "",
            "https://api.openai.com/v1",
            &[],
            &["gpt-5"],
        ));
        crate::providers::auth::set_auth(
            "openai",
            crate::providers::auth::AuthEntry {
                r#type: "oauth".into(),
                access: "tok".into(),
                refresh: "r".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let ps = build_providers().await;
        let p = provider_by_name(&ps, "openai").expect("missing openai");
        assert_eq!(p.key, "tok");
        assert_eq!(p.id, "openai");
    }

    #[tokio::test]
    async fn fetch_models_catalog_fallback_on_http_error() {
        let _g = test_lock();
        let srv = StubServer::spawn(1, |_i, _req| {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        });
        let _c = inject_models_dev(catalog_one("openrouter", "", &srv.addr, &[], &["or-model"]));
        let p = Provider {
            name: "openrouter".into(),
            id: "openrouter".into(),
            base_url: format!("http://{}/v1", srv.addr),
            key: "k".into(),
            ..Default::default()
        };
        let ids = fetch_models(&p).await.unwrap();
        assert_eq!(ids, vec!["or-model".to_string()]);
    }

    #[tokio::test]
    async fn fetch_models_empty_http_uses_catalog() {
        let _g = test_lock();
        // Isolate so a real ~/.local/share/atom/auth.json can't influence
        // the openai oauth branch of fetch_models.
        let _d = isolate_data_dir("prov-empty-http");
        let body = r#"{"data":[]}"#;
        let srv = StubServer::spawn(1, move |_i, _req| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        let _c = inject_models_dev(catalog_one("openai", "", &srv.addr, &[], &["gpt-5"]));
        let p = Provider {
            name: "openai".into(),
            id: "openai".into(),
            base_url: format!("http://{}/v1", srv.addr),
            ..Default::default()
        };
        let ids = fetch_models(&p).await.unwrap();
        assert_eq!(ids, vec!["gpt-5".to_string()]);
    }

    #[tokio::test]
    async fn fetch_models_lists_http_ids_with_bearer() {
        let _g = test_lock();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let srv = StubServer::spawn(1, move |_i, req| {
            *seen2.lock().unwrap() = req.to_string();
            let body = r#"{"data":[{"id":"m-a"},{"id":"m-b"}]}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        crate::providers::modelsdev::set_models_dev_catalog_for_test(None);
        let p = Provider {
            name: "custom".into(),
            id: "custom".into(),
            base_url: format!("http://{}/v1", srv.addr),
            key: "sk-x".into(),
            ..Default::default()
        };
        let ids = fetch_models(&p).await.unwrap();
        assert_eq!(ids, vec!["m-a".to_string(), "m-b".to_string()]);
        let reqtext = seen.lock().unwrap().to_lowercase();
        assert!(reqtext.contains("authorization: bearer sk-x"));
    }

    #[tokio::test]
    async fn fetch_models_public_zen_lists_zero_cost_models_with_public_bearer() {
        let _g = test_lock();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let srv = StubServer::spawn(1, move |_i, req| {
            *seen2.lock().unwrap() = req.to_string();
            let body = r#"{"data":[{"id":"free-model"},{"id":"paid-model"}]}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        use crate::providers::modelsdev::{ModelsDevCost, ModelsDevModel, ModelsDevProvider};
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "opencode".into(),
            ModelsDevProvider {
                models: HashMap::from([
                    (
                        "free-model".into(),
                        ModelsDevModel {
                            cost: Some(ModelsDevCost::default()),
                            ..Default::default()
                        },
                    ),
                    (
                        "paid-model".into(),
                        ModelsDevModel {
                            cost: Some(ModelsDevCost {
                                input: 1.0,
                                output: 2.0,
                            }),
                            ..Default::default()
                        },
                    ),
                ]),
                ..Default::default()
            },
        );
        let _c = inject_models_dev(cat);
        let p = Provider {
            name: "opencode-zen".into(),
            id: "opencode".into(),
            base_url: format!("http://{}/v1", srv.addr),
            key: "public".into(),
            ..Default::default()
        };
        assert_eq!(fetch_models(&p).await.unwrap(), vec!["free-model"]);
        let reqtext = seen.lock().unwrap().to_lowercase();
        assert!(
            reqtext.contains("authorization: bearer public"),
            "public tier must send the public bearer token, got: {}",
            reqtext
        );
    }

    #[tokio::test]
    async fn fetch_models_openai_oauth_uses_catalog_not_http() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-fetch-oauth");
        let hit = Arc::new(Mutex::new(false));
        let hit2 = hit.clone();
        let srv = StubServer::spawn(1, move |_i, _req| {
            *hit2.lock().unwrap() = true;
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string()
        });
        let _c = inject_models_dev(catalog_one("openai", "", &srv.addr, &[], &["gpt-5"]));
        crate::providers::auth::set_auth(
            "openai",
            crate::providers::auth::AuthEntry {
                r#type: "oauth".into(),
                access: "tok".into(),
                refresh: "r".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let p = Provider {
            name: "openai".into(),
            id: "openai".into(),
            base_url: format!("http://{}/v1", srv.addr),
            key: "tok".into(),
            ..Default::default()
        };
        let ids = fetch_models(&p).await.unwrap();
        assert!(!*hit.lock().unwrap(), "oauth should not call /v1/models");
        assert_eq!(ids, vec!["gpt-5".to_string()]);
    }

    #[tokio::test]
    async fn find_provider_for_model_scans_all() {
        let _g = test_lock();
        let body = r#"{"data":[{"id":"target-model"}]}"#;
        let srv = StubServer::spawn(2, move |_i, _req| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        });
        crate::providers::modelsdev::set_models_dev_catalog_for_test(None);
        let providers = vec![
            Provider {
                name: "one".into(),
                id: "one".into(),
                base_url: format!("http://{}/v1a", srv.addr),
                ..Default::default()
            },
            Provider {
                name: "two".into(),
                id: "two".into(),
                base_url: format!("http://{}/v1b", srv.addr),
                ..Default::default()
            },
        ];
        let p = find_provider_for_model(&providers, "target-model")
            .await
            .expect("model should be found");
        assert_eq!(p.name, "one");
        assert!(find_provider_for_model(&providers, "missing")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn list_addable_providers_sorts_connected_first() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-list");
        let _e = clear_builtin_provider_env();
        let mut cat = ModelsDevCatalog::new();
        cat.extend(catalog_one("zzz", "Zed", "https://z.example/v1", &[], &[]));
        cat.extend(catalog_one("aaa", "Aaa", "https://a.example/v1", &[], &[]));
        let _c = inject_models_dev(cat);
        crate::providers::auth::set_auth(
            "zzz",
            crate::providers::auth::AuthEntry {
                r#type: "api".into(),
                key: "k".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let got = list_addable_providers();
        let mut seen_unconnected = false;
        for e in &got {
            if !e.connected {
                seen_unconnected = true;
                continue;
            }
            assert!(!seen_unconnected, "connected after unconnected: {:?}", got);
        }
        assert!(!got.is_empty() && got[0].connected, "{:?}", got);
    }

    #[test]
    fn list_addable_providers_includes_public_zen_and_excludes_ollama_local() {
        let _g = test_lock();
        let _d = isolate_data_dir("prov-list-public-zen");
        let _e = clear_builtin_provider_env();
        let mut cat = ModelsDevCatalog::new();
        cat.extend(catalog_one(
            "opencode",
            "OpenCode Zen",
            "https://opencode.ai/zen/v1",
            &[],
            &[],
        ));
        cat.extend(catalog_one(
            "ollama-local",
            "Ollama Local",
            "http://localhost:11434/v1",
            &[],
            &[],
        ));
        let _c = inject_models_dev(cat);

        let got = list_addable_providers();
        let zen = got
            .iter()
            .find(|entry| entry.id == "opencode")
            .expect("public Zen provider entry");
        assert!(zen.connected);
        assert!(!zen.stored);
        assert_eq!(zen.status, "connected (public)");
        assert!(got.iter().all(|entry| entry.id != "ollama-local"));
    }

    #[test]
    fn sse_data_semantics_match_go() {
        assert_eq!(sse_data("data: hello"), Some("hello"));
        assert_eq!(sse_data("data:hello"), Some("hello"));
        assert_eq!(sse_data("  data: [DONE]  "), Some("[DONE]"));
        assert_eq!(sse_data("data:"), Some(""));
        assert_eq!(sse_data(": comment"), None);
        assert_eq!(sse_data("event: done"), None);
        assert_eq!(sse_data(""), None);
    }

    fn sse_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn test_chat_request() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![types::Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            }],
            stream: true,
            tools: vec![],
            reasoning_effort: String::new(),
            stream_options: None,
        }
    }

    async fn collect_chunks(
        base: &str,
        key: &str,
    ) -> Vec<anyhow::Result<crate::types::StreamChunk>> {
        let stream = stream_chat(base, key, test_chat_request(), "reasoning")
            .await
            .unwrap();
        stream.collect::<Vec<_>>().await
    }

    #[tokio::test]
    async fn stream_chat_parses_sse_chunks_and_stops_at_done() {
        let _g = test_lock();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let body = concat!(
            ": ping\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n",
            "data: [DONE]\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"NEVER\"}}]}\n",
        );
        let srv = StubServer::spawn(1, move |_i, req| {
            *seen2.lock().unwrap() = req.to_string();
            sse_response(body)
        });
        let chunks = collect_chunks(&format!("http://{}/v1", srv.addr), "sk-live").await;
        assert!(
            chunks.iter().all(|c| c.is_ok()),
            "chunk errors: {:?}",
            chunks
                .iter()
                .filter_map(|c| c.as_ref().err())
                .collect::<Vec<_>>()
        );
        let contents: Vec<String> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .flat_map(|c| c.choices.iter())
            .map(|ch| ch.delta.content.clone())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(contents.join(""), "Hello");
        let reasonings: Vec<String> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .flat_map(|c| c.choices.iter())
            .map(|ch| ch.delta.reasoning.clone())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(reasonings.join(""), "think");
        let usage = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .find_map(|c| c.usage.clone());
        let usage = usage.expect("usage chunk missing");
        assert_eq!((usage.prompt_tokens, usage.total_tokens), (3, 5));
        let reqtext = seen.lock().unwrap().to_lowercase();
        assert!(reqtext.contains("post /v1/chat/completions"));
        assert!(reqtext.contains("authorization: bearer sk-live"));
    }

    #[tokio::test]
    async fn stream_chat_buffers_partial_lines_and_omits_empty_auth() {
        let _g = test_lock();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        // One JSON object split across two network writes, no [DONE].
        let srv = StubServer::spawn(1, move |_i, req| {
            *seen2.lock().unwrap() = req.to_string();
            let full =
                "data: {\"choices\":[{\"delta\":{\"content\":\"split\"},\"finish_reason\":\"stop\"}]}\n";
            let mid = full.len() / 2;
            let (a, b) = full.split_at(mid);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                a.len(), a, b.len(), b
            )
        });
        let chunks = collect_chunks(&format!("http://{}/v1", srv.addr), "").await;
        let joined: String = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .flat_map(|c| c.choices.iter())
            .map(|ch| ch.delta.content.clone())
            .collect();
        assert_eq!(joined, "split");
        let finish = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .flat_map(|c| c.choices.iter())
            .map(|ch| ch.finish_reason.clone())
            .find(|f| !f.is_empty());
        assert_eq!(finish.as_deref(), Some("stop"));
        let reqtext = seen.lock().unwrap().to_lowercase();
        assert!(
            !reqtext.contains("authorization:"),
            "empty key must omit auth header"
        );
    }

    #[tokio::test]
    async fn stream_chat_non_retryable_400_is_error() {
        let _g = test_lock();
        let srv = StubServer::spawn(1, |_i, _req| {
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 15\r\nConnection: close\r\n\r\ninvalid request"
                .to_string()
        });
        let req = test_chat_request();
        let res = stream_chat(&format!("http://{}/v1", srv.addr), "", req, "reasoning").await;
        let err = match res {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("400 Bad Request"));
        assert!(err.to_string().contains("invalid request"));
    }

    #[test]
    fn strip_internal_fields_removes_metadata() {
        let mut body = serde_json::json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "hi"},
                {
                    "role": "assistant",
                    "content": "",
                    "reasoning": "I am thinking",
                    "reasoning_signature": "sig-abc",
                    "reasoning_ms": 1234,
                    "diff": "--- a\n+++ b",
                    "provider": "opencode-go",
                    "model": "deepseek-v4-flash",
                    "duration_ms": 5000,
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                }
            ]
        });
        super::strip_internal_fields(&mut body, "reasoning");
        let msg = &body["messages"][1];
        assert!(msg.get("reasoning_signature").is_none());
        assert!(msg.get("reasoning_ms").is_none());
        assert!(msg.get("diff").is_none());
        assert!(msg.get("provider").is_none());
        assert!(msg.get("model").is_none());
        assert!(msg.get("duration_ms").is_none());
        assert!(msg.get("usage").is_none());
        // reasoning kept as-is when field matches
        assert_eq!(msg["reasoning"], "I am thinking");
    }

    #[test]
    fn strip_internal_fields_renames_reasoning_for_deepseek() {
        let mut body = serde_json::json!({
            "model": "test",
            "messages": [{
                "role": "assistant",
                "content": "hello",
                "reasoning": "deep thought",
                "reasoning_ms": 500,
                "provider": "opencode-go"
            }]
        });
        super::strip_internal_fields(&mut body, "reasoning_content");
        let msg = &body["messages"][0];
        assert!(msg.get("reasoning").is_none());
        assert_eq!(msg["reasoning_content"], "deep thought");
        assert!(msg.get("reasoning_ms").is_none());
        assert!(msg.get("provider").is_none());
    }

    #[test]
    fn strip_internal_fields_skips_empty_reasoning() {
        let mut body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "assistant", "content": "hello", "reasoning": ""}]
        });
        super::strip_internal_fields(&mut body, "reasoning_content");
        let msg = &body["messages"][0];
        assert!(msg.get("reasoning").is_none());
        assert!(msg.get("reasoning_content").is_none());
    }
}
