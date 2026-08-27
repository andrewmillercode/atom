//! dispatch tool, ported from dispatch.go: argument validation
//! (model catalog + thinking levels via an injectable ModelCatalog),
//! session-id parsing helpers, and DispatchPlan routing through the
//! ctx spawner. Session-store work (child creation, ownership checks,
//! nested-dispatch guard) lives behind SubagentHandle because Go's
//! executeDispatch reaches into *SessionStore/*Session directly.

use crate::exec::ToolCtx;
use once_cell::sync::Lazy;
use std::sync::RwLock;

/// One dispatch invocation after parsing: spawn (session_id empty),
/// follow-up (prompt) or cancel (cancel=true).
#[derive(Debug, Clone, Default)]
pub struct DispatchPlan {
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub prompt: String,
    pub session_id: String,
    pub wait_mode: String,
    pub batch_id: String,
    pub batch_index: usize,
    pub ids: Vec<String>,
    pub include_results: bool,
}

// ---------------------------------------------------------------------------
// models.dev catalog hook. Go reads the shared catalog from modelsdev.go;
// until that crate surface exists here, the server installs a
// ModelCatalog implementation. None (or an empty catalog) matches Go's
// offline behavior: model ids are accepted and any non-empty thinking
// level passes.
// ---------------------------------------------------------------------------

pub trait ModelCatalog: Send + Sync {
    /// reasoningLevelsFor: empty = omit reasoning_effort / accept any.
    fn reasoning_levels_for(&self, provider: &str, model_id: &str) -> Vec<String>;
    fn contains_model(&self, model_id: &str) -> bool;
    /// Candidate ids for typo suggestions (unfiltered).
    fn catalog_ids(&self) -> Vec<String>;
}

static CATALOG: Lazy<RwLock<Option<std::sync::Arc<dyn ModelCatalog>>>> =
    Lazy::new(|| RwLock::new(None));

/// Installs the process-wide catalog (the server wires models.dev).
pub fn set_model_catalog(catalog: Option<std::sync::Arc<dyn ModelCatalog>>) {
    *CATALOG.write().unwrap() = catalog;
}

fn with_catalog<R>(f: impl FnOnce(Option<&dyn ModelCatalog>) -> R) -> R {
    let guard = CATALOG.read().unwrap();
    f(guard.as_deref())
}

fn catalog_empty(catalog: Option<&dyn ModelCatalog>) -> bool {
    match catalog {
        None => true,
        Some(c) => c.catalog_ids().is_empty(),
    }
}

/// validateDispatchModel rejects ids that are not in the models.dev
/// catalog. An empty catalog (offline / tests) skips the check so a
/// locally hosted model still works.
pub fn validate_dispatch_model(model: &str) -> String {
    // Dispatch runs after ensure_models_dev_catalog(). Prefer atom-core's
    // live production catalog; the injectable hook below remains useful for
    // isolated atom-tools tests and offline/local-model operation.
    let core_ids = atom_core::providers::modelsdev::models_dev_provider_ids();
    if !core_ids.is_empty() {
        if atom_core::providers::modelsdev::catalog_contains_model(model) {
            return String::new();
        }
        let sugg = atom_core::providers::modelsdev::suggest_catalog_model_ids(model, 3);
        if !sugg.is_empty() {
            return format!(
                "error: unknown model \"{model}\" (did you mean {}?)",
                sugg.join(", ")
            );
        }
        return format!("error: unknown model \"{model}\"");
    }

    with_catalog(|catalog| {
        if catalog_empty(catalog) {
            return String::new();
        }
        let catalog = catalog.unwrap();
        if catalog.contains_model(model) {
            return String::new();
        }
        let sugg =
            suggest_catalog_model_ids(catalog.catalog_ids().iter().map(String::as_str), model, 3);
        if !sugg.is_empty() {
            return format!(
                "error: unknown model \"{model}\" (did you mean {}?)",
                sugg.join(", ")
            );
        }
        format!("error: unknown model \"{model}\"")
    })
}

