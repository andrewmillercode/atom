//! Sandbox configuration persisted at `dataDir()/sandbox.json`.
//!
//! All fields carry serde defaults so a missing or partial file loads as
//! the documented defaults (Workspace mode, network denied).

use atom_core::session::store::data_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How aggressively commands are confined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxMode {
    /// No gate, no confinement; audit only.
    #[serde(rename = "off")]
    Off,
    /// Deny-by-default kernel sandbox with the workspace writable.
    #[default]
    #[serde(rename = "workspace")]
    Workspace,
    /// Like Workspace but path escapes are denied outright instead of
    /// prompting.
    #[serde(rename = "strict")]
    Strict,
}

/// Kernel-level stance on outbound network access from confined commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetPolicy {
    /// `(deny network*)` in the profile — connections fail fast.
    #[default]
    #[serde(rename = "deny")]
    Deny,
    /// Profile denies network; a rule match prompts before running.
    #[serde(rename = "ask")]
    Ask,
    /// Network allowed inside the profile (unix sockets still denied).
    #[serde(rename = "allow")]
    Allow,
}

/// User-editable sandbox settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    #[serde(default)]
    pub network: NetPolicy,
    /// Additional directory subtrees the profile may write.
    #[serde(default)]
    pub extra_writable: Vec<PathBuf>,
    /// Additional read-only roots surfaced to tooling (informational).
    #[serde(default)]
    pub extra_readonly: Vec<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            mode: SandboxMode::Workspace,
            network: NetPolicy::Deny,
            extra_writable: Vec::new(),
            extra_readonly: Vec::new(),
        }
    }
}

impl SandboxConfig {
    /// Path of the persisted config: dataDir()/sandbox.json.
    pub fn path() -> PathBuf {
        data_dir().join("sandbox.json")
    }

    /// Loads config from dataDir(), falling back to defaults when the file
    /// is missing or unparsable.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persists config to dataDir()/sandbox.json.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// True when the profile should permit outbound network.
    pub fn net_allowed(&self) -> bool {
        matches!(self.network, NetPolicy::Allow)
    }

    /// True when path escapes are denied instead of prompting.
    pub fn strict(&self) -> bool {
        matches!(self.mode, SandboxMode::Strict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SandboxConfig::load_from(&dir.path().join("nope.json"));
        assert_eq!(cfg, SandboxConfig::default());
        assert_eq!(cfg.mode, SandboxMode::Workspace);
        assert_eq!(cfg.network, NetPolicy::Deny);
    }

    #[test]
    fn partial_json_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        std::fs::write(&p, br#"{"mode":"strict"}"#).unwrap();
        let cfg = SandboxConfig::load_from(&p);
        assert_eq!(cfg.mode, SandboxMode::Strict);
        assert_eq!(cfg.network, NetPolicy::Deny);
        assert!(cfg.extra_writable.is_empty());
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(SandboxConfig::load_from(&p), SandboxConfig::default());
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("sandbox.json");
        let cfg = SandboxConfig {
            mode: SandboxMode::Strict,
            network: NetPolicy::Allow,
            extra_writable: vec![PathBuf::from("/tmp/build-cache")],
            extra_readonly: vec![PathBuf::from("/opt/refs")],
        };
        cfg.save_to(&p).unwrap();
        assert_eq!(SandboxConfig::load_from(&p), cfg);
    }
}
