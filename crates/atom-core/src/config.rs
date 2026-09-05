//! User-editable, non-secret Atom configuration.
//!
//! Preferences live in `$XDG_CONFIG_HOME/atom/config.json` (falling back
//! to `~/.config/atom/config.json`). Credentials deliberately live in the
//! provider auth store or environment variables instead.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_COMPACTION_PROVIDER: &str = "opencode-zen";
pub const DEFAULT_COMPACTION_MODEL: &str = "mimo-v2.5-free";
pub const DEFAULT_WEB_SEARCH_SERVER: &str = "tinyfish";
pub const DEFAULT_WEB_FETCH_SERVER: &str = "tinyfish";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    /// `None` means auto-compaction is enabled (the default). Set to
    /// `false` to disable folding the conversation when it nears the
    /// context limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl CompactionConfig {
    pub fn resolved_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Bundled profile id or an MCP server name from mcp.json.
    #[serde(default)]
    pub server: String,
    /// MCP tool name. Bundled profiles fill their conventional tool when
    /// this is omitted.
    #[serde(default)]
    pub tool: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchConfig {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomConfig {
    #[serde(default = "config_version")]
    pub version: u32,
    /// `None` is meaningful: startup can offer setup without making a
    /// missing config file prevent Atom from launching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<WebFetchConfig>,
    /// `None` means auto-update is enabled (the default). Set to `false`
    /// to disable the startup auto-updater.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    /// Selected theme id (file stem of a `ui/themes/*.json` built-in or
    /// a user theme in the config `themes/` directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// `None` means the chat viewport paints the theme background like
    /// every other surface (the default). `Some(true)` renders the
    /// conversation viewport on the terminal's default background
    /// (transparent when the terminal profile has no background color);
    /// all other surfaces keep the theme background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparent_background: Option<bool>,
}

const fn config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for AtomConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            compaction: None,
            web_search: None,
            web_fetch: None,
            auto_update: None,
            theme: None,
            transparent_background: None,
        }
    }
}

impl AtomConfig {
    /// Whether the startup auto-updater is enabled. `None` (the default)
    /// and `Some(true)` both enable it; only an explicit `false` disables.
    pub fn resolved_auto_update(&self) -> bool {
        self.auto_update.unwrap_or(true)
    }

    pub fn resolved_compaction(&self) -> CompactionConfig {
        let mut value = self.compaction.clone().unwrap_or_default();
        if value.provider.trim().is_empty() {
            value.provider = DEFAULT_COMPACTION_PROVIDER.into();
        }
        if value.model.trim().is_empty() {
            value.model = DEFAULT_COMPACTION_MODEL.into();
        }
        value
    }

    pub fn resolved_web_search(&self) -> WebSearchConfig {
        let mut value = self.web_search.clone().unwrap_or_default();
        if value.server.trim().is_empty() {
            value.server = DEFAULT_WEB_SEARCH_SERVER.into();
        }
        if value.tool.trim().is_empty() {
            value.tool = bundled_web_search_profile(&value.server)
                .map(|p| p.tool)
                .unwrap_or_default();
        }
        value
    }

    pub fn resolved_web_fetch(&self) -> WebFetchConfig {
        let mut value = self.web_fetch.clone().unwrap_or_default();
        if value.server.trim().is_empty() {
            value.server = DEFAULT_WEB_FETCH_SERVER.into();
        }
        if value.tool.trim().is_empty() {
            value.tool = bundled_web_fetch_profile(&value.server)
                .map(|p| p.tool)
                .unwrap_or_default();
        }
        value
    }

    /// Whether the chat viewport renders on the terminal's default
    /// background instead of the theme background. Off by default.
    pub fn resolved_transparent_background(&self) -> bool {
        self.transparent_background.unwrap_or(false)
    }