pub fn reasoning_levels_for(provider: &str, model: &str) -> Vec<String> {
    if !atom_core::providers::modelsdev::models_dev_provider_ids().is_empty() {
        return atom_core::providers::modelsdev::reasoning_levels_for(provider, model)
            .unwrap_or_default();
    }
    with_catalog(|catalog| match catalog {
        Some(c) => c.reasoning_levels_for(provider, model),
        None => Vec::new(),
    })
}

/// validThinkingLevel reports whether level is allowed for the model.
/// With no catalog entry, any non-empty string is accepted so dispatch
/// still works offline.
pub fn valid_thinking_level(provider: &str, model: &str, level: &str) -> bool {
    if level.is_empty() {
        return false;
    }
    let levels = reasoning_levels_for(provider, model);
    if levels.is_empty() {
        return true;
    }
    levels.iter().any(|l| l == level)
}

pub fn normalize_model_id(id: &str) -> String {
    id.chars()
        .filter(|r| !matches!(r, '-' | '_' | ':' | '.' | '/' | ' '))
        .flat_map(|r| r.to_lowercase())
        .collect()
}

/// suggestCatalogModelIDs returns catalog ids that look like typos of
/// want (stripped punctuation). Empty when the query is too short to
/// match safely. Shared helper for ModelCatalog implementations.
pub fn suggest_catalog_model_ids<'a>(
    ids: impl Iterator<Item = &'a str>,
    want: &str,
    limit: usize,
) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let norm = normalize_model_id(want);
    if norm.len() < 4 {
        return Vec::new();
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut matches = Vec::new();
    for id in ids {
        if seen.contains(id) {
            continue;
        }
        let nid = normalize_model_id(id);
        let eq = nid == norm;
        let close = norm.len() >= 6 && (nid.starts_with(&norm) || norm.starts_with(&nid));
        if !eq && !close {
            continue;
        }
        seen.insert(id.to_string());
        matches.push(id.to_string());
    }
    matches.sort();
    matches.truncate(limit);
    matches
}

// ---------------------------------------------------------------------------
// Session-id helpers.
// ---------------------------------------------------------------------------

/// parseDispatchSessionID extracts the 16-hex session id from a dispatch
/// tool result. The first "id: ..." line wins.
pub fn parse_dispatch_session_id(result: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(id) = value.get("id").and_then(|id| id.as_str()).or_else(|| {
            value
                .get("delegates")
                .and_then(|delegates| delegates.as_array())
                .and_then(|delegates| delegates.first())
                .and_then(|delegate| delegate.get("id"))
                .and_then(|id| id.as_str())
        }) {
            if is_dispatch_session_id(id) {
                return id.to_string();
            }
        }
    }
    for line in result.split('\n') {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("id:") else {
            continue;
        };
        let id = rest.trim();
        if is_dispatch_session_id(id) {
            return id.to_string();
        }
    }
    String::new()
}

pub fn is_dispatch_session_id(id: &str) -> bool {
    id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit())
}

pub async fn execute_dispatch_models(ctx: &ToolCtx<'_>, arguments: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        query: String,
    }

    let args: Args = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(err) => return format!("error parsing arguments: {err}"),
    };

    atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
    let active_name = atom_core::providers::provider_name_for_url(&ctx.base_url);
    let active_id = if matches!(active_name.as_str(), "custom" | "ollama-local") {
        String::new()
    } else {
        atom_core::providers::modelsdev::models_dev_provider_id(&active_name)
    };
    let mut providers = atom_core::providers::providers::build_providers().await;
    if !providers.iter().any(|provider| {
        provider.base_url.trim_end_matches('/') == ctx.base_url.trim_end_matches('/')
    }) {
        providers.push(atom_core::providers::Provider {
            name: active_name,
            id: active_id,
            base_url: ctx.base_url.clone(),
            key: ctx.api_key.clone(),
            reasoning_field: ctx.reasoning_field.clone(),
        });
    }

    let results = futures::future::join_all(providers.into_iter().map(|provider| async move {
        let models = atom_core::providers::providers::fetch_models(&provider).await;
        (provider.name, models)
    }))
    .await;
    let available = results
        .into_iter()
        .filter_map(|(provider, result)| result.ok().map(|models| (provider, models)))
        .collect();
    format_dispatch_models(&args.query, available)
}

