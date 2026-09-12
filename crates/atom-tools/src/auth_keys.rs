//! Provider API key resolution: env var > auth.json store > legacy flat file.
//!
//! Both web_search and web_fetch route through this so a paid key saved
//! via auth set works for any provider without code changes.

use atom_core::providers::auth::{auth_bearer, legacy_provider_key, lookup_auth_entry};

fn env_var_for(provider: &str) -> Option<&'static str> {
    match provider {
        "parallel" => Some("PARALLEL_API_KEY"),
        "tinyfish" => Some("TINYFISH_API_KEY"),
        "exa" => Some("EXA_API_KEY"),
        "ollama" => Some("OLLAMA_API_KEY"),
        _ => None,
    }
}

pub fn resolve_provider_key(provider: &str) -> String {
    if let Some(env_var) = env_var_for(provider) {
        if let Ok(key) = std::env::var(env_var) {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Some(entry) = lookup_auth_entry(provider) {
        let token = auth_bearer(provider, &entry);
        if !token.trim().is_empty() {
            return token;
        }
    }
    let legacy = legacy_provider_key(provider);
    if !legacy.trim().is_empty() {
        return legacy;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_lookup_table_covers_all_bundled_providers() {
        for id in ["parallel", "tinyfish", "exa", "ollama"] {
            assert!(
                env_var_for(id).is_some(),
                "missing env var mapping for {id}"
            );
        }
    }

    #[test]
    fn unknown_provider_returns_empty() {
        assert_eq!(resolve_provider_key("nope"), "");
    }
}