    pub fn setup_complete(&self) -> bool {
        let compaction_complete = self
            .compaction
            .as_ref()
            .is_some_and(|c| !c.provider.trim().is_empty() && !c.model.trim().is_empty());
        let web_search_complete = self.web_search.as_ref().is_some_and(|w| {
            let server = w.server.trim();
            !server.is_empty()
                && (bundled_web_search_profile(server).is_some() || !w.tool.trim().is_empty())
        });
        compaction_complete && web_search_complete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchProfile {
    pub id: String,
    pub name: String,
    pub url: String,
    pub tool: String,
    pub query_argument: String,
    pub auth: WebSearchAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchAuth {
    Optional,
    Required,
}

pub fn bundled_web_search_profiles() -> Vec<WebSearchProfile> {
    vec![
        WebSearchProfile {
            id: "parallel".into(),
            name: "Parallel Web Search".into(),
            url: "https://search.parallel.ai/mcp".into(),
            tool: "web_search".into(),
            query_argument: "search_queries".into(),
            auth: WebSearchAuth::Optional,
        },
        WebSearchProfile {
            id: "exa".into(),
            name: "Exa Web Search".into(),
            url: "https://mcp.exa.ai/mcp?tools=web_search_exa".into(),
            tool: "web_search_exa".into(),
            query_argument: "query".into(),
            auth: WebSearchAuth::Optional,
        },
        // TinyFish requires an API key on every call (free at any
        // wallet balance, but never unauthenticated); it exposes REST
        // only, so keyless runs skip straight to the next provider.
        WebSearchProfile {
            id: "tinyfish".into(),
            name: "TinyFish Web Search".into(),
            url: "https://api.search.tinyfish.ai".into(),
            tool: "web_search".into(),
            query_argument: "query".into(),
            auth: WebSearchAuth::Required,
        },
        // Ollama does not publish a hosted MCP endpoint. atom-tools
        // exposes it through the same selected-capability boundary using
        // the official REST API as a bundled compatibility adapter.
        WebSearchProfile {
            id: "ollama".into(),
            name: "Ollama Cloud Web Search".into(),
            url: String::new(),
            tool: "web_search".into(),
            query_argument: "query".into(),
            auth: WebSearchAuth::Required,
        },
    ]
}

pub fn bundled_web_search_profile(id: &str) -> Option<WebSearchProfile> {
    bundled_web_search_profiles()
        .into_iter()
        .find(|profile| profile.id == id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchProfile {
    pub id: String,
    pub name: String,
    /// REST endpoint (keyed; metered per call where noted).
    pub url: String,
    pub tool: String,
    pub auth: WebSearchAuth,
    /// Hosted MCP endpoint for the keyless route, if the provider
    /// publishes one (parallel and exa). Empty = no MCP route.
    pub mcp_url: String,
    /// The MCP tool served at `mcp_url` (parallel: `web_fetch`,
    /// exa: `web_fetch_exa`). Empty when there is no MCP route.
    pub mcp_tool: String,
}

pub fn bundled_web_fetch_profiles() -> Vec<WebFetchProfile> {
    vec![
        WebFetchProfile {
            id: "parallel".into(),
            name: "Parallel Web Fetch".into(),
            url: "https://api.parallel.ai/v1/extract".into(),
            tool: "web_fetch".into(),
            auth: WebSearchAuth::Optional,
            mcp_url: "https://search.parallel.ai/mcp".into(),
            mcp_tool: "web_fetch".into(),
        },
        // Ordered free -> paid in the picker: parallel and exa serve
        // keyless hosted-MCP fetch routes; tinyfish and ollama need API
        // keys on every call.
        WebFetchProfile {
            id: "exa".into(),
            name: "Exa Web Fetch".into(),
            url: "https://api.exa.ai/contents".into(),
            tool: "web_fetch".into(),
            auth: WebSearchAuth::Optional,
            mcp_url: "https://mcp.exa.ai/mcp?tools=web_fetch_exa".into(),
            mcp_tool: "web_fetch_exa".into(),
        },
        WebFetchProfile {
            id: "tinyfish".into(),
            name: "TinyFish Web Fetch".into(),
            url: "https://api.fetch.tinyfish.ai".into(),
            tool: "web_fetch".into(),
            auth: WebSearchAuth::Required,
            mcp_url: String::new(),
            mcp_tool: String::new(),
        },
        WebFetchProfile {
            id: "ollama".into(),
            name: "Ollama Cloud Web Fetch".into(),
            url: "https://ollama.com/api/web_fetch".into(),
            tool: "web_fetch".into(),
            auth: WebSearchAuth::Required,
            mcp_url: String::new(),
            mcp_tool: String::new(),
        },
    ]
}

pub fn bundled_web_fetch_profile(id: &str) -> Option<WebFetchProfile> {
    bundled_web_fetch_profiles()
        .into_iter()
        .find(|profile| profile.id == id)
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join(crate::build::dir_leaf());
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join(crate::build::dir_leaf())
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn load() -> AtomConfig {
    load_from(&config_path()).unwrap_or_default()
}

pub fn load_from(path: &Path) -> std::io::Result<AtomConfig> {
    let bytes = std::fs::read(path)?;
    let mut config: AtomConfig = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Unknown future versions are read permissively because all fields are
    // additive and optional. Writes always use the schema this binary knows.
    config.version = CONFIG_VERSION;
    Ok(config)
}

pub fn save(config: &AtomConfig) -> std::io::Result<()> {
    save_to(&config_path(), config)
}

pub fn save_to(path: &Path, config: &AtomConfig) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no parent",
        ));
    };
    std::fs::create_dir_all(parent)?;
    let mut value = config.clone();
    value.version = CONFIG_VERSION;
    let bytes = serde_json::to_vec_pretty(&value)?;
    let tmp = parent.join(format!(
        ".config.json.tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // Best effort: persist the directory entry as well as the file
        // contents on platforms that support syncing directories.
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_resolve_without_becoming_configured() {
        let config = AtomConfig::default();
        assert!(!config.setup_complete());
        assert!(config.resolved_web_search().server == DEFAULT_WEB_SEARCH_SERVER);
        assert_eq!(config.resolved_compaction().model, DEFAULT_COMPACTION_MODEL);
    }

    #[test]
    fn compaction_enabled_defaults_on_and_disables_explicitly() {
        assert!(CompactionConfig::default().resolved_enabled());
        assert!(AtomConfig::default()
            .resolved_compaction()
            .resolved_enabled());
        assert!(!CompactionConfig {
            enabled: Some(false),
            ..Default::default()
        }
        .resolved_enabled());
    }

    #[test]
    fn transparent_background_defaults_off_and_toggles_explicitly() {
        // `None` (unset) and `Some(false)` both keep the viewport opaque;
        // only an explicit `Some(true)` switches to the terminal default.
        assert!(!AtomConfig::default().resolved_transparent_background());
        assert!(!AtomConfig {
            transparent_background: Some(false),
            ..Default::default()
        }
        .resolved_transparent_background());
        assert!(AtomConfig {
            transparent_background: Some(true),
            ..Default::default()
        }
        .resolved_transparent_background());
    }

    #[test]
    fn tinyfish_is_default_for_search_and_fetch() {
        assert_eq!(DEFAULT_WEB_SEARCH_SERVER, "tinyfish");
        assert_eq!(DEFAULT_WEB_FETCH_SERVER, "tinyfish");
        let config = AtomConfig::default();
        assert_eq!(config.resolved_web_search().server, "tinyfish");
        assert_eq!(config.resolved_web_fetch().server, "tinyfish");
    }

    #[test]
    fn bundled_fetch_profiles_are_stable() {
        assert!(bundled_web_fetch_profile("tinyfish").is_some());
        assert!(bundled_web_fetch_profile("parallel").is_some());
        assert!(bundled_web_fetch_profile("exa").is_some());
        assert!(bundled_web_fetch_profile("ollama").is_some());
        // exa serves a keyless hosted-MCP fetch route (web_fetch_exa);
        // tinyfish is REST-only and always requires an API key.
        assert_eq!(
            bundled_web_fetch_profile("exa").unwrap().auth,
            WebSearchAuth::Optional
        );
        assert_eq!(
            bundled_web_fetch_profile("tinyfish").unwrap().auth,
            WebSearchAuth::Required
        );
        for id in ["parallel", "exa"] {
            let p = bundled_web_fetch_profile(id).unwrap();
            assert!(!p.mcp_url.is_empty() && !p.mcp_tool.is_empty(), "{id}");
        }
        for id in ["tinyfish", "ollama"] {
            assert!(bundled_web_fetch_profile(id).unwrap().mcp_url.is_empty());
        }
        // parallel is the first (free, keyless) fallback in both
        // bundled orderings.
        assert_eq!(bundled_web_fetch_profiles()[0].id, "parallel");
        assert_eq!(bundled_web_search_profiles()[0].id, "parallel");
    }

    #[test]
    fn custom_web_search_requires_an_explicit_tool() {
        let mut config = AtomConfig {
            compaction: Some(CompactionConfig {
                provider: DEFAULT_COMPACTION_PROVIDER.into(),
                model: DEFAULT_COMPACTION_MODEL.into(),
                ..Default::default()
            }),
            web_search: Some(WebSearchConfig {
                server: "custom".into(),
                tool: String::new(),
            }),
            ..AtomConfig::default()
        };
        assert!(!config.setup_complete());
        assert!(config.resolved_web_search().tool.is_empty());

        config.web_search.as_mut().unwrap().tool = "search".into();
        assert!(config.setup_complete());
    }

    #[test]
    fn malformed_config_falls_back_only_at_the_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{not json").unwrap();
        let error = load_from(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn round_trip_and_replace_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.json");
        let config = AtomConfig {
            compaction: Some(CompactionConfig {
                provider: "openai".into(),
                model: "gpt-5".into(),
                ..Default::default()
            }),
            web_search: Some(WebSearchConfig {
                server: "exa".into(),
                tool: "web_search_exa".into(),
            }),
            ..AtomConfig::default()
        };
        save_to(&path, &config).unwrap();
        assert_eq!(load_from(&path).unwrap(), config);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn bundled_profiles_are_stable() {
        assert_eq!(
            bundled_web_search_profile("parallel").unwrap().tool,
            "web_search"
        );
        assert_eq!(
            bundled_web_search_profile("exa").unwrap().tool,
            "web_search_exa"
        );
        assert_eq!(
            bundled_web_search_profile("ollama").unwrap().auth,
            WebSearchAuth::Required
        );
    }
}
