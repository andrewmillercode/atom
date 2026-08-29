//! models.dev catalog: fetch, cache, reasoning levels per model, and
//! context windows. Ported from modelsdev.go (plus the contextWindowTokens
//! helper from main.go, which is catalog-derived).

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
pub const MODELS_DEV_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
pub const MODELS_DEV_USER_AGENT: &str = "atom";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevCost {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub input: f64,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub output: f64,
}

/// modelsDevCatalog is providers -> models from models.dev/api.json.
pub type ModelsDevCatalog = HashMap<String, ModelsDevProvider>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsDevProvider {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub api: String,
    /// AI SDK package from models.dev; "@ai-sdk/anthropic" marks providers
    /// spoken to with the Anthropic Messages wire style.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub npm: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    pub env: Vec<String>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub doc: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_map_values_as_default"
    )]
    pub models: HashMap<String, ModelsDevModel>,
}

/// modelsDevBaseURLFallback covers well-known hosts whose models.dev
/// entries omit `api`. Anthropic native speaks the Messages wire style
/// (see provider_is_anthropic_style); openai stays OpenAI-compatible.
/// amazon-bedrock has no single base URL (endpoints are per-region), so
/// the entry points at the us-east-1 runtime host; stream_bedrock
/// rebuilds the real regional URL per request from AWS_REGION / the
/// model id's geo prefix.
fn models_dev_base_url_fallback(id: &str) -> Option<&'static str> {
    match id {
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com/v1"),
        "amazon-bedrock" => Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        _ => None,
    }
}

/// The models.dev npm marker for providers spoken to with the Anthropic
/// Messages API wire style.
pub const ANTHROPIC_NPM: &str = "@ai-sdk/anthropic";

/// providerIsAnthropicStyle reports whether a models.dev provider id (or
/// atom display name) is documented to speak the Anthropic Messages wire
/// style: its catalog entry sets npm = "@ai-sdk/anthropic", or it is the
/// first-party anthropic fallback.
pub fn provider_is_anthropic_style(id_or_name: &str) -> bool {
    if id_or_name == "anthropic" {
        return true;
    }
    let cat = MODELS_DEV_CATALOG.read().unwrap();
    let Some(p) = cat.as_ref().and_then(|c| c.get(id_or_name)) else {
        return false;
    };
    p.npm.as_ref() == ANTHROPIC_NPM
}

/// modelsDevStyle returns the wire dialect for a catalog provider:
/// "anthropic", "bedrock", or "openai" (the default).
pub fn models_dev_style(id_or_name: &str) -> &'static str {
    if provider_is_anthropic_style(id_or_name) {
        "anthropic"
    } else if provider_is_bedrock(id_or_name) {
        "bedrock"
    } else {
        "openai"
    }
}

/// APIProtocol is the wire dialect a model speaks. Routing picks the
/// stream function that knows how to talk to it. Per-model npm from
/// models.dev overrides provider-level npm, so a model on a
/// Chat-Completions-shaped provider can still speak the Responses API
/// (and vice versa). The default is ChatCompletions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum APIProtocol {
    /// POST {base}/chat/completions. Used by Ollama, OpenCode Go, and
    /// the bulk of models.dev models (npm = "@ai-sdk/openai-compatible"
    /// or "@ai-sdk/azure", or unset).
    ChatCompletions,
    /// POST {base}/responses. Newer OpenAI dialect; npm =
    /// "@ai-sdk/openai". muse-spark-1.2-contributor-free on opencode's
    /// Zen tier is the motivating example — Chat Completions returns
    /// "Internal server error" because the upstream gateway only routes
    /// Responses requests for it.
    OpenAIResponses,
    /// POST {base}/messages. Anthropic Messages; npm =
    /// "@ai-sdk/anthropic". Handled separately by anthropic_style_for_url
    /// but listed here so a model-level npm override can route to it
    /// even when the provider URL doesn't have the /anthropic/ segment.
    AnthropicMessages,
    /// Bedrock Converse; npm = "@ai-sdk/amazon-bedrock". Handled by
    /// bedrock_style_for_url today, kept here for parity.
    BedrockConverse,
    /// POST to {base}/v1beta/models/{model}:streamGenerateContent.
    /// npm = "@ai-sdk/google". Not implemented in MVP; surfaces as a
    /// clear error so users know their model needs new wiring instead
    /// of falling back to Chat Completions and silently 400ing.
    GoogleGemini,
}

impl APIProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            APIProtocol::ChatCompletions => "chat_completions",
            APIProtocol::OpenAIResponses => "openai_responses",
            APIProtocol::AnthropicMessages => "anthropic_messages",
            APIProtocol::BedrockConverse => "bedrock_converse",
            APIProtocol::GoogleGemini => "google_gemini",
        }
    }
}

/// protocolForNPM maps a models.dev AI SDK npm marker to the wire
/// dialect. Unrecognized npm values fall through to ChatCompletions —
/// most third-party gateways (mistral, deepinfra, openrouter,
/// togetherai, groq, cohere, ...) speak the OpenAI Chat Completions
/// dialect at the wire level even when their npm differs from
/// @ai-sdk/openai-compatible.
pub fn protocol_for_npm(npm: &str) -> APIProtocol {
    match npm {
        "" => APIProtocol::ChatCompletions,
        "@ai-sdk/openai" => APIProtocol::OpenAIResponses,
        "@ai-sdk/anthropic" => APIProtocol::AnthropicMessages,
        "@ai-sdk/amazon-bedrock" | "@ai-sdk/amazon-bedrock/mantle" => APIProtocol::BedrockConverse,
        "@ai-sdk/google" | "@ai-sdk/google-vertex" => APIProtocol::GoogleGemini,
        // Anything else (openai-compatible, azure, gateway, deepinfra,
        // mistral, groq, togetherai, cohere, openrouter, ...) defaults
        // to Chat Completions. These are all wire-compatible even when
        // their SDK differs.
        _ => APIProtocol::ChatCompletions,
    }
}

/// effectiveModelNPM returns the AI SDK npm marker atom should use to
/// route a (provider, model) pair. Per-model `provider.npm` from
/// models.dev overrides the provider-level `npm` when set; missing on
/// both sides yields an empty string (treated as ChatCompletions by
/// protocol_for_npm). An empty provider name searches preferred hosts
/// first — same policy as reasoning_levels_for.
pub fn effective_model_npm(provider_name: &str, model_id: &str) -> String {
    let Some(cat) = current_models_dev_catalog() else {
        return String::new();
    };
    let Some((_, entry)) = find_compact_model(&cat, provider_name, model_id) else {
        return String::new();
    };
    if !entry.npm.is_empty() {
        return entry.npm.to_string();
    }
    // Fall back to provider-level npm when the model didn't override.
    cat.get(&entry.provider_id)
        .map(|p| p.npm.to_string())
        .unwrap_or_default()
}

/// apiProtocolFor resolves the wire dialect a (provider, model) pair
/// speaks. An empty provider name searches preferred hosts first.
/// Returns ChatCompletions when the catalog is empty so unknown models
/// keep the legacy /chat/completions path instead of erroring.
pub fn api_protocol_for(provider_name: &str, model_id: &str) -> APIProtocol {
    if model_id.is_empty() {
        return APIProtocol::ChatCompletions;
    }
    let npm = effective_model_npm(provider_name, model_id);
    if !npm.is_empty() {
        return protocol_for_npm(&npm);
    }
    // No model entry — fall back to provider-level npm only when the
    // caller named the provider. Empty provider + unknown model is
    // ChatCompletions, matching the pre-npm default.
    if !provider_name.is_empty() {
        let id = models_dev_provider_id(provider_name);
        if let Some(p) = current_models_dev_catalog()
            .as_ref()
            .and_then(|c| c.get(&id))
        {
            if !p.npm.is_empty() {
                return protocol_for_npm(&p.npm);
            }
        }
    }
    APIProtocol::ChatCompletions
}

