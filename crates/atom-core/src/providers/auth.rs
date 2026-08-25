//! Auth store for atom providers. Credentials live in
//! ~/.local/share/atom/auth.json (or $XDG_DATA_HOME/atom/auth.json),
//! keyed by models.dev provider ID. OpenCode-compatible schema:
//!
//! ```json
//! {
//!   "openrouter": { "type": "api", "key": "sk-..." },
//!   "github-copilot": { "type": "oauth", "access": "...", "refresh": "...", "expires": 1234567890 }
//! }
//! ```
//!
//! type is "api" or "oauth". expires is unix ms or seconds; 0 means no
//! known expiry. Flat files under providers/<id> remain a read fallback.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;

/// authEntry is one stored credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthEntry {
    /// "api" or "oauth"
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh: String,
    /// unix ms or seconds; 0 = no known expiry
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expires: i64,
    // BTreeMap keeps serialized key order stable (Go sorts map keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

pub fn auth_store_path() -> PathBuf {
    crate::session::store::data_dir().join("auth.json")
}

pub fn legacy_provider_key_path(id: &str) -> PathBuf {
    crate::session::store::data_dir().join("providers").join(id)
}

/// loadAuthStore returns the credential map. A missing or corrupt file
/// yields an empty map; it never panics.
pub fn load_auth_store() -> HashMap<String, AuthEntry> {
    let b = match std::fs::read(auth_store_path()) {
        Ok(b) => b,
        Err(_) => return HashMap::new(),
    };
    match serde_json::from_slice::<HashMap<String, AuthEntry>>(&b) {
        Ok(store) => store,
        Err(_) => HashMap::new(),
    }
}

pub fn save_auth_store(store: &HashMap<String, AuthEntry>) -> anyhow::Result<()> {
    std::fs::create_dir_all(crate::session::store::data_dir())?;
    let b = serde_json::to_vec_pretty(store)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(auth_store_path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(&b)?;
    Ok(())
}

pub fn set_auth(id: &str, e: AuthEntry) -> anyhow::Result<()> {
    let mut store = load_auth_store();
    store.insert(id.to_string(), e);
    save_auth_store(&store)
}

pub fn remove_auth(id: &str) -> anyhow::Result<()> {
    let mut store = load_auth_store();
    store.remove(id);
    for alias in auth_ids_for(id) {
        store.remove(&alias);
    }
    save_auth_store(&store)
}

/// authBearer is the token sent as Authorization: Bearer. API entries
/// use Key. OAuth entries use Access, falling back to Refresh (github-copilot
/// style). Expires<=0 is treated as not expired; expired tokens are still
/// returned (this store does not refresh).
pub fn auth_bearer(_id: &str, e: &AuthEntry) -> String {
    match e.r#type.as_str() {
        "oauth" => {
            if !e.access.is_empty() {
                e.access.clone()
            } else {
                e.refresh.clone()
            }
        }
        _ => e.key.clone(),
    }
}

/// authIDsFor returns the store keys to try for a provider, including
/// builtin aliases (ollama-cloud <-> ollama, opencode <-> opencode-zen).
pub fn auth_ids_for(id: &str) -> Vec<String> {
    let mut ids = vec![id.to_string()];
    match id {
        "ollama" => ids.push("ollama-cloud".into()),
        "ollama-cloud" => ids.push("ollama".into()),
        "opencode-zen" => ids.push("opencode".into()),
        "opencode" => ids.push("opencode-zen".into()),
        _ => {}
    }
    ids
}

pub fn lookup_auth_entry(id: &str) -> Option<AuthEntry> {
    let store = load_auth_store();
    for k in auth_ids_for(id) {
        if let Some(e) = store.get(&k) {
            return Some(e.clone());
        }
    }
    None
}

