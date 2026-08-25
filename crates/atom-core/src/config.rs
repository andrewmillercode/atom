//! User-editable, non-secret Atom configuration.
//!
//! Preferences live in `$XDG_CONFIG_HOME/atom/config.json` (falling back
//! to `~/.config/atom/config.json`). Credentials deliberately live in the
//! provider auth store or environment variables instead.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_COMPACTION_PROVIDER: &str = "ollama-local";
pub const DEFAULT_COMPACTION_MODEL: &str = "deepseek-v4-flash:0731";
pub const DEFAULT_WEB_SEARCH_SERVER: &str = "parallel";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
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
        }
    }
}

impl AtomConfig {
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
        // Ollama does not publish a hosted MCP endpoint. atom-tools
        // exposes it through the same selected-capability boundary using
        // the official REST API as a bundled compatibility adapter.
        WebSearchProfile {
            id: "ollama".into(),
            name: "Ollama Web Search".into(),
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

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("atom");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("atom")
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
        assert_eq!(config.resolved_web_search().server, "parallel");
        assert_eq!(config.resolved_compaction().model, DEFAULT_COMPACTION_MODEL);
    }

    #[test]
    fn custom_web_search_requires_an_explicit_tool() {
        let mut config = AtomConfig::default();
        config.compaction = Some(config.resolved_compaction());
        config.web_search = Some(WebSearchConfig {
            server: "custom".into(),
            tool: String::new(),
        });
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
        let mut config = AtomConfig::default();
        config.compaction = Some(CompactionConfig {
            provider: "openai".into(),
            model: "gpt-5".into(),
        });
        config.web_search = Some(WebSearchConfig {
            server: "exa".into(),
            tool: "web_search_exa".into(),
        });
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