/// BEDROCK_PROVIDER_ID is the models.dev catalog id whose entry speaks
/// the Bedrock Converse wire style (npm = "@ai-sdk/amazon-bedrock").
pub const BEDROCK_PROVIDER_ID: &str = "amazon-bedrock";

/// providerIsBedrock reports whether a models.dev provider id (or atom
/// display name) is the Amazon Bedrock provider. The Converse API is
/// model-agnostic, so id equality is the whole check.
pub fn provider_is_bedrock(id_or_name: &str) -> bool {
    id_or_name == BEDROCK_PROVIDER_ID
}

/// anthropicStyleForURL reports whether a base URL looks like an
/// Anthropic Messages endpoint: api.anthropic.com itself, or any gateway
/// exposing an /anthropic/ path segment (MiniMax-style mirrors).
pub fn anthropic_style_for_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    if lower.contains("api.anthropic.com") {
        return true;
    }
    // Path segment check: host/path boundary or a leading path component,
    // so "notanthropic.com" does not match but "/anthropic/v1" does.
    lower.contains("/anthropic/") || lower.ends_with("/anthropic") || lower.contains("//anthropic.")
}

/// bedrockStyleForURL reports whether a base URL points at an Amazon
/// Bedrock runtime endpoint. The Converse API lives on per-region
/// bedrock-runtime hosts, which is also the shape of the catalog
/// fallback entry.
pub fn bedrock_style_for_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("//bedrock-runtime.") && lower.contains(".amazonaws.com")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsDevModel {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub reasoning: bool,
    #[serde(default)]
    pub cost: Option<ModelsDevCost>,
    #[serde(
        default,
        rename = "reasoning_options",
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    pub reasoning_options: Vec<ModelsDevReasoningOpt>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub limit: ModelsDevLimit,
    /// Supported input/output content types, e.g. `input: ["text",
    /// "image"]`. Missing modalities deserialize to empty vectors. Used
    /// to tell multimodal models apart from text-only ones so images are
    /// stripped before a text-only model rejects them.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub modalities: ModelsDevModalities,
    /// Per-model AI SDK npm override. models.dev nests it under
    /// `provider.npm` (a `provider` sub-object that mirrors the
    /// provider-level `npm` for the rare cases a single host offers
    /// different dialects per model — e.g. opencode's Zen tier hosts
    /// gpt-5 with `@ai-sdk/openai` Responses API while other models
    /// use `@ai-sdk/openai-compatible` Chat Completions). Empty /
    /// missing means "use the provider-level npm", which itself can be
    /// empty (legacy caches) and falls through to Chat Completions.
    #[serde(
        rename = "provider_npm",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub provider_npm: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevReasoningOpt {
    #[serde(
        rename = "type",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub kind: String,
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    pub values: Vec<String>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub min: i64,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub max: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsDevLimit {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub context: i64,
}

/// ModelsDevModalities mirrors the models.dev `modalities` object: the
/// content types a model accepts (`input`) and emits (`output`), e.g.
/// `input: ["text", "image"]`. atom only acts on whether `input`
/// includes "image"; the rest is preserved for completeness.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevModalities {
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    pub input: Vec<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    pub output: Vec<String>,
}

#[derive(Default)]
struct CompactModels {
    entries: Box<[CompactModelEntry]>,
    levels: Box<[Box<str>]>,
    level_ids: Box<[u16]>,
}

struct CompactModelEntry {
    id: Box<str>,
    context: i64,
    level_start: u32,
    level_len: u16,
    reasoning: bool,
    free: bool,
    /// Whether the model accepts image input (`modalities.input` lists
    /// "image"). False for text-only models so images can be stripped
    /// before the request leaves atom.
    image: bool,
    /// Per-model npm override (models.dev `provider.npm`). Empty means
    /// "fall back to the provider-level npm" — see effective_model_npm.
    npm: Box<str>,
    /// Catalog provider id the model was found under. Used by
    /// effective_model_npm to fall back to provider-level npm.
    provider_id: Box<str>,
}

#[derive(Default, Deserialize)]
struct CompactModelWire {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    reasoning: bool,
    #[serde(default)]
    cost: Option<ModelsDevCost>,
    #[serde(
        default,
        rename = "reasoning_options",
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    reasoning_options: Vec<ModelsDevReasoningOpt>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    limit: ModelsDevLimit,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    modalities: ModelsDevModalities,
    /// Per-model npm override lives under the `provider` sub-object in
    /// models.dev. We deserialize only the npm field (and ignore the
    /// rest of the provider metadata, which is a copy of the
    /// provider-level entry). An absent `provider`, an absent `npm`,
    /// and an explicit JSON null all deserialize to "" — the same
    /// effective default as no override.
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    npm: Box<str>,
}

/// modelProviderObject is the deserializer for the models.dev
/// `provider` sub-object on a model entry. Only `npm` is consumed;
/// other fields (api, env, doc) are duplicates of the provider-level
/// entry and not worth carrying twice in memory.
#[derive(Default, Deserialize)]
struct ModelProviderObject {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    npm: Box<str>,
}

/// CompactModelWireRaw mirrors the raw models.dev JSON for a model:
/// every field is a direct sibling except `provider`, which is a
/// nested object holding the per-model npm override. We flatten
/// `provider.npm` into the wire's `npm` field in a custom Deserialize
/// impl, keeping the rest of the catalog parsing unchanged.
#[derive(Default)]
struct CompactModelWireRaw {
    reasoning: bool,
    cost: Option<ModelsDevCost>,
    reasoning_options: Vec<ModelsDevReasoningOpt>,
    limit: ModelsDevLimit,
    modalities: ModelsDevModalities,
    /// Flattened from `provider.npm`; absent / null provider object
    /// means no override.
    npm: Box<str>,
}

impl<'de> Deserialize<'de> for CompactModelWireRaw {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Pull every field the catalog schema declares for a model,
        // then collapse `provider.npm` into `npm`. Using Value keeps
        // the parser tolerant: missing fields default to their zero
        // value (or `null_as_default` for Strings), unknown fields are
        // silently ignored, and an absent `provider` object just yields
        // an empty npm override.
        #[derive(Default, Deserialize)]
        struct Raw {
            #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
            reasoning: bool,
            #[serde(default)]
            cost: Option<ModelsDevCost>,
            #[serde(
                default,
                rename = "reasoning_options",
                deserialize_with = "crate::serde_null::null_elements_as_default"
            )]
            reasoning_options: Vec<ModelsDevReasoningOpt>,
            #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
            limit: ModelsDevLimit,
            #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
            modalities: ModelsDevModalities,
            #[serde(default)]
            provider: Option<ModelProviderObject>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(CompactModelWireRaw {
            reasoning: raw.reasoning,
            cost: raw.cost,
            reasoning_options: raw.reasoning_options,
            limit: raw.limit,
            modalities: raw.modalities,
            npm: raw.provider.map(|p| p.npm).unwrap_or_default(),
        })
    }
}

/// CompactModelWire flattens the raw on-disk shape push_compact_model
/// expects (no provider object). Keeping a separate struct avoids
/// leaking the catalog schema into the rest of the crate.
impl From<CompactModelWireRaw> for CompactModelWire {
    fn from(raw: CompactModelWireRaw) -> Self {
        CompactModelWire {
            reasoning: raw.reasoning,
            cost: raw.cost,
            reasoning_options: raw.reasoning_options,
            limit: raw.limit,
            modalities: raw.modalities,
            npm: raw.npm,
        }
    }
}