/// legacyProviderKey reads the old flat-file key at providers/<id>.
pub fn legacy_provider_key(id: &str) -> String {
    for k in auth_ids_for(id) {
        let b = match std::fs::read(legacy_provider_key_path(&k)) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let s = String::from_utf8_lossy(&b).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

pub fn remove_legacy_provider_key(id: &str) {
    for k in auth_ids_for(id) {
        let _ = std::fs::remove_file(legacy_provider_key_path(&k));
    }
}

/// loadProviderKey returns a Bearer token for the provider. auth.json
/// wins over the legacy providers/<id> file.
pub async fn load_provider_key(id: &str) -> String {
    if id == "openai" {
        if let Some(e) = lookup_auth_entry("openai") {
            if e.r#type == "oauth" {
                if let Ok(live) = super::oauth::ensure_openai_auth().await {
                    let tok = auth_bearer("openai", &live);
                    if !tok.is_empty() {
                        return tok;
                    }
                }
            }
        }
    }
    if let Some(e) = lookup_auth_entry(id) {
        let tok = auth_bearer(id, &e);
        if !tok.is_empty() {
            return tok;
        }
    }
    legacy_provider_key(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_store_round_trip_api_and_oauth() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-roundtrip");

        let api = AuthEntry {
            r#type: "api".into(),
            key: "sk-test".into(),
            ..Default::default()
        };
        let oauth = AuthEntry {
            r#type: "oauth".into(),
            access: "acc".into(),
            refresh: "ref".into(),
            ..Default::default()
        };
        set_auth("openrouter", api).unwrap();
        set_auth("github-copilot", oauth).unwrap();

        let store = load_auth_store();
        assert_eq!(store["openrouter"].r#type, "api");
        assert_eq!(store["openrouter"].key, "sk-test");
        assert_eq!(store["github-copilot"].r#type, "oauth");
        assert_eq!(store["github-copilot"].access, "acc");
        assert_eq!(auth_bearer("openrouter", &store["openrouter"]), "sk-test");
        assert_eq!(
            auth_bearer("github-copilot", &store["github-copilot"]),
            "acc"
        );
        let no_access = AuthEntry {
            r#type: "oauth".into(),
            access: String::new(),
            refresh: "ref".into(),
            ..Default::default()
        };
        assert_eq!(auth_bearer("github-copilot", &no_access), "ref");
    }

    #[tokio::test]
    async fn load_provider_key_prefers_auth_json() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-prefers");

        let providers_dir = crate::session::store::data_dir().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        std::fs::write(providers_dir.join("openrouter"), "legacy-key").unwrap();
        set_auth(
            "openrouter",
            AuthEntry {
                r#type: "api".into(),
                key: "auth-key".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_provider_key("openrouter").await, "auth-key");
    }

    #[tokio::test]
    async fn load_provider_key_legacy_fallback() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-legacy");

        let providers_dir = crate::session::store::data_dir().join("providers");
        std::fs::create_dir_all(&providers_dir).unwrap();
        std::fs::write(providers_dir.join("openrouter"), "  legacy-key\n").unwrap();
        assert_eq!(load_provider_key("openrouter").await, "legacy-key");
    }

    #[tokio::test]
    async fn load_provider_key_ollama_cloud_alias() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-alias");

        set_auth(
            "ollama-cloud",
            AuthEntry {
                r#type: "api".into(),
                key: "ollama-sk".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_provider_key("ollama").await, "ollama-sk");
        assert_eq!(load_provider_key("ollama-cloud").await, "ollama-sk");
    }

    #[tokio::test]
    async fn remove_auth_removes_entry() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-remove");

        set_auth(
            "openrouter",
            AuthEntry {
                r#type: "api".into(),
                key: "sk".into(),
                ..Default::default()
            },
        )
        .unwrap();
        remove_auth("openrouter").unwrap();
        assert!(!load_auth_store().contains_key("openrouter"));
        assert_eq!(load_provider_key("openrouter").await, "");
    }

    #[tokio::test]
    async fn load_auth_store_corrupt() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-corrupt");

        std::fs::create_dir_all(crate::session::store::data_dir()).unwrap();
        std::fs::write(auth_store_path(), "{not json").unwrap();
        assert!(load_auth_store().is_empty());
    }

    #[tokio::test]
    async fn load_auth_store_missing() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-missing");

        assert!(load_auth_store().is_empty());
    }

    #[tokio::test]
    async fn save_auth_store_mode_0600() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-mode");

        let mut store = HashMap::new();
        store.insert(
            "openrouter".to_string(),
            AuthEntry {
                r#type: "api".into(),
                key: "sk".into(),
                ..Default::default()
            },
        );
        save_auth_store(&store).unwrap();
        let meta = std::fs::metadata(auth_store_path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        let decoded: HashMap<String, AuthEntry> =
            serde_json::from_slice(&std::fs::read(auth_store_path()).unwrap()).unwrap();
        assert_eq!(decoded["openrouter"].key, "sk");
    }

    #[tokio::test]
    async fn oauth_bearer_empty_access_uses_refresh() {
        let _g = crate::providers::test_lock();
        let _d = crate::providers::isolate_data_dir("auth-refresh-only");

        set_auth(
            "github-copilot",
            AuthEntry {
                r#type: "oauth".into(),
                refresh: "refresh-only".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_provider_key("github-copilot").await, "refresh-only");
    }

    #[test]
    fn auth_ids_aliases() {
        assert_eq!(auth_ids_for("ollama"), vec!["ollama", "ollama-cloud"]);
        assert_eq!(auth_ids_for("opencode"), vec!["opencode", "opencode-zen"]);
        assert_eq!(auth_ids_for("openrouter"), vec!["openrouter"]);
    }

    #[test]
    fn entry_json_matches_go_tags() {
        let e = AuthEntry {
            r#type: "oauth".into(),
            access: "a".into(),
            refresh: "r".into(),
            expires: 1234567890,
            ..Default::default()
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "oauth");
        assert!(v.get("key").is_none());
        assert_eq!(v["expires"], 1234567890);
    }
}
