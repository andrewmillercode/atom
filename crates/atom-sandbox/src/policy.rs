//! Sandbox configuration persisted at `dataDir()/sandbox.json`.
//!
//! v2 schema (`docs/sandbox-v2.md`):
//!
//! ```json
//! {
//!   "version": 2,
//!   "rules": { "allow": ["cargo test *"], "deny": ["rm * ~"] }
//! }
//! ```
//!
//! Rules are command-prefix globs. An `allow` rule promotes the matching
//! command family into the silent allowlist (Tier 1); a `deny` rule keeps
//! the family in Tier 2 but prefixes the prompt reason with the rule name
//! so the user understands why the family keeps asking. The static rule
//! table still drives the initial verdict; config rules only edit what
//! the user has explicitly approved or rejected.
//!
//! v1 fields (`mode`, `network`, `extra_writable`, `extra_readonly`) are
//! dropped on load — see [`SandboxConfig::load_from`].

use atom_core::session::store::data_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current schema version.
pub const VERSION: u32 = 2;

/// User-editable sandbox settings (v2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub rules: Rules,
    /// Path the config was loaded from (or the most-recent save
    /// target). Persisted calls use this so tests that pass a temp
    /// dir don't end up writing to the real data dir. Skipped from
    /// JSON so the on-disk format stays clean.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

fn default_version() -> u32 {
    VERSION
}

/// User-maintained prefix rules. Order is not significant: an `allow` rule
/// is consulted before a `deny` rule, and neither beats a guardrail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            version: VERSION,
            rules: Rules::default(),
            path: None,
        }
    }
}

impl SandboxConfig {
    /// Path of the persisted config: dataDir()/sandbox.json.
    pub fn path() -> PathBuf {
        data_dir().join("sandbox.json")
    }

    /// Override the on-disk path. Useful when callers (notably the
    /// sandbox exec pipeline) need to persist at a non-default
    /// location — production uses the default; tests use a tempdir.
    pub fn with_path(mut self, path: PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// Effective save target: an explicit path if one was set,
    /// otherwise the default dataDir()/sandbox.json.
    pub fn save_path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(Self::path)
    }