fn format_dispatch_models(query: &str, providers: Vec<(String, Vec<String>)>) -> String {
    let query = query.trim().to_lowercase();
    let mut providers: Vec<(String, Vec<String>)> = providers
        .into_iter()
        .filter_map(|(provider, models)| {
            let mut models: Vec<String> = models
                .into_iter()
                .filter(|model| query.is_empty() || model.to_lowercase().contains(&query))
                .collect();
            models.sort();
            models.dedup();
            (!models.is_empty()).then_some((provider, models))
        })
        .collect();
    providers.sort_by(|a, b| a.0.cmp(&b.0));
    serde_json::json!({
        "query": query,
        "providers": providers
            .into_iter()
            .map(|(provider, models)| serde_json::json!({
                "provider": provider,
                "models": models,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Tool body.
// ---------------------------------------------------------------------------

const MAX_DISPATCH_BATCH: usize = 100;

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct DispatchArgs {
    action: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    prompt: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    tasks: Vec<String>,
    batch_id: String,
    ids: Vec<String>,
    messages: Vec<DispatchMessage>,
    wait: String,
    results: Option<bool>,
    statuses: Vec<String>,
    query: String,
}

#[derive(serde::Deserialize)]
struct DispatchMessage {
    id: String,
    prompt: String,
}

/// Accept `tasks` as either a list of strings or a single string (which
/// becomes the first — and only — element of the list).
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string or a list of strings")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut tasks = Vec::new();
            while let Some(task) = seq.next_element::<String>()? {
                tasks.push(task);
            }
            Ok(tasks)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

/// executeDispatch spawns a child, posts a follow-up to one, fetches a
/// result, or cancels it. Without a spawner this mirrors Go's nil
/// store/parent error.
pub async fn execute_dispatch(ctx: &ToolCtx<'_>, arguments: &str) -> String {
    let args: DispatchArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    let Some(spawner) = ctx.spawner else {
        return "error: dispatch requires an active session".to_string();
    };
    let action = args.action.trim();
    if action == "models" {
        return execute_dispatch_models(ctx, &serde_json::json!({"query": args.query}).to_string())
            .await;
    }
    if !matches!(args.wait.as_str(), "" | "none" | "any" | "all") {
        return "error: wait must be one of none, any, all".to_string();
    }

    let base = DispatchPlan {
        provider: args.provider.trim().to_string(),
        model: args.model.trim().to_string(),
        thinking: args.thinking.trim().to_string(),
        prompt: args.prompt.trim().to_string(),
        wait_mode: if args.wait.is_empty() {
            "none".into()
        } else {
            args.wait.clone()
        },
        batch_id: args.batch_id.trim().to_string(),
        ids: args.ids.clone(),
        include_results: args.results.unwrap_or(true),
        ..Default::default()
    };

    match action {
        "spawn" => {
            if args.tasks.is_empty() {
                return "error: spawn requires at least one task".to_string();
            }
            if args.tasks.len() > MAX_DISPATCH_BATCH {
                return format!(
                    "error: tasks cannot contain more than {MAX_DISPATCH_BATCH} subagents"
                );
            }
            if args.tasks.iter().any(|task| task.trim().is_empty()) {
                return "error: every task must be a non-empty string".to_string();
            }
            let batch_id = atom_core::session::store::new_session_id();
            let plans = args
                .tasks
                .into_iter()
                .enumerate()
                .map(|(index, prompt)| DispatchPlan {
                    prompt: prompt.trim().to_string(),
                    batch_id: batch_id.clone(),
                    batch_index: index + 1,
                    ..base.clone()
                });
            let spawned = futures::future::join_all(plans.map(|plan| spawner.spawn(plan))).await;
            if let Some(error) = spawned.iter().find(|result| result.starts_with("error:")) {
                return error.clone();
            }
            spawner
                .result(DispatchPlan {
                    batch_id,
                    include_results: base.wait_mode != "none",
                    ..base
                })
                .await
        }
        "inspect" => spawner.result(base).await,
        "send" => {
            let mut plans = Vec::new();
            for message in args.messages {
                plans.push(DispatchPlan {
                    session_id: message.id.trim().to_string(),
                    prompt: message.prompt.trim().to_string(),
                    thinking: base.thinking.clone(),
                    ..Default::default()
                });
            }
            if plans.is_empty() {
                let target = spawner
                    .result(DispatchPlan {
                        include_results: false,
                        ..base.clone()
                    })
                    .await;
                let ids = delegate_ids_from_snapshot(&target);
                plans.extend(ids.into_iter().map(|session_id| DispatchPlan {
                    session_id,
                    prompt: base.prompt.clone(),
                    thinking: base.thinking.clone(),
                    ..Default::default()
                }));
            }
            if plans.is_empty() || plans.iter().any(|plan| plan.prompt.is_empty()) {
                return "error: send requires targets and a non-empty prompt".to_string();
            }
            let ids: Vec<String> = plans.iter().map(|plan| plan.session_id.clone()).collect();
            let sent =
                futures::future::join_all(plans.into_iter().map(|plan| spawner.cont(plan))).await;
            if let Some(error) = sent.iter().find(|result| result.starts_with("error:")) {
                return error.clone();
            }
            spawner
                .result(DispatchPlan {
                    ids,
                    include_results: false,
                    ..Default::default()
                })
                .await
        }
        "cancel" => {
            let target = spawner
                .result(DispatchPlan {
                    include_results: false,
                    ..base
                })
                .await;
            let statuses = args.statuses;
            let ids = delegates_from_snapshot(&target)
                .into_iter()
                .filter(|(_, status)| statuses.is_empty() || statuses.iter().any(|s| s == status))
                .map(|(id, _)| id);
            let ids: Vec<String> = ids.collect();
            let cancelled = futures::future::join_all(
                ids.iter().map(|id| async move { spawner.cancel(id).await }),
            )
            .await;
            if let Some(error) = cancelled.iter().find(|result| result.starts_with("error:")) {
                return error.clone();
            }
            spawner
                .result(DispatchPlan {
                    ids,
                    include_results: false,
                    ..Default::default()
                })
                .await
        }
        _ => "error: action must be one of models, spawn, inspect, send, cancel".to_string(),
    }
}

fn delegates_from_snapshot(snapshot: &str) -> Vec<(String, String)> {
    serde_json::from_str::<serde_json::Value>(snapshot)
        .ok()
        .and_then(|value| value.get("delegates").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            Some((
                item.get("id")?.as_str()?.to_string(),
                item.get("status")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn delegate_ids_from_snapshot(snapshot: &str) -> Vec<String> {
    delegates_from_snapshot(snapshot)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids_from_results() {
        assert_eq!(
            parse_dispatch_session_id("id: 0123456789abcdef\nmodel: m\nthinking: max"),
            "0123456789abcdef"
        );
        assert_eq!(parse_dispatch_session_id("error: nope"), "");
        assert_eq!(parse_dispatch_session_id("id: short"), "");
        assert_eq!(
            parse_dispatch_session_id("id: 0123456789ABCDEF"),
            "0123456789ABCDEF"
        );
        assert_eq!(
            parse_dispatch_session_id(r#"{"delegates":[{"id":"fedcba9876543210"}]}"#),
            "fedcba9876543210"
        );
    }

    #[test]
    fn id_shape_check() {
        assert!(is_dispatch_session_id("0123456789abcdef"));
        assert!(!is_dispatch_session_id("short"));
        assert!(!is_dispatch_session_id("0123456789abcdeg"));
        assert!(!is_dispatch_session_id(""));
    }

    #[test]
    fn dispatch_models_are_sorted_deduplicated_and_filtered() {
        let output = format_dispatch_models(
            "GPT",
            vec![
                ("z-provider".into(), vec!["other".into()]),
                (
                    "opencode-go".into(),
                    vec!["gpt-5.6-sol".into(), "gpt-5.6-sol".into()],
                ),
            ],
        );
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["providers"][0]["provider"], "opencode-go");
        assert_eq!(
            output["providers"][0]["models"],
            serde_json::json!(["gpt-5.6-sol"])
        );

        assert_eq!(
            format_dispatch_models(
                "missing",
                vec![("opencode-go".into(), vec!["gpt-5.6-sol".into()])]
            ),
            r#"{"providers":[],"query":"missing"}"#
        );
    }

    struct FakeCatalog {
        ids: Vec<String>,
        levels: Vec<(String, Vec<String>)>,
    }
    impl ModelCatalog for FakeCatalog {
        fn reasoning_levels_for(&self, _provider: &str, model: &str) -> Vec<String> {
            self.levels
                .iter()
                .find(|(m, _)| m == model)
                .map(|(_, l)| l.clone())
                .unwrap_or_default()
        }
        fn contains_model(&self, model: &str) -> bool {
            self.ids.iter().any(|i| i == model)
        }
        fn catalog_ids(&self) -> Vec<String> {
            self.ids.clone()
        }
    }

    struct CatalogGuard;
    impl Drop for CatalogGuard {
        fn drop(&mut self) {
            set_model_catalog(None);
        }
    }

    /// The catalog is process-global; serialize the tests that install it.
    static CATALOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn install() -> (CatalogGuard, std::sync::MutexGuard<'static, ()>) {
        let lock = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = install_locked();
        (guard, lock)
    }

    fn install_locked() -> CatalogGuard {
        set_model_catalog(Some(std::sync::Arc::new(FakeCatalog {
            ids: vec![
                "deepseek-v4-flash:0731".to_string(),
                "opencode-go/big".to_string(),
                "m".to_string(),
            ],
            levels: vec![
                (
                    "m".to_string(),
                    vec!["none".into(), "low".into(), "high".into(), "max".into()],
                ),
                (
                    "deepseek-v4-flash:0731".to_string(),
                    vec!["high".into(), "max".into()],
                ),
            ],
        })));
        CatalogGuard
    }

    #[test]
    fn unknown_model_gets_typo_suggestion() {
        let (_g, _lock) = install();
        let err = validate_dispatch_model("deepseekv4flash");
        assert_eq!(
            err,
            "error: unknown model \"deepseekv4flash\" (did you mean deepseek-v4-flash:0731?)"
        );
        let err = validate_dispatch_model("totally-absent-model");
        assert_eq!(err, "error: unknown model \"totally-absent-model\"");
        assert_eq!(validate_dispatch_model("m"), "");
    }

    #[test]
    fn offline_catalog_skips_validation_and_accepts_any_thinking() {
        // No catalog installed at all (serialized against installers).
        let _lock = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_model_catalog(None);
        assert_eq!(validate_dispatch_model("local-only"), "");
        assert!(valid_thinking_level("", "local-only", "whatever"));
        assert!(!valid_thinking_level("", "local-only", ""));
    }

    #[test]
    fn thinking_must_be_listed_when_known() {
        let (_g, _lock) = install();
        assert!(valid_thinking_level("", "m", "low"));
        assert!(!valid_thinking_level("", "m", "extreme"));
        assert_eq!(
            reasoning_levels_for("", "m"),
            vec!["none", "low", "high", "max"]
        );
    }

    #[test]
    fn suggestion_needs_four_normalized_chars() {
        let (_g, _lock) = install();
        assert!(validate_dispatch_model("ab").starts_with("error: unknown model"));
    }
}