fn push_compact_model(
    entries: &mut Vec<CompactModelEntry>,
    pooled_levels: &mut Vec<Box<str>>,
    level_ids: &mut Vec<u16>,
    id: Box<str>,
    provider_id: Box<str>,
    wire: CompactModelWire,
) -> Result<(), &'static str> {
    let model = ModelsDevModel {
        reasoning: wire.reasoning,
        cost: wire.cost,
        reasoning_options: wire.reasoning_options,
        limit: wire.limit,
        modalities: wire.modalities.clone(),
        provider_npm: wire.npm.to_string(),
    };
    let supports_image = wire
        .modalities
        .input
        .iter()
        .any(|m| m.eq_ignore_ascii_case("image"));
    let level_start = u32::try_from(level_ids.len()).map_err(|_| "models.dev catalog too large")?;
    if let Some(levels) = derive_reasoning_levels(&model) {
        for level in levels {
            let level_id = match pooled_levels
                .iter()
                .position(|known| known.as_ref() == level)
            {
                Some(i) => i,
                None => {
                    pooled_levels.push(level.into_boxed_str());
                    pooled_levels.len() - 1
                }
            };
            level_ids
                .push(u16::try_from(level_id).map_err(|_| "too many models.dev reasoning levels")?);
        }
    }
    let level_len = u16::try_from(level_ids.len() - level_start as usize)
        .map_err(|_| "too many reasoning levels for one model")?;
    entries.push(CompactModelEntry {
        id,
        context: model.limit.context,
        level_start,
        level_len,
        reasoning: model.reasoning,
        free: model
            .cost
            .as_ref()
            .is_some_and(|cost| cost.input == 0.0 && cost.output == 0.0),
        image: supports_image,
        npm: wire.npm,
        provider_id,
    });
    Ok(())
}

impl<'de> Deserialize<'de> for CompactModels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CompactModelsVisitor;

        impl<'de> Visitor<'de> for CompactModelsVisitor {
            type Value = CompactModels;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a models.dev model map or null")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CompactModels::default())
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(CompactModels::default())
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                let mut levels = Vec::new();
                let mut level_ids = Vec::new();
                while let Some((id, wire)) =
                    map.next_entry::<Box<str>, Option<CompactModelWireRaw>>()?
                {
                    let raw = wire.unwrap_or_default();
                    push_compact_model(
                        &mut entries,
                        &mut levels,
                        &mut level_ids,
                        id,
                        String::new().into_boxed_str(),
                        raw.into(),
                    )
                    .map_err(serde::de::Error::custom)?;
                }
                entries.sort_unstable_by(|a, b| a.id.cmp(&b.id));
                Ok(CompactModels {
                    entries: entries.into_boxed_slice(),
                    levels: levels.into_boxed_slice(),
                    level_ids: level_ids.into_boxed_slice(),
                })
            }
        }

        deserializer.deserialize_any(CompactModelsVisitor)
    }
}

impl CompactModels {
    /// Convenience wrapper for callers that have no provider id handy
    /// (e.g. legacy in-memory conversions). The provider id is only
    /// needed for the npm fallback in effective_model_npm; callers
    /// that don't go through that path can ignore it.
    #[allow(dead_code)]
    fn from_raw(models: HashMap<String, ModelsDevModel>) -> Self {
        Self::from_raw_with_provider(models, "")
    }

    /// from_raw_with_provider builds the compact form knowing which
    /// catalog provider the models belong to. provider_id is recorded
    /// on every entry so effective_model_npm can fall back to the
    /// provider-level npm when the model has no override.
    fn from_raw_with_provider(models: HashMap<String, ModelsDevModel>, provider_id: &str) -> Self {
        let pid = provider_id.to_string().into_boxed_str();
        let mut entries = Vec::with_capacity(models.len());
        let mut levels = Vec::new();
        let mut level_ids = Vec::new();
        for (id, model) in models {
            let wire = CompactModelWire {
                reasoning: model.reasoning,
                cost: model.cost,
                reasoning_options: model.reasoning_options,
                limit: model.limit,
                modalities: model.modalities,
                npm: model.provider_npm.into_boxed_str(),
            };
            push_compact_model(
                &mut entries,
                &mut levels,
                &mut level_ids,
                id.into_boxed_str(),
                pid.clone(),
                wire,
            )
            .expect("in-memory models.dev catalog exceeds compact limits");
        }
        entries.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Self {
            entries: entries.into_boxed_slice(),
            levels: levels.into_boxed_slice(),
            level_ids: level_ids.into_boxed_slice(),
        }
    }

    fn get(&self, id: &str) -> Option<&CompactModelEntry> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.id.as_ref().cmp(id))
            .ok()?;
        Some(&self.entries[index])
    }

    fn levels_for(&self, model: &CompactModelEntry) -> Vec<String> {
        let start = model.level_start as usize;
        self.level_ids[start..start + model.level_len as usize]
            .iter()
            .map(|id| self.levels[*id as usize].to_string())
            .collect()
    }
}

#[derive(Default, Deserialize)]
struct CompactProvider {
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    name: Box<str>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    api: Box<str>,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    npm: Box<str>,
    #[serde(
        default,
        deserialize_with = "crate::serde_null::null_elements_as_default"
    )]
    env: Vec<Box<str>>,
    #[serde(default)]
    models: CompactModels,
}

struct CompactProviderEntry {
    id: Box<str>,
    provider: CompactProvider,
}

#[derive(Default)]
struct CompactModelsDevCatalog {
    providers: Box<[CompactProviderEntry]>,
}

impl<'de> Deserialize<'de> for CompactModelsDevCatalog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CompactCatalogVisitor;

        impl<'de> Visitor<'de> for CompactCatalogVisitor {
            type Value = CompactModelsDevCatalog;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a models.dev provider map or null")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CompactModelsDevCatalog::default())
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(CompactModelsDevCatalog::default())
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut providers = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((id, provider)) =
                    map.next_entry::<Box<str>, Option<CompactProvider>>()?
                {
                    providers.push(CompactProviderEntry {
                        id,
                        provider: provider.unwrap_or_default(),
                    });
                }
                providers.sort_unstable_by(|a, b| a.id.cmp(&b.id));
                Ok(CompactModelsDevCatalog {
                    providers: providers.into_boxed_slice(),
                })
            }
        }

        deserializer.deserialize_any(CompactCatalogVisitor)
    }
}

impl CompactModelsDevCatalog {
    fn from_raw(catalog: ModelsDevCatalog) -> Self {
        let mut providers = Vec::with_capacity(catalog.len());
        for (id, provider) in catalog {
            let id_box = id.clone().into_boxed_str();
            providers.push(CompactProviderEntry {
                id: id_box.clone(),
                provider: CompactProvider {
                    name: provider.name.into_boxed_str(),
                    api: provider.api.into_boxed_str(),
                    npm: provider.npm.into_boxed_str(),
                    env: provider
                        .env
                        .into_iter()
                        .map(String::into_boxed_str)
                        .collect(),
                    models: CompactModels::from_raw_with_provider(provider.models, &id_box),
                },
            });
        }
        providers.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Self {
            providers: providers.into_boxed_slice(),
        }
    }

    fn get(&self, id: &str) -> Option<&CompactProvider> {
        let index = self
            .providers
            .binary_search_by(|entry| entry.id.as_ref().cmp(id))
            .ok()?;
        Some(&self.providers[index].provider)
    }
}

static MODELS_DEV_CATALOG: RwLock<Option<Arc<CompactModelsDevCatalog>>> = RwLock::new(None);
static MODELS_DEV_INIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn models_dev_cache_path() -> PathBuf {
    crate::session::store::data_dir().join("models.dev.json")
}

/// setModelsDevCatalogForTest installs a process-wide catalog without
/// touching the network. Tests use it so lookups never fetch api.json.
pub fn set_models_dev_catalog_for_test(cat: Option<ModelsDevCatalog>) {
    *MODELS_DEV_CATALOG.write().unwrap() = cat.map(CompactModelsDevCatalog::from_raw).map(Arc::new);
}