    /// Loads config from dataDir(), falling back to defaults when the
    /// file is missing or unparsable. v1 fields migrate transparently.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        let mut cfg = match std::fs::read(path) {
            Ok(b) => Self::parse_or_migrate(&b, path),
            Err(_) => Self::default().with_path(path.to_path_buf()),
        };
        cfg.path = Some(path.to_path_buf());
        cfg
    }

    fn parse_or_migrate(bytes: &[u8], path: &Path) -> Self {
        // Detect "v1" by absence of the `version` field — the v2
        // default would mask real v1 files otherwise.
        let raw: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let is_v1 = !raw.get("version").is_some()
            && (raw.get("mode").is_some()
                || raw.get("network").is_some()
                || raw.get("extra_writable").is_some()
                || raw.get("extra_readonly").is_some());
        if is_v1 {
            // Drop v1 fields, seed the deny list from approvals.json.
            let mut cfg = Self::default();
            let approvals_path = path.with_file_name("approvals.json");
            if let Ok(raw) = std::fs::read(&approvals_path) {
                if let Ok(keys) = serde_json::from_slice::<Vec<String>>(&raw) {
                    for key in keys {
                        if let Some(rule_id) = key.split('\u{1f}').next() {
                            if !rule_id.is_empty()
                                && rule_id != "unknown-command"
                                && !cfg.rules.deny.iter().any(|r| r == rule_id)
                            {
                                cfg.rules.deny.push(rule_id.to_string());
                            }
                        }
                    }
                }
            }
            let _ = cfg.save_to(path);
            let _ = std::fs::remove_file(approvals_path);
            return cfg;
        }
        // v2 path.
        if let Ok(cfg) = serde_json::from_slice::<SandboxConfig>(bytes) {
            if cfg.version == VERSION {
                return cfg;
            }
        }
        // Garbled file: defaults.
        Self::default()
    }

    /// Persists config to the path it was loaded from (or
    /// dataDir()/sandbox.json as a fallback). Writers coordinate via
    /// the lockfile at dataDir()/sandbox.json.lock so concurrent
    /// processes don't drop each other's rules.
    pub fn save(&self) -> anyhow::Result<()> {
        let p = self.save_path();
        self.save_to(&p)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        Self::write_atomic(path, &json)?;
        Ok(())
    }

    fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".sandbox.json.")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        use std::io::Write;
        tmp.write_all(bytes)?;
        tmp.flush()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Merge a freshly-decided rule into the config and persist. Dedupes
    /// against existing rules and shadows opposite-side duplicates
    /// (an allow shadows a same-text deny, and vice versa). Saves to
    /// the config's [`SandboxConfig::save_path`].
    pub fn add_rule(&mut self, kind: RuleKind, rule: &str) -> anyhow::Result<()> {
        let trimmed = rule.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let target = match kind {
            RuleKind::Allow => &mut self.rules.allow,
            RuleKind::Deny => &mut self.rules.deny,
        };
        if !target.iter().any(|r| r == trimmed) {
            target.push(trimmed.to_string());
            target.sort();
        }
        let other = match kind {
            RuleKind::Allow => &mut self.rules.deny,
            RuleKind::Deny => &mut self.rules.allow,
        };
        other.retain(|r| r != trimmed);
        self.save()
    }

    /// Remove a rule by exact match. No-op if absent.
    pub fn remove_rule(&mut self, kind: RuleKind, rule: &str) -> anyhow::Result<()> {
        let target = match kind {
            RuleKind::Allow => &mut self.rules.allow,
            RuleKind::Deny => &mut self.rules.deny,
        };
        target.retain(|r| r != rule);
        self.save()
    }

    /// Consult the user's rules for `command`. Returns the strongest
    /// match: Allow > Deny. An Allow hit promotes the command to Tier 1
    /// without prompting; a Deny hit keeps it in Tier 2 but exposes
    /// the rule id as the prompt reason.
    pub fn classify(&self, command: &str) -> Option<RuleMatch> {
        for rule in &self.rules.allow {
            if matches_rule(rule, command) {
                return Some(RuleMatch::Allow(rule.clone()));
            }
        }
        for rule in &self.rules.deny {
            if matches_rule(rule, command) {
                return Some(RuleMatch::Deny(rule.clone()));
            }
        }
        None
    }
}

/// Which side of [`Rules`] a user decision writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Allow,
    Deny,
}

/// Result of consulting the user's rules for a single command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleMatch {
    Allow(String),
    Deny(String),
}

/// True when `rule` (a user-editable prefix glob) covers `command`.
///
/// Rule shapes:
/// - exact: `cargo test` matches exactly `cargo test`
/// - token-prefix: `cargo test *` matches any command starting with
///   the literal whitespace-tokenized tokens `cargo test`
/// - token-suffix: `* ~` matches any command whose final token is `~`
/// - both: `rm * ~` matches commands starting with `rm` and ending
///   with `~` (the middle is unconstrained)
/// - bare wildcard: `*` matches anything
pub fn matches_rule(rule: &str, command: &str) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    if rule == "*" {
        return true;
    }
    let rule_tokens: Vec<&str> = rule.split_whitespace().collect();
    let cmd_tokens: Vec<&str> = command.split_whitespace().collect();
    let star_count = rule_tokens.iter().filter(|t| **t == "*").count();
    if star_count == 0 {
        return cmd_tokens == rule_tokens;
    }
    if star_count > 1 {
        // Conservative: a single wildcard token is enough expressivity
        // for the user-facing rule table; multi-wildcard rules fall
        // back to exact match.
        return cmd_tokens == rule_tokens;
    }
    let star_idx = rule_tokens.iter().position(|t| *t == "*").unwrap();
    let (prefix, suffix) = rule_tokens.split_at(star_idx);
    let suffix = &suffix[1..];
    if cmd_tokens.len() < prefix.len() + suffix.len() {
        return false;
    }
    let starts = cmd_tokens[..prefix.len()].iter().eq(prefix.iter());
    let ends = cmd_tokens[cmd_tokens.len() - suffix.len()..]
        .iter()
        .eq(suffix.iter());
    starts && ends
}

