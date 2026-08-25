//! Provider registry, models.dev catalog, auth, OAuth, codex compat,
//! and the shared SSE streaming client.

pub mod anthropic;
pub mod auth;
pub mod bedrock;
pub mod codex;
pub mod modelsdev;
#[cfg(test)]
pub mod modelsdev_go_compat;
pub mod oauth;
pub mod providers;
pub mod retry;

// Re-exports other crates are expected to call (atom-server, atom-tools).
pub use anthropic::stream_anthropic;
pub use auth::AuthEntry;
pub use bedrock::{map_stop_reason as bedrock_map_stop_reason, stream_bedrock};
pub use codex::{
    do_openai_codex_round, marshal_openai_codex_request, openai_codex_auth_for_key,
    CodexRoundOutcome, CodexStreamOutcome,
};
pub use modelsdev::{
    anthropic_style_for_url, bedrock_style_for_url, catalog_free_model_ids, context_window_tokens,
    derive_reasoning_levels, ensure_models_dev_catalog, find_catalog_model,
    load_models_dev_catalog_bytes, models_dev_style, provider_is_anthropic_style,
    provider_is_bedrock, reasoning_levels_for, set_models_dev_catalog_for_test,
    suggest_catalog_model_ids, thinking_off_value, valid_thinking_level, ModelsDevCatalog,
    ModelsDevCost, ModelsDevLimit, ModelsDevModel, ModelsDevProvider,
};
pub use oauth::{
    ensure_openai_auth, new_openai_oauth_flow, redact_oauth_text, refresh_openai_token,
    run_openai_oauth, OpenAIOAuthFlow,
};
pub use providers::{
    build_providers, entry_matches_query, filter_entries, filter_provider_entries,
    find_provider_for_model, list_addable_providers, provider_by_name, provider_name_for_url,
    reasoning_field_for_url, sse_data, stream_chat, ModelEntry, Provider, ProviderListEntry,
};
pub use retry::{
    do_http_with_retry, incomplete_reasoning_stream, long_timeout_client,
    should_nudge_incomplete_reasoning, ProviderHTTPError, MAX_EMPTY_RESPONSE_RETRIES,
    MAX_REASONING_NUDGES, REASONING_NUDGE_TEXT,
};

#[cfg(test)]
pub(crate) use self::providers::testutil;
#[cfg(test)]
pub(crate) use self::providers::testutil::{
    clear_builtin_provider_env, inject_models_dev, isolate_data_dir, set_env, test_lock,
    CatalogGuard, DataDirGuard, EnvVarGuard, StubServer,
};