fn set_catalog(cat: CompactModelsDevCatalog) {
    let mut g = MODELS_DEV_CATALOG.write().unwrap();
    if g.is_none() {
        *g = Some(Arc::new(cat));
    }
}

fn current_models_dev_catalog() -> Option<Arc<CompactModelsDevCatalog>> {
    MODELS_DEV_CATALOG.read().unwrap().as_ref().map(Arc::clone)
}

pub fn load_models_dev_catalog_bytes(b: &[u8]) -> anyhow::Result<ModelsDevCatalog> {
    // Like Go's encoding/json: a null provider value (or a whole-null
    // document) becomes a zero value / empty map instead of an error.
    let mut cat = serde_json::Deserializer::from_slice(b);
    Ok(crate::serde_null::null_map_values_as_default(&mut cat)?)
}

fn load_compact_models_dev_catalog_bytes(b: &[u8]) -> anyhow::Result<CompactModelsDevCatalog> {
    Ok(serde_json::from_slice(b)?)
}

/// catalogHasAPIMetadata reports whether any provider still has an `api`
/// URL. Older caches were re-marshaled through a models-only struct and
/// dropped those fields, which left /providers with only the openai
/// fallback.
pub fn catalog_has_api_metadata(cat: &ModelsDevCatalog) -> bool {
    cat.values().any(|p| !p.api.trim().is_empty())
}

pub fn cache_is_current(cached: &ModelsDevCatalog, mtime: SystemTime) -> bool {
    cache_is_current_at(cached, mtime, SystemTime::now())
}

/// Pure variant of cacheIsCurrent with injectable clock.
pub fn cache_is_current_at(cached: &ModelsDevCatalog, mtime: SystemTime, now: SystemTime) -> bool {
    // Negative ages (future mtime / clock skew) count as fresh, like
    // Go's time.Since comparison.
    let age = now.duration_since(mtime).unwrap_or_default();
    age < MODELS_DEV_MAX_AGE && catalog_has_api_metadata(cached)
}

fn compact_cache_is_current(
    cached: &CompactModelsDevCatalog,
    mtime: SystemTime,
    now: SystemTime,
) -> bool {
    let age = now.duration_since(mtime).unwrap_or_default();
    age < MODELS_DEV_MAX_AGE
        && cached
            .providers
            .iter()
            .any(|entry| !entry.provider.api.trim().is_empty())
}

/// ensureModelsDevCatalog loads a cached catalog or fetches models.dev.
/// A cache younger than 24h is used as-is when it still has provider
/// `api` URLs. On fetch failure, a stale cache is used. Lookups on an
/// empty catalog return no levels.
pub async fn ensure_models_dev_catalog() {
    if current_models_dev_catalog().is_some() {
        return;
    }
    let _init = MODELS_DEV_INIT.lock().await;
    if current_models_dev_catalog().is_some() {
        return;
    }
    let cache_path = models_dev_cache_path();
    let read_path = cache_path.clone();
    let mut cached = tokio::task::spawn_blocking(move || read_models_dev_cache(&read_path))
        .await
        .ok()
        .flatten();
    if let Some((cat, mtime)) = &cached {
        if compact_cache_is_current(cat, *mtime, SystemTime::now()) {
            let (cat, _) = cached.take().unwrap();
            set_catalog(cat);
            return;
        }
    }

    let fetched = fetch_models_dev_catalog().await;
    match fetched {
        Ok(raw) => {
            let parsed = tokio::task::spawn_blocking(move || {
                load_compact_models_dev_catalog_bytes(&raw).map(|fresh| (fresh, raw))
            })
            .await
            .ok()
            .and_then(Result::ok);
            let Some((fresh, raw)) = parsed else {
                if let Some((cat, _)) = cached {
                    set_catalog(cat);
                }
                return;
            };
            if current_models_dev_catalog().is_some() {
                return;
            }
            set_catalog(fresh);
            tokio::task::spawn_blocking(move || write_models_dev_cache(&cache_path, &raw))
                .await
                .ok();
        }
        Err(_) => {
            if current_models_dev_catalog().is_none() {
                if let Some((cat, _)) = cached {
                    set_catalog(cat);
                }
            }
        }
    }
}

fn read_models_dev_cache(path: &PathBuf) -> Option<(CompactModelsDevCatalog, SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let file = std::fs::File::open(path).ok()?;
    let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
    let cat = CompactModelsDevCatalog::deserialize(&mut de).ok()?;
    if cat.providers.is_empty() {
        return None;
    }
    Some((cat, mtime))
}

fn write_models_dev_cache(path: &PathBuf, raw: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, raw)?;
    Ok(())
}

async fn fetch_models_dev_catalog() -> anyhow::Result<Vec<u8>> {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    });
    let resp = CLIENT
        .get(MODELS_DEV_URL)
        .header("User-Agent", MODELS_DEV_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = resp.status();
    let b = resp.bytes().await?;
    if !status.is_success() || status.as_u16() != 200 {
        anyhow::bail!("{}", status);
    }
    Ok(b.to_vec())
}

pub fn models_dev_provider_id(atom_name: &str) -> String {
    match atom_name {
        "ollama" | "ollama-local" => "ollama-cloud".to_string(),
        "opencode-zen" => "opencode".to_string(),
        "opencode-go" => "opencode-go".to_string(),
        other => other.to_string(),
    }
}

pub fn provider_catalog_id(p: &super::providers::Provider) -> String {
    if !p.id.is_empty() {
        return p.id.clone();
    }
    models_dev_provider_id(&p.name)
}

pub fn models_dev_provider_ids() -> Vec<String> {
    match MODELS_DEV_CATALOG.read().unwrap().as_ref() {
        Some(cat) => cat
            .providers
            .iter()
            .map(|entry| entry.id.to_string())
            .collect(),
        None => Vec::new(),
    }
}

pub fn models_dev_provider(id: &str) -> Option<ModelsDevProvider> {
    let cat = MODELS_DEV_CATALOG.read().unwrap();
    let provider = cat.as_ref()?.get(id)?;
    Some(ModelsDevProvider {
        name: provider.name.to_string(),
        api: provider.api.to_string(),
        npm: provider.npm.to_string(),
        env: provider.env.iter().map(ToString::to_string).collect(),
        ..Default::default()
    })
}

/// modelsDevBaseURL is the chat base URL for a catalog provider, with no
/// trailing slash. Empty means atom cannot talk to it. For
/// Anthropic-style providers requests append /messages and /models; for
/// OpenAI-style ones /chat/completions and /models.
pub fn models_dev_base_url(id: &str) -> String {
    if let Some(cat) = MODELS_DEV_CATALOG.read().unwrap().as_ref() {
        if let Some(p) = cat.get(id) {
            let api = p.api.trim().trim_end_matches('/');
            if !api.is_empty() {
                return api.to_string();
            }
        }
    }
    if let Some(u) = models_dev_base_url_fallback(id) {
        return u.trim_end_matches('/').to_string();
    }
    String::new()
}

pub fn catalog_model_ids(id: &str) -> Option<Vec<String>> {
    let cat = MODELS_DEV_CATALOG.read().unwrap();
    let p = cat.as_ref()?.get(id)?;
    Some(
        p.models
            .entries
            .iter()
            .map(|entry| entry.id.to_string())
            .collect(),
    )
}

/// Returns the provider's models whose catalog input and output prices are
/// both zero. OpenCode uses this same models.dev metadata to expose its
/// keyless public Zen tier, so this list stays current as promotions rotate.
pub fn catalog_free_model_ids(id: &str) -> Option<Vec<String>> {
    let cat = MODELS_DEV_CATALOG.read().unwrap();
    let p = cat.as_ref()?.get(id)?;
    Some(
        p.models
            .entries
            .iter()
            .filter(|entry| entry.free)
            .map(|entry| entry.id.to_string())
            .collect(),
    )
}