/// Build a prefix rule for `[a] accept-all`. The wildcard lands after
/// the first non-flag token past argv0, capped at one word for
/// dangerous heads (`rm`, `sudo`, `chmod -R`, network-to-interpreter
/// shapes) so `[a]` on a dangerous command can't accidentally create a
/// narrow allow.
pub fn prefix_for_command(cmd: &str) -> String {
    use crate::rules::{dangerous_heads, tokenize_for_prefix};
    let toks = tokenize_for_prefix(cmd);
    if toks.is_empty() {
        return "*".to_string();
    }
    let head = toks[0].to_lowercase();
    let cap = if dangerous_heads(&head) { 1 } else { 2 };
    let take = toks.len().min(cap);
    let prefix: Vec<&str> = toks[..take].iter().map(String::as_str).collect();
    let mut out = prefix.join(" ");
    out.push(' ');
    out.push('*');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SandboxConfig::load_from(&dir.path().join("nope.json"));
        assert_eq!(cfg.rules, Rules::default());
        assert_eq!(cfg.version, VERSION);
    }

    #[test]
    fn v2_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        let cfg = SandboxConfig {
            version: VERSION,
            rules: Rules {
                allow: vec!["cargo test *".into()],
                deny: vec!["rm *".into()],
            },
            path: Some(p.clone()),
        };
        cfg.save_to(&p).unwrap();
        assert_eq!(SandboxConfig::load_from(&p), cfg);
    }

    #[test]
    fn partial_v2_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        std::fs::write(&p, br#"{"rules":{"allow":["ls *"]}}"#).unwrap();
        let cfg = SandboxConfig::load_from(&p);
        assert_eq!(cfg.version, VERSION);
        assert_eq!(cfg.rules.allow, vec!["ls *".to_string()]);
        assert!(cfg.rules.deny.is_empty());
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        std::fs::write(&p, b"{not json").unwrap();
        let cfg = SandboxConfig::load_from(&p);
        assert_eq!(cfg.rules, Rules::default());
    }

    #[test]
    fn v1_drops_old_fields_and_migrates_approvals() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("sandbox.json");
        let appr_path = dir.path().join("approvals.json");
        std::fs::write(
            &cfg_path,
            br#"{"mode":"workspace","network":"deny","extra_writable":["/tmp"]}"#,
        )
        .unwrap();
        // v1 keys were `rule_id\x1f/cwd`; serde_json serializes the
        // 0x1F unit separator as `\u001f`, so the on-disk file used
        // that escape.
        std::fs::write(
            &appr_path,
            br#"["curl\u001f/ws","unknown-command\u001f/ws"]"#,
        )
        .unwrap();
        let cfg = SandboxConfig::load_from(&cfg_path);
        assert_eq!(cfg.version, VERSION);
        assert_eq!(cfg.rules.deny, vec!["curl".to_string()]);
        assert!(cfg.rules.allow.is_empty());
        assert!(!appr_path.exists());
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(on_disk.contains("\"version\": 2"));
    }

    #[test]
    fn add_rule_dedupes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        // Use load_from so the config knows about its on-disk path
        // via subsequent save() calls; tests that want a controlled
        // path use save_to explicitly.
        let mut cfg = SandboxConfig::default();
        cfg.add_rule(RuleKind::Allow, "cargo test *").unwrap();
        cfg.add_rule(RuleKind::Allow, "cargo test *").unwrap();
        cfg.add_rule(RuleKind::Allow, "cargo build *").unwrap();
        assert_eq!(cfg.rules.allow.len(), 2);
        cfg.save_to(&p).unwrap();
        let on_disk = SandboxConfig::load_from(&p);
        assert_eq!(on_disk.rules.allow.len(), 2);
    }

    #[test]
    fn add_allow_overrides_existing_deny() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sandbox.json");
        let mut cfg = SandboxConfig::default().with_path(p.clone());
        cfg.add_rule(RuleKind::Deny, "cargo test *").unwrap();
        cfg.add_rule(RuleKind::Allow, "cargo test *").unwrap();
        assert!(cfg.rules.allow.contains(&"cargo test *".to_string()));
        assert!(!cfg.rules.deny.contains(&"cargo test *".to_string()));
        let on_disk = SandboxConfig::load_from(&p);
        assert_eq!(on_disk.rules, cfg.rules);
    }

    #[test]
    fn prefix_match_shapes() {
        assert!(matches_rule("cargo test", "cargo test"));
        assert!(!matches_rule("cargo test", "cargo tests"));
        assert!(matches_rule("cargo test *", "cargo test --release"));
        assert!(matches_rule("cargo test *", "cargo test -p atom --release"));
        assert!(!matches_rule("cargo test *", "cargo build"));
        assert!(!matches_rule("cargo test *", "cargo"));
        // `rm*` (no space) is not a supported rule shape in v2; users
        // must write `rm *` to match all rm invocations. The
        // whitespace-tokenized prefix matches `rm` followed by at
        // least one more token, so `rmdir` (which starts with `rm` as
        // a string but not as a token) does NOT match `rm *`.
        assert!(!matches_rule("rm*", "rm -rf /tmp"));
        assert!(matches_rule("rm *", "rm -rf /tmp"));
        assert!(!matches_rule("rm *", "rmdir /tmp"));
        assert!(!matches_rule("", "anything"));
    }

    #[test]
    fn classify_returns_strongest_match() {
        let cfg = SandboxConfig {
            version: VERSION,
            rules: Rules {
                allow: vec!["cargo test *".into()],
                deny: vec!["rm *".into()],
            },
            path: None,
        };
        assert_eq!(
            cfg.classify("cargo test --release"),
            Some(RuleMatch::Allow("cargo test *".into()))
        );
        assert_eq!(
            cfg.classify("rm -rf /tmp/x"),
            Some(RuleMatch::Deny("rm *".into()))
        );
        assert_eq!(cfg.classify("ls -la"), None);
    }

    #[test]
    fn rule_wildcard_token_handles_middle_and_suffix() {
        // prefix-only wildcard
        assert!(matches_rule("cargo test *", "cargo test --release"));
        assert!(!matches_rule("cargo test *", "cargo build"));
        // bare wildcard
        assert!(matches_rule("*", "anything goes"));
        // suffix-only wildcard: last token is `~`
        assert!(matches_rule("* ~", "rm -rf foo ~"));
        assert!(!matches_rule("* ~", "rm -rf ~/foo"));
        // both ends: rm ... ~ (with ~ as the literal final token)
        assert!(matches_rule("rm * ~", "rm foo ~"));
        assert!(!matches_rule("rm * ~", "rm foo /tmp/bar"));
        // exact match
        assert!(matches_rule("ls", "ls"));
        assert!(!matches_rule("ls", "ls -la"));
    }

    #[test]
    fn prefix_for_command_picks_two_words_for_safe_heads() {
        assert_eq!(prefix_for_command("cargo test --release"), "cargo test *");
        assert_eq!(prefix_for_command("git push origin main"), "git push *");
        assert_eq!(
            prefix_for_command("cargo test -p atom --release"),
            "cargo test *"
        );
    }

    #[test]
    fn prefix_for_command_caps_dangerous_heads_at_one_word() {
        assert_eq!(prefix_for_command("rm -rf /tmp/foo"), "rm *");
        assert_eq!(prefix_for_command("sudo make install"), "sudo *");
        assert_eq!(prefix_for_command("chmod -R 777 /tmp"), "chmod *");
    }
}