pub fn is_addable_models_dev_provider(id: &str, p: &ModelsDevProvider) -> bool {
    if !p.api.trim().trim_end_matches('/').is_empty() {
        return true;
    }
    // amazon-bedrock's catalog entry has no api URL (endpoints are
    // per-region); atom talks to it through the bedrock dialect with the
    // fallback runtime host from models_dev_base_url.
    if id == "amazon-bedrock" {
        return true;
    }
    !models_dev_base_url(id).is_empty()
}

pub fn lookup_models_dev_model(
    cat: &ModelsDevCatalog,
    provider_id: &str,
    model_id: &str,
) -> Option<ModelsDevModel> {
    if model_id.is_empty() {
        return None;
    }
    let p = cat.get(provider_id)?;
    if let Some(m) = p.models.get(model_id) {
        return Some(m.clone());
    }
    if let Some(trimmed) = model_id.strip_suffix(":cloud") {
        if let Some(m) = p.models.get(trimmed) {
            return Some(m.clone());
        }
    }
    None
}

fn lookup_compact_model<'a>(
    cat: &'a CompactModelsDevCatalog,
    provider_id: &str,
    model_id: &str,
) -> Option<(&'a CompactModels, &'a CompactModelEntry)> {
    if model_id.is_empty() {
        return None;
    }
    let models = &cat.get(provider_id)?.models;
    if let Some(model) = models.get(model_id) {
        return Some((models, model));
    }
    let trimmed = model_id.strip_suffix(":cloud")?;
    Some((models, models.get(trimmed)?))
}

fn find_compact_model<'a>(
    cat: &'a CompactModelsDevCatalog,
    provider_name: &str,
    model_id: &str,
) -> Option<(&'a CompactModels, &'a CompactModelEntry)> {
    if model_id.is_empty() {
        return None;
    }
    if !provider_name.is_empty() {
        return lookup_compact_model(cat, &models_dev_provider_id(provider_name), model_id);
    }
    for provider_id in CATALOG_PREFERRED_PROVIDERS {
        if let Some(model) = lookup_compact_model(cat, provider_id, model_id) {
            return Some(model);
        }
    }
    for entry in &cat.providers {
        if CATALOG_PREFERRED_PROVIDERS.contains(&entry.id.as_ref()) {
            continue;
        }
        if let Some(model) = lookup_compact_model(cat, &entry.id, model_id) {
            return Some(model);
        }
    }
    None
}

/// catalogPreferredProviders is the lookup order when the caller does
/// not name a provider (dispatch, inherited model id).
const CATALOG_PREFERRED_PROVIDERS: &[&str] = &["opencode-go", "ollama-cloud", "opencode"];

/// findCatalogModel looks up a model in the models.dev catalog. An empty
/// provider searches preferred hosts first, then every other provider.
pub fn find_catalog_model(provider_name: &str, model_id: &str) -> Option<ModelsDevModel> {
    let cat = current_models_dev_catalog()?;
    let (models, model) = find_compact_model(&cat, provider_name, model_id)?;
    let levels = models.levels_for(model);
    let reasoning_options = if levels.is_empty() {
        Vec::new()
    } else {
        vec![ModelsDevReasoningOpt {
            kind: "effort".into(),
            values: levels,
            ..Default::default()
        }]
    };
    Some(ModelsDevModel {
        reasoning: model.reasoning,
        cost: Some(if model.free {
            ModelsDevCost::default()
        } else {
            // Compact lookup only needs to preserve whether pricing is zero.
            ModelsDevCost {
                input: 1.0,
                output: 1.0,
            }
        }),
        reasoning_options,
        limit: ModelsDevLimit {
            context: model.context,
        },
        modalities: if model.image {
            ModelsDevModalities {
                input: vec!["text".into(), "image".into()],
                output: vec!["text".into()],
            }
        } else {
            ModelsDevModalities {
                input: vec!["text".into()],
                output: vec!["text".into()],
            }
        },
        // Preserved so any caller that takes the public ModelsDevModel
        // (e.g. /providers listings) sees the per-model npm. Routing
        // itself uses effective_model_npm, which reads from the compact
        // entry directly to avoid reconstructing a ModelsDevModel.
        provider_npm: model.npm.to_string(),
    })
}

pub fn catalog_contains_model(model_id: &str) -> bool {
    current_models_dev_catalog()
        .and_then(|cat| find_compact_model(&cat, "", model_id).map(|_| ()))
        .is_some()
}

/// modelSupportsImageInput reports whether the model accepts image
/// input, per the models.dev catalog's `modalities.input`. Returns
/// `Some(true)` when the model is multimodal, `Some(false)` when the
/// catalog explicitly lists it as text-only, and `None` for a model the
/// catalog does not know (so callers keep current behavior instead of
/// silently dropping images from a custom model that may in fact be
/// multimodal). An empty provider name searches preferred hosts first.
pub fn model_supports_image_input(provider: &str, model: &str) -> Option<bool> {
    let cat = current_models_dev_catalog()?;
    let (_, entry) = find_compact_model(&cat, provider, model)?;
    Some(entry.image)
}

pub fn normalize_model_id(id: &str) -> String {
    id.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ':' | '.' | '/' | ' '))
        .collect()
}

/// suggestCatalogModelIDs returns catalog ids that look like typos of
/// want (stripped punctuation). Empty when the query is too short to
/// match safely.
pub fn suggest_catalog_model_ids(want: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let norm = normalize_model_id(want);
    if norm.len() < 4 {
        return Vec::new();
    }
    let cat = match current_models_dev_catalog() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for provider in &cat.providers {
        for model in &provider.provider.models.entries {
            let id = &model.id;
            if seen.contains(id.as_ref()) {
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
    }
    matches.sort();
    matches.truncate(limit);
    matches
}

/// reasoningLevelsFor returns the TUI cycle list for a model, derived
/// from that model's models.dev reasoning_options. An empty provider
/// searches preferred hosts, then the rest of the catalog. None means
/// omit reasoning_effort.
pub fn reasoning_levels_for(provider_name: &str, model_id: &str) -> Option<Vec<String>> {
    let cat = current_models_dev_catalog()?;
    let (models, model) = find_compact_model(&cat, provider_name, model_id)?;
    if !model.reasoning || model.level_len == 0 {
        return None;
    }
    Some(models.levels_for(model))
}

pub fn derive_reasoning_levels(m: &ModelsDevModel) -> Option<Vec<String>> {
    if !m.reasoning {
        return None;
    }
    let mut effort: Vec<String> = Vec::new();
    let mut has_toggle = false;
    let mut seen = std::collections::HashSet::new();
    for opt in &m.reasoning_options {
        match opt.kind.as_str() {
            "toggle" => has_toggle = true,
            "effort" => {
                for v in &opt.values {
                    if v.is_empty() || v == "null" || seen.contains(v) {
                        continue;
                    }
                    seen.insert(v.clone());
                    effort.push(v.clone());
                }
            }
            _ => {}
        }
    }
    // models.dev marks off as "none" in effort values. A toggle without
    // that token still means thinking can be disabled, so prepend it.
    if has_toggle && effort.is_empty() {
        return Some(vec!["none".into(), "low".into(), "high".into()]);
    }
    if has_toggle && !has_thinking_off(&effort) {
        effort.insert(0, "none".into());
    }
    if effort.is_empty() {
        return None;
    }
    Some(effort)
}

fn has_thinking_off(levels: &[String]) -> bool {
    levels.iter().any(|l| l == "none")
}

pub fn default_thinking_index(levels: &[String]) -> usize {
    levels.len().saturating_sub(1)
}

/// thinkingOffValue is the wire token that disables reasoning for the
/// model (models.dev "none"), or the first catalog level. Empty means omit.
pub fn thinking_off_value(provider: &str, model: &str) -> String {
    let levels = reasoning_levels_for(provider, model).unwrap_or_default();
    for l in &levels {
        if l == "none" {
            return l.clone();
        }
    }
    levels.first().cloned().unwrap_or_default()
}

/// validThinkingLevel reports whether level is allowed for the model.
/// With no catalog entry, any non-empty string is accepted so dispatch
/// still works offline.
pub fn valid_thinking_level(provider: &str, model: &str, level: &str) -> bool {
    if level.is_empty() {
        return false;
    }
    let levels = reasoning_levels_for(provider, model).unwrap_or_default();
    if levels.is_empty() {
        return true;
    }
    levels.iter().any(|l| l == level)
}

/// contextWindowTokens returns the model's context window size in tokens
/// from the models.dev catalog. Unknown models (or catalog entries with
/// no limit) fall back to 128K so the status-bar fill still has a
/// denominator.
pub fn context_window_tokens(provider_name: &str, model: &str) -> i64 {
    if let Some(context) = current_models_dev_catalog()
        .and_then(|cat| find_compact_model(&cat, provider_name, model).map(|(_, m)| m.context))
    {
        if context > 0 {
            return context;
        }
    }
    128000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_model(provider: &str, id: &str, reasoning: bool, opts: Vec<ModelsDevReasoningOpt>) {
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            provider.to_string(),
            ModelsDevProvider {
                models: HashMap::from([(
                    id.to_string(),
                    ModelsDevModel {
                        reasoning,
                        reasoning_options: opts,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
    }

    fn effort(values: &[&str]) -> ModelsDevReasoningOpt {
        ModelsDevReasoningOpt {
            kind: "effort".into(),
            values: values.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn toggle() -> ModelsDevReasoningOpt {
        ModelsDevReasoningOpt {
            kind: "toggle".into(),
            ..Default::default()
        }
    }

    // Serializes catalog injection across tests (the catalog is global).
    fn lock() -> crate::providers::testutil::TestLockGuard {
        crate::providers::test_lock()
    }

    #[test]
    fn reasoning_levels_effort_only() {
        let _g = lock();
        fixture_model(
            "openai",
            "gpt-5",
            true,
            vec![effort(&["low", "medium", "high"])],
        );
        assert_eq!(
            reasoning_levels_for("openai", "gpt-5"),
            Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string()
            ])
        );
    }

    #[test]
    fn reasoning_levels_toggle_prepends_none() {
        let _g = lock();
        fixture_model(
            "ollama-cloud",
            "deepseek-v4-flash:0731",
            true,
            vec![toggle(), effort(&["high", "max"])],
        );
        assert_eq!(
            reasoning_levels_for("ollama", "deepseek-v4-flash:0731"),
            Some(vec![
                "none".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );
    }

    #[test]
    fn reasoning_levels_toggle_with_none_does_not_prepend() {
        let _g = lock();
        fixture_model(
            "openai",
            "gpt-5",
            true,
            vec![toggle(), effort(&["none", "low", "high"])],
        );
        assert_eq!(
            reasoning_levels_for("openai", "gpt-5"),
            Some(vec![
                "none".to_string(),
                "low".to_string(),
                "high".to_string()
            ])
        );
    }

    #[test]
    fn reasoning_levels_toggle_only() {
        let _g = lock();
        fixture_model("opencode-go", "qwen", true, vec![toggle()]);
        assert_eq!(
            reasoning_levels_for("opencode-go", "qwen"),
            Some(vec![
                "none".to_string(),
                "low".to_string(),
                "high".to_string()
            ])
        );
    }

    #[test]
    fn reasoning_levels_cloud_suffix() {
        let _g = lock();
        fixture_model(
            "ollama-cloud",
            "gpt-oss:20b",
            true,
            vec![effort(&["low", "high"])],
        );
        assert_eq!(
            reasoning_levels_for("ollama", "gpt-oss:20b:cloud"),
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn reasoning_levels_does_not_strip_other_tags() {
        let _g = lock();
        fixture_model("ollama-cloud", "gpt-oss", true, vec![effort(&["low"])]);
        assert_eq!(reasoning_levels_for("ollama", "gpt-oss:20b"), None);
    }

    #[test]
    fn reasoning_levels_provider_mapping() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "ollama-cloud".into(),
            ModelsDevProvider {
                models: HashMap::from([(
                    "m".to_string(),
                    ModelsDevModel {
                        reasoning: true,
                        reasoning_options: vec![effort(&["low"])],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
        cat.insert(
            "opencode".into(),
            ModelsDevProvider {
                models: HashMap::from([(
                    "z".to_string(),
                    ModelsDevModel {
                        reasoning: true,
                        reasoning_options: vec![effort(&["high"])],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            reasoning_levels_for("ollama", "m"),
            Some(vec!["low".to_string()]),
            "ollama maps to ollama-cloud"
        );
        assert_eq!(
            reasoning_levels_for("ollama-local", "m"),
            Some(vec!["low".to_string()]),
            "ollama-local maps to ollama-cloud"
        );
        assert_eq!(
            reasoning_levels_for("opencode-zen", "z"),
            Some(vec!["high".to_string()]),
            "opencode-zen maps to opencode"
        );
    }

    #[test]
    fn reasoning_levels_unknown_model() {
        let _g = lock();
        fixture_model("ollama-cloud", "known", true, vec![effort(&["low"])]);
        assert_eq!(reasoning_levels_for("ollama", "unknown"), None);
    }

    #[test]
    fn reasoning_levels_empty_catalog() {
        let _g = lock();
        set_models_dev_catalog_for_test(None);
        assert_eq!(reasoning_levels_for("ollama", "anything"), None);
    }

    #[test]
    fn reasoning_false_returns_nil() {
        let _g = lock();
        fixture_model("openai", "m", false, vec![effort(&["low", "high"])]);
        assert_eq!(reasoning_levels_for("openai", "m"), None);
    }

    #[test]
    fn default_thinking_index_is_last() {
        let levels = vec!["none".to_string(), "high".to_string(), "max".to_string()];
        assert_eq!(default_thinking_index(&levels), 2);
        assert_eq!(default_thinking_index(&[]), 0);
    }

    #[test]
    fn thinking_off_value_uses_none() {
        let _g = lock();
        fixture_model(
            "ollama-cloud",
            "deepseek-v4-flash:0731",
            true,
            vec![toggle(), effort(&["high", "max"])],
        );
        assert_eq!(
            thinking_off_value("ollama", "deepseek-v4-flash:0731"),
            "none"
        );
    }

    #[test]
    fn valid_thinking_level_offline() {
        let _g = lock();
        set_models_dev_catalog_for_test(None);
        assert!(valid_thinking_level("", "m", "extreme"));
        assert!(!valid_thinking_level("", "m", ""));
    }

    #[test]
    fn load_models_dev_catalog_bytes_fixture() {
        let _g = lock();
        let cat = load_models_dev_catalog_bytes(
            br#"{"openai":{"models":{"gpt-5":{"reasoning":true,"reasoning_options":[{"type":"effort","values":["low","high"]}]}}}}"#,
        )
        .unwrap();
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            reasoning_levels_for("openai", "gpt-5"),
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn compact_catalog_preserves_runtime_fields() {
        let _g = lock();
        let cat = load_compact_models_dev_catalog_bytes(
            br#"{"openai":{"name":"OpenAI","api":"https://api.openai.com/v1/","env":["OPENAI_API_KEY"],"doc":"ignored","models":{"gpt-5":{"reasoning":true,"reasoning_options":[{"type":"toggle"},{"type":"effort","values":["low","high"]}],"limit":{"context":400000}},"plain":null}}}"#,
        )
        .unwrap();
        set_models_dev_catalog_for_test(None);
        set_catalog(cat);

        let provider = models_dev_provider("openai").unwrap();
        assert_eq!(provider.name, "OpenAI");
        assert_eq!(provider.env, vec!["OPENAI_API_KEY"]);
        assert_eq!(models_dev_base_url("openai"), "https://api.openai.com/v1");
        assert_eq!(
            reasoning_levels_for("openai", "gpt-5"),
            Some(vec!["none".into(), "low".into(), "high".into()])
        );
        assert_eq!(context_window_tokens("openai", "gpt-5"), 400000);
        assert_eq!(
            catalog_model_ids("openai"),
            Some(vec!["gpt-5".into(), "plain".into()])
        );
    }

    #[test]
    fn reasoning_levels_search_by_model_id() {
        let _g = lock();
        fixture_model(
            "opencode-go",
            "qwen",
            true,
            vec![toggle(), effort(&["low", "high"])],
        );
        let got = reasoning_levels_for("", "qwen").unwrap();
        assert!(
            got.join(",").contains("none"),
            "dispatch lookup by model id: {:?}",
            got
        );
    }

    #[test]
    fn reasoning_levels_search_all_providers() {
        let _g = lock();
        fixture_model("openai", "gpt-5", true, vec![effort(&["low", "high"])]);
        assert_eq!(
            reasoning_levels_for("", "gpt-5"),
            Some(vec!["low".to_string(), "high".to_string()]),
            "empty provider should find openai models"
        );
    }

    #[test]
    fn suggest_catalog_model_ids_matches_typos() {
        let _g = lock();
        fixture_model(
            "ollama-cloud",
            "deepseek-v4-flash:0731",
            true,
            vec![effort(&["max"])],
        );
        let got = suggest_catalog_model_ids("deepseekv4flash", 3);
        assert_eq!(got, vec!["deepseek-v4-flash:0731".to_string()]);
        assert!(!catalog_contains_model("deepseekv4flash"));
        assert!(catalog_contains_model("deepseek-v4-flash:0731"));
    }

    #[test]
    fn load_models_dev_catalog_provider_metadata() {
        let cat = load_models_dev_catalog_bytes(
            br#"{"openrouter":{"name":"OpenRouter","api":"https://openrouter.ai/api/v1","env":["OPENROUTER_API_KEY"],"doc":"https://openrouter.ai","models":{"x":{"reasoning":false}}}}"#,
        )
        .unwrap();
        let p = &cat["openrouter"];
        assert_eq!(p.name, "OpenRouter");
        assert_eq!(p.api, "https://openrouter.ai/api/v1");
        assert_eq!(p.env, vec!["OPENROUTER_API_KEY"]);
    }

    #[test]
    fn models_dev_base_url_fallbacks() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "openai".into(),
            ModelsDevProvider {
                name: "OpenAI".into(),
                ..Default::default()
            },
        );
        cat.insert(
            "openrouter".into(),
            ModelsDevProvider {
                api: "https://openrouter.ai/api/v1/".into(),
                ..Default::default()
            },
        );
        cat.insert(
            "anthropic".into(),
            ModelsDevProvider {
                name: "Anthropic".into(),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(models_dev_base_url("openai"), "https://api.openai.com/v1");
        assert_eq!(
            models_dev_base_url("openrouter"),
            "https://openrouter.ai/api/v1"
        );
        // Anthropic native has no catalog `api` but is reachable through
        // the Messages-style fallback.
        assert_eq!(
            models_dev_base_url("anthropic"),
            "https://api.anthropic.com/v1"
        );
        assert!(is_addable_models_dev_provider(
            "anthropic",
            &ModelsDevProvider::default()
        ));
        assert!(is_addable_models_dev_provider(
            "openai",
            &ModelsDevProvider::default()
        ));
    }

    #[test]
    fn anthropic_style_detection() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "anthropic".into(),
            ModelsDevProvider {
                name: "Anthropic".into(),
                ..Default::default()
            },
        );
        cat.insert(
            "minimax".into(),
            ModelsDevProvider {
                name: "MiniMax".into(),
                api: "https://api.minimax.io/anthropic/v1".into(),
                npm: "@ai-sdk/anthropic".into(),
                env: vec!["MINIMAX_API_KEY".to_string()],
                ..Default::default()
            },
        );
        cat.insert(
            "openrouter".into(),
            ModelsDevProvider {
                name: "OpenRouter".into(),
                api: "https://openrouter.ai/api/v1".into(),
                npm: "@ai-sdk/openai-compatible".into(),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        // First-party fallback plus the npm marker.
        assert!(provider_is_anthropic_style("anthropic"));
        assert!(provider_is_anthropic_style("minimax"));
        assert!(!provider_is_anthropic_style("openrouter"));
        assert!(!provider_is_anthropic_style("unknown-provider"));
        assert!(!provider_is_anthropic_style(""));
        assert_eq!(models_dev_style("minimax"), "anthropic");
        assert_eq!(models_dev_style("openrouter"), "openai");

        // An old cache without npm data must still load and simply be
        // treated as openai style (except first-party anthropic).
        let raw = br#"{"gateway":{"name":"Gateway","api":"https://gw.example/v1","models":{}}}"#;
        let cat = load_models_dev_catalog_bytes(raw).unwrap();
        assert!(cat["gateway"].npm.is_empty());
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(models_dev_style("gateway"), "openai");
    }

    #[test]
    fn catalog_has_api_metadata_and_cache_freshness() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        let mut stripped = ModelsDevCatalog::new();
        stripped.insert(
            "openai".into(),
            ModelsDevProvider {
                models: HashMap::from([("gpt-5".to_string(), ModelsDevModel::default())]),
                ..Default::default()
            },
        );
        assert!(!catalog_has_api_metadata(&stripped));
        assert!(!cache_is_current_at(&stripped, now, now));

        let mut full = ModelsDevCatalog::new();
        full.insert(
            "openrouter".into(),
            ModelsDevProvider {
                api: "https://openrouter.ai/api/v1".into(),
                ..Default::default()
            },
        );
        assert!(catalog_has_api_metadata(&full));
        assert!(cache_is_current_at(&full, now, now));
        assert!(!cache_is_current_at(
            &full,
            now,
            now + MODELS_DEV_MAX_AGE + Duration::from_secs(1)
        ));
    }

    #[test]
    fn write_models_dev_cache_preserves_api() {
        let _g = lock();
        let d = crate::providers::isolate_data_dir("modelsdev-cache");

        let raw =
            br#"{"openrouter":{"name":"OpenRouter","api":"https://openrouter.ai/api/v1","models":{}}}"#;
        write_models_dev_cache(&models_dev_cache_path(), raw).unwrap();
        let (cat, _) = read_models_dev_cache(&models_dev_cache_path()).unwrap();
        assert_eq!(
            cat.get("openrouter").unwrap().api.as_ref(),
            "https://openrouter.ai/api/v1"
        );
        drop(d);
    }

    #[test]
    fn catalog_model_ids_sorted() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "openai".into(),
            ModelsDevProvider {
                api: "https://api.openai.com/v1".into(),
                models: HashMap::from([
                    ("z".to_string(), ModelsDevModel::default()),
                    ("a".to_string(), ModelsDevModel::default()),
                ]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            catalog_model_ids("openai"),
            Some(vec!["a".to_string(), "z".to_string()])
        );
    }

    #[test]
    fn context_window_tokens_from_limit() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "openai".into(),
            ModelsDevProvider {
                models: HashMap::from([
                    (
                        "big".to_string(),
                        ModelsDevModel {
                            limit: ModelsDevLimit { context: 400000 },
                            ..Default::default()
                        },
                    ),
                    ("small".to_string(), ModelsDevModel::default()),
                ]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(context_window_tokens("openai", "big"), 400000);
        assert_eq!(context_window_tokens("openai", "small"), 128000);
        assert_eq!(context_window_tokens("openai", "unknown"), 128000);
    }

    #[test]
    fn model_supports_image_input_reads_modalities() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "openai".into(),
            ModelsDevProvider {
                models: HashMap::from([
                    (
                        "vision-model".to_string(),
                        ModelsDevModel {
                            modalities: ModelsDevModalities {
                                input: vec!["text".into(), "image".into()],
                                output: vec!["text".into()],
                            },
                            ..Default::default()
                        },
                    ),
                    (
                        "text-only-model".to_string(),
                        ModelsDevModel {
                            modalities: ModelsDevModalities {
                                input: vec!["text".into()],
                                output: vec!["text".into()],
                            },
                            ..Default::default()
                        },
                    ),
                    // No modalities at all → treated as text-only.
                    ("no-modalities".to_string(), ModelsDevModel::default()),
                ]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            model_supports_image_input("openai", "vision-model"),
            Some(true)
        );
        assert_eq!(
            model_supports_image_input("openai", "text-only-model"),
            Some(false)
        );
        assert_eq!(
            model_supports_image_input("openai", "no-modalities"),
            Some(false)
        );
        // Unknown model / provider → None: callers keep current behavior
        // instead of stripping images from a possibly-multimodal custom model.
        assert_eq!(model_supports_image_input("openai", "unknown-model"), None);
        assert_eq!(model_supports_image_input("nope", "vision-model"), None);
    }

    #[test]
    fn normalize_strips_punctuation_and_lowercases() {
        assert_eq!(
            normalize_model_id("GPT-OSS:20B_Cloud/Fast"),
            "gptoss20bcloudfast"
        );
    }

    #[test]
    fn current_catalog_snapshots_share_storage() {
        let _g = lock();
        fixture_model("openai", "gpt-5", false, vec![]);
        let first = current_models_dev_catalog().unwrap();
        let second = current_models_dev_catalog().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// protocol_for_npm maps each well-known AI SDK marker to the
    /// wire dialect atom talks in. Unrecognized / unset npm falls
    /// through to ChatCompletions because most third-party gateways
    /// (mistral, deepinfra, openrouter, togetherai, groq, cohere, ...)
    /// speak Chat Completions even though their SDK marker differs.
    #[test]
    fn protocol_for_npm_maps_well_known_markers() {
        assert_eq!(protocol_for_npm(""), APIProtocol::ChatCompletions);
        assert_eq!(
            protocol_for_npm("@ai-sdk/openai"),
            APIProtocol::OpenAIResponses
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/anthropic"),
            APIProtocol::AnthropicMessages
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/amazon-bedrock"),
            APIProtocol::BedrockConverse
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/amazon-bedrock/mantle"),
            APIProtocol::BedrockConverse
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/google"),
            APIProtocol::GoogleGemini
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/google-vertex"),
            APIProtocol::GoogleGemini
        );
        // Chat-Completions-compatible surface (openai-compatible, azure,
        // openrouter, deepinfra, mistral, groq, togetherai, ...).
        assert_eq!(
            protocol_for_npm("@ai-sdk/openai-compatible"),
            APIProtocol::ChatCompletions
        );
        assert_eq!(
            protocol_for_npm("@ai-sdk/azure"),
            APIProtocol::ChatCompletions
        );
        assert_eq!(
            protocol_for_npm("@openrouter/ai-sdk-provider"),
            APIProtocol::ChatCompletions
        );
        assert_eq!(protocol_for_npm(""), APIProtocol::ChatCompletions);
    }

    /// Per-model npm override beats provider-level npm. This is the
    /// critical case: opencode's Zen tier has provider-level npm =
    /// "@ai-sdk/openai-compatible" but hosts models that speak
    /// Responses (npm = "@ai-sdk/openai") and Google Gemini (npm =
    /// "@ai-sdk/google") on the same base URL — routing must follow
    /// the model entry.
    #[test]
    fn effective_model_npm_per_model_override_wins() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "opencode".into(),
            ModelsDevProvider {
                // Provider-level npm would default to ChatCompletions
                // if a model didn't override.
                npm: "@ai-sdk/openai-compatible".into(),
                models: HashMap::from([
                    (
                        // muse-spark-1.2-contributor-free on the Zen tier
                        // speaks /responses.
                        "muse-spark-1.2-contributor-free".into(),
                        ModelsDevModel {
                            provider_npm: "@ai-sdk/openai".into(),
                            ..Default::default()
                        },
                    ),
                    (
                        // gemini-3-pro on the Zen tier speaks Google.
                        "gemini-3-pro".into(),
                        ModelsDevModel {
                            provider_npm: "@ai-sdk/google".into(),
                            ..Default::default()
                        },
                    ),
                    (
                        // No model-level override → falls back to the
                        // provider-level npm.
                        "laguna-s-2.1-free".into(),
                        ModelsDevModel::default(),
                    ),
                ]),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            effective_model_npm("opencode", "muse-spark-1.2-contributor-free"),
            "@ai-sdk/openai",
            "model-level override beats provider-level default"
        );
        assert_eq!(
            effective_model_npm("opencode", "gemini-3-pro"),
            "@ai-sdk/google"
        );
        assert_eq!(
            effective_model_npm("opencode", "laguna-s-2.1-free"),
            "@ai-sdk/openai-compatible",
            "missing override falls back to provider npm"
        );
        // Unknown model: empty result, api_protocol_for falls through.
        assert_eq!(effective_model_npm("opencode", "not-in-catalog"), "");
    }

    /// api_protocol_for is the routing entry point used in turn.rs. An
    /// empty model id defaults to ChatCompletions (defensive against
    /// uninitialized state), the override path picks Responses for
    /// muse-spark, and an empty catalog returns ChatCompletions so
    /// unknown custom models keep the legacy /chat/completions path.
    #[test]
    fn api_protocol_for_routes_per_model() {
        let _g = lock();
        let mut cat = ModelsDevCatalog::new();
        cat.insert(
            "opencode".into(),
            ModelsDevProvider {
                npm: "@ai-sdk/openai-compatible".into(),
                models: HashMap::from([(
                    "muse-spark-1.2-contributor-free".into(),
                    ModelsDevModel {
                        provider_npm: "@ai-sdk/openai".into(),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        );
        cat.insert(
            "opencode-go".into(),
            ModelsDevProvider {
                npm: "@ai-sdk/openai-compatible".into(),
                ..Default::default()
            },
        );
        set_models_dev_catalog_for_test(Some(cat));

        assert_eq!(
            api_protocol_for("opencode", "muse-spark-1.2-contributor-free"),
            APIProtocol::OpenAIResponses,
            "muse-spark on Zen routes to Responses"
        );
        // No override on opencode-go → provider npm wins.
        assert_eq!(
            api_protocol_for("opencode-go", "mimo-v2.5"),
            APIProtocol::ChatCompletions
        );
        // Empty model id → default to ChatCompletions.
        assert_eq!(
            api_protocol_for("opencode", ""),
            APIProtocol::ChatCompletions
        );
    }

    /// api_protocol_for returns ChatCompletions when the catalog is
    /// empty (e.g. before the first fetch_models_dev_catalog call
    /// completes, or when offline). Unknown custom models must keep
    /// the legacy /chat/completions path so dispatch doesn't suddenly
    /// error mid-session.
    #[test]
    fn api_protocol_for_empty_catalog_defaults_to_chat() {
        let _g = lock();
        set_models_dev_catalog_for_test(None);
        assert_eq!(
            api_protocol_for("opencode", "muse-spark-1.2-contributor-free"),
            APIProtocol::ChatCompletions
        );
    }
}
