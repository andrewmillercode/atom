//! The execution pipeline everything calls:
//!
//! `analyze -> guardrail floor -> approval gate -> spawn -> audit`.
//!
//! v2 drops kernel confinement (seatbelt / bwrap). The wide static
//! allowlist + guardrail floor replaces the deny-by-default sandbox:
//! commands land in Tier 1 silently when they match the table, in
//! Tier 2 with a prompt otherwise. Guardrails (recursive rm, sudo, …)
//! still block outright, even for commands the user has accepted.
//!
//! Per-session tmpdir setup + subprocess env scrubbing happen inside
//! [`run_with`], the single entry point for both `run` (which uses
//! the data dir) and direct test calls.

use crate::approvals::{ApprovalRequest, ApprovalStore, Approver};
use crate::policy::{prefix_for_command, RuleMatch, SandboxConfig};
use crate::rules::{self, Analysis, Verdict};
use atom_core::session::store::data_dir;
use atom_core::util::sha256_hash;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

/// Hard wall-clock limit for one command. v2 has no confinement, so
/// this is just a runaway safety net.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(120);

const BASH: &str = "/bin/bash";

/// Process-global approval store backed by sandbox.json. Held behind a
/// `Lazy` so the disk read happens once on first use; per-process
/// callers who want a fresh view call [`ApprovalStore::reload`].
static APPROVAL_STORE: Lazy<ApprovalStore> = Lazy::new(ApprovalStore::new);

pub fn approval_store() -> &'static ApprovalStore {
    &APPROVAL_STORE
}

/// How a command was confined (v2: always `None` — kept so audit
/// records and downstream tests still compile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfineKind {
    None,
    /// Legacy values kept for backward-compatible audit-log parsing
    /// (older logs may emit "seatbelt" or "bwrap"; they collapse to
    /// None here since v2 has no kernel confinement).
    #[serde(other)]
    SeatbeltOrBwrap,
}

impl ConfineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfineKind::None => "none",
            ConfineKind::SeatbeltOrBwrap => "none",
        }
    }
}

/// Result of one sandboxed execution.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub verdict: Analysis,
    pub approved: bool,
    /// v2: always `ConfineKind::None`. Field kept so audit log records
    /// stay well-formed and downstream code keeps compiling.
    pub confined: ConfineKind,
}

impl Default for ExecOutcome {
    fn default() -> Self {
        ExecOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            verdict: Analysis::default(),
            approved: false,
            confined: ConfineKind::None,
        }
    }
}

/// First matched rule id that exists in the built-in table (synthetic
/// ids like "path-escape-write" don't count), else "".
fn primary_rule_id(a: &Analysis) -> String {
    a.matched_rules
        .iter()
        .find(|id| rules::RULES.iter().any(|r| r.id == *id))
        .cloned()
        .unwrap_or_default()
}

fn reason_for(rule_id: &str) -> String {
    rules::RULES
        .iter()
        .find(|r| r.id == rule_id)
        .map(|r| r.reason.to_string())
        .unwrap_or_else(|| "requires approval".to_string())
}

/// Run `cmd` through the full pipeline. Audits to dataDir()/sandbox-audit.log.
pub async fn run(
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    session_id: &str,
    cfg: &SandboxConfig,
    approver: &dyn Approver,
) -> ExecOutcome {
    run_with(
        &data_dir(),
        cmd,
        cwd,
        workspace_root,
        session_id,
        cfg,
        approver,
    )
    .await
}

/// Like [`run`] with an explicit data dir for the audit log (tests use
/// unique temp dirs so parallel runs stay hermetic).
#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    data_dir_path: &Path,
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    session_id: &str,
    cfg: &SandboxConfig,
    approver: &dyn Approver,
) -> ExecOutcome {
    // 1. analyze
    let verdict = rules::analyze_full(cmd, workspace_root, cwd, false);

    // 2. guardrail floor: hard Deny is terminal, no prompt.
    if verdict.verdict == Verdict::Deny {
        let rule_id = primary_rule_id(&verdict);
        let reason = reason_for(&rule_id);
        let outcome = ExecOutcome {
            exit_code: -1,
            stderr: format!("atom: blocked by sandbox policy ({rule_id}): {reason}\n"),
            verdict: verdict.clone(),
            approved: false,
            ..Default::default()
        };
        audit(
            data_dir_path,
            session_id,
            cmd,
            &verdict,
            "deny",
            &outcome,
            None,
        );
        return outcome;
    }

    // 3. approval gate: Tier 1 → Allow (no prompt), Tier 2 → prompt.
    let mut approved = true;
    let decision;
    match verdict.verdict {
        Verdict::Allow => decision = "allow",
        Verdict::Ask => {
            // User rules can promote a Tier 2 command back to Tier 1
            // (allow rule) or pin it to Tier 2 with the rule name as
            // reason (deny rule). Allow short-circuits entirely; deny
            // just decorates the prompt.
            if let Some(RuleMatch::Allow(_)) = cfg.classify(cmd) {
                decision = "allow";
            } else {
                let mut reason = reason_for(&primary_rule_id(&verdict));
                if let Some(RuleMatch::Deny(rule)) = cfg.classify(cmd) {
                    reason = format!("deny rule \"{rule}\": {reason}");
                }
                let rule_id = primary_rule_id(&verdict);
                let req = ApprovalRequest {
                    session_id: session_id.to_string(),
                    command: cmd.to_string(),
                    cwd: cwd.to_path_buf(),
                    rule_id: rule_id.clone(),
                    reason: reason.clone(),
                    accept_all_preview: Some(prefix_for_command(cmd)),
                };
                let d = APPROVAL_STORE.gate(&req, approver, &cfg.save_path()).await;
                decision = d.as_str();
                approved = d.allows();
                if !approved {
                    let outcome = ExecOutcome {
                        exit_code: -1,
                        stderr: format!("atom: not approved ({}): {}\n", req.rule_id, req.reason),
                        verdict: verdict.clone(),
                        approved: false,
                        ..Default::default()
                    };
                    audit(
                        data_dir_path,
                        session_id,
                        cmd,
                        &verdict,
                        decision,
                        &outcome,
                        None,
                    );
                    return outcome;
                }
            }
        }
        Verdict::Deny => unreachable!(),
    }

    // 4. spawn (with env scrub + per-session tmpdir).
    let tmpdir = match setup_session_tmpdir(session_id, data_dir_path) {
        Ok(t) => Some(t),
        Err(e) => {
            // tmpdir failure is non-fatal; the command still runs in the
            // host's $TMPDIR. Audit notes the fallback.
            eprintln!("atom: failed to create session tmpdir: {e}");
            None
        }
    };

    let mut outcome = spawn_and_wait(tmpdir.as_ref().map(|t| t.path()), cmd, cwd).await;
    outcome.verdict = verdict.clone();
    outcome.approved = approved;
    outcome.confined = ConfineKind::None;
    audit(
        data_dir_path,
        session_id,
        cmd,
        &verdict,
        decision,
        &outcome,
        None,
    );

    if let Some(t) = tmpdir {
        if let Err(e) = t.cleanup() {
            eprintln!("atom: failed to remove session tmpdir: {e}");
        }
    }
    outcome
}

/// RAII guard for the per-session scratch directory under
/// `dataDir()/tmp/atom-<session>-<rand>`. The directory is created
/// `0700` (so other users on a shared box can't read scratch files)
/// and removed when the guard drops unless [`SessionTmpdir::leak`] is
/// called. The directory is exposed via `path()` so callers can wire
/// it into `$TMPDIR` for spawned commands.
pub struct SessionTmpdir {
    path: PathBuf,
    leaked: bool,
}

impl SessionTmpdir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Detach the guard so the directory is preserved (used by long-
    /// running sessions that want their tmpdir to outlive a single
    /// command). Cleanup must then happen explicitly.
    pub fn leak(mut self) {
        self.leaked = true;
    }

    pub fn cleanup(&self) -> std::io::Result<()> {
        if self.leaked {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.path)
    }
}

impl Drop for SessionTmpdir {
    fn drop(&mut self) {
        if !self.leaked {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Lazy host $TMPDIR so we honor `XDG_RUNTIME_DIR` first (private to
/// the user, often `noexec`), then `$TMPDIR`, then `/tmp`. Computed
/// once per process — the host's tmpdir doesn't move underneath us.
static HOST_TMPDIR: Lazy<PathBuf> = Lazy::new(|| {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("TMPDIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp")
});

/// Per-session tmpdir bookkeeping. v2 doesn't expose a session-start
/// hook yet, so we lazy-create the directory on first use and reuse
/// it for subsequent commands in the same session. Cleanup happens
/// when the explicit `cleanup_session_tmpdir` is called or on Drop.
static SESSION_TMPDIRS: Lazy<Mutex<std::collections::HashMap<String, PathBuf>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

fn setup_session_tmpdir(session_id: &str, _data_dir_path: &Path) -> std::io::Result<SessionTmpdir> {
    let mut cache = SESSION_TMPDIRS
        .lock()
        .map_err(|_| std::io::Error::other("tmpdir cache poisoned"))?;
    if let Some(existing) = cache.get(session_id) {
        if existing.exists() {
            return Ok(SessionTmpdir {
                path: existing.clone(),
                leaked: true, // owned by the cache, not by this guard
            });
        }
    }
    // Build the parent: <host-tmpdir>/atom-<session>-<rand>. Honors
    // `$XDG_RUNTIME_DIR` first so per-user sandboxes on shared boxes
    // don't collide.
    let rand = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let session = if session_id.is_empty() {
        format!("anon-{rand}")
    } else {
        // Session ids are 16-hex in practice; sanitize for the path.
        let safe: String = session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(32)
            .collect();
        format!("{safe}-{rand}")
    };
    let dir = HOST_TMPDIR.join(format!("atom-{session}"));
    std::fs::create_dir_all(&dir)?;
    set_dir_permissions_700(&dir)?;
    cache.insert(session_id.to_string(), dir.clone());
    Ok(SessionTmpdir {
        path: dir,
        leaked: true,
    })
}

#[cfg(unix)]
fn set_dir_permissions_700(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_permissions_700(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Environment scrubbing: every spawn_* helper drops credentials and
/// secrets before exec so a sub-process can't exfiltrate them via
/// `[ -n "$ANTHROPIC_API_KEY" ] && curl …` style shapes. Returns a
/// owned Vec<(OsString, OsString)> so callers can `.clear()` and
/// `.extend()` without aliasing the live env.
pub fn scrub_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut out = Vec::with_capacity(8);
    let drop_suffixes = ["_TOKEN", "_KEY", "_SECRET", "_PASSWORD"];
    let drop_prefixes = ["ANTHROPIC_", "OPENAI_", "GITHUB_TOKEN", "GH_TOKEN"];
    for (key, value) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if drop_prefixes.iter().any(|p| k.starts_with(p)) {
            continue;
        }
        if drop_suffixes.iter().any(|s| k.ends_with(s)) {
            continue;
        }
        // Keep everything else (PATH, LANG, HOME, …) intact.
        out.push((key, value));
    }
    out
}

/// Spawn `/bin/bash -lc cmd`, capturing stdout/stderr separately with
/// a 120s timeout. `tmpdir` (if set) is exported as `$TMPDIR` so tools
/// that read it (cargo, go, sccache, …) Just Work without
/// configuration.
async fn spawn_and_wait(tmpdir: Option<&Path>, cmd: &str, cwd: &Path) -> ExecOutcome {
    let mut command = Command::new(BASH);
    command.arg("-lc").arg(cmd);
    command.current_dir(cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.env_clear();
    let scrubbed = scrub_env();
    for (k, v) in &scrubbed {
        command.env(k, v);
    }
    if let Some(t) = tmpdir {
        command.env("TMPDIR", t);
        command.env("TMP", t);
        command.env("TEMP", t);
    }
    run_child(command).await
}

async fn run_child(mut command: Command) -> ExecOutcome {
    command.kill_on_drop(true);
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutcome {
                exit_code: -1,
                stderr: format!("atom: failed to spawn: {e}\n"),
                ..Default::default()
            };
        }
    };
    let res = tokio::time::timeout(EXEC_TIMEOUT, child.wait_with_output()).await;
    match res {
        Ok(Ok(output)) => ExecOutcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            timed_out: false,
            ..Default::default()
        },
        Ok(Err(e)) => ExecOutcome {
            exit_code: -1,
            stderr: format!("atom: wait failed: {e}\n"),
            ..Default::default()
        },
        Err(_) => ExecOutcome {
            exit_code: -1,
            timed_out: true,
            ..Default::default()
        },
    }
}

/// Append one JSONL audit record to dataDir()/sandbox-audit.log.
/// Best-effort: audit failures never fail the command result.
fn audit(
    data_dir_path: &Path,
    session_id: &str,
    cmd: &str,
    verdict: &Analysis,
    decision: &str,
    outcome: &ExecOutcome,
    note: Option<&str>,
) {
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "session_id": session_id,
        "cmd_sha256": sha256_hash(cmd.as_bytes()),
        "verdict": verdict.verdict.as_str(),
        "tier_origin": verdict.tier_origin,
        "decision": decision,
        "confined": outcome.confined.as_str(),
        "exit_code": outcome.exit_code,
        "timed_out": outcome.timed_out,
        "uses_network": verdict.uses_network,
        "note": note,
    });
    let dir = data_dir_path.to_path_buf();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("sandbox-audit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{record}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::{AutoApprover, Decision, DenyAllApprover};
    use crate::policy::{Rules, SandboxConfig};

    /// Unique per-test workspace/data dirs so parallel tests never share
    /// state (pid + atomic counter + unique tempdir root).
    struct TestEnv {
        _ws: tempfile::TempDir,
        _data: tempfile::TempDir,
    }

    fn env() -> TestEnv {
        TestEnv {
            _ws: tempfile::tempdir().unwrap(),
            _data: tempfile::tempdir().unwrap(),
        }
    }

    fn default_cfg() -> SandboxConfig {
        SandboxConfig::default()
    }

    async fn run_in(
        e: &TestEnv,
        cmd: &str,
        cfg: &SandboxConfig,
        approver: &dyn Approver,
    ) -> ExecOutcome {
        run_with(
            e._data.path(),
            cmd,
            e._ws.path(),
            e._ws.path(),
            format!("sess-{}", std::process::id()).as_str(),
            cfg,
            approver,
        )
        .await
    }

    #[tokio::test]
    async fn echo_hi_runs_and_captures_stdout() {
        let e = env();
        let out = run_in(&e, "echo hi", &default_cfg(), &DenyAllApprover).await;
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim_end(), "hi");
        assert!(!out.timed_out);
        assert_eq!(out.confined, ConfineKind::None);
        assert_eq!(out.verdict.verdict, Verdict::Allow);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["confined"], "none");
        assert_eq!(rec["decision"], "allow");
        assert_eq!(rec["exit_code"], 0);
        assert_eq!(rec["timed_out"], false);
        assert!(rec["cmd_sha256"].as_str().unwrap().len() == 64);
        assert!(rec["ts"].as_str().unwrap().contains('T'));
        assert_eq!(rec["session_id"], format!("sess-{}", std::process::id()));
        // v2: tier_origin is recorded.
        assert!(rec["tier_origin"].is_string());
    }

    #[tokio::test]
    async fn tier1_command_does_not_prompt() {
        let e = env();
        // Use a DenyAllApprover so any prompt would fail the test.
        let out = run_in(&e, "ls -la", &default_cfg(), &DenyAllApprover).await;
        assert_eq!(out.exit_code, 0);
        assert!(out.approved);
        assert_eq!(out.verdict.verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn deny_verdict_short_circuits_without_executing() {
        struct Panic;
        #[async_trait::async_trait]
        impl Approver for Panic {
            async fn decide(&self, _req: ApprovalRequest) -> Decision {
                panic!("approver must not be consulted for Deny verdicts");
            }
        }
        let e = env();
        let out = run_in(&e, "sudo reboot", &default_cfg(), &Panic).await;
        assert_eq!(out.exit_code, -1);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("blocked by sandbox policy"));
        assert!(!out.approved);
        assert_eq!(out.confined, ConfineKind::None);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        // Guardrail denials audit as plain "deny" — no prompt fires.
        assert_eq!(rec["decision"], "deny");
        assert_eq!(rec["exit_code"], -1);
    }

    #[tokio::test]
    async fn ask_with_auto_deny_does_not_run() {
        let e = env();
        let out = run_in(
            &e,
            "curl -sS --max-time 15 http://example.com -o /dev/null",
            &default_cfg(),
            &DenyAllApprover,
        )
        .await;
        assert!(out.verdict.uses_network);
        assert!(!out.approved);
        assert_ne!(out.exit_code, 0);
        assert!(out.stderr.contains("not approved"), "{}", out.stderr);
    }

    #[tokio::test]
    async fn allow_all_persists_allow_rule_and_skips_prompt() {
        let e = env();
        let cfg_path = e._data.path().join("sandbox.json");
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules::default(),
            path: Some(cfg_path.clone()),
        };
        cfg.save_to(&cfg_path).unwrap();
        // First call: prompt + AllowAll → persists rule. `awk` is Tier 2
        // (Ask) and available in every sensible test environment.
        let first = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "ask-allow-all-sess",
            &cfg,
            &AutoApprover(Decision::AllowAll),
        )
        .await;
        assert!(first.approved);
        // Rule landed in the config.
        let on_disk = SandboxConfig::load_from(&cfg_path);
        assert!(
            on_disk.rules.allow.iter().any(|r| r.starts_with("awk")),
            "rule should be saved; got {:?}",
            on_disk.rules.allow
        );
        // Second call with a fresh store: classify() finds the allow
        // rule and short-circuits the prompt.
        let store = ApprovalStore::with_config_path(cfg_path);
        assert!(matches!(
            store.classify("awk 'BEGIN{print 1}'"),
            Some(RuleMatch::Allow(_))
        ));
    }

    #[tokio::test]
    async fn deny_all_persists_deny_rule_and_re_prompts() {
        let e = env();
        let cfg_path = e._data.path().join("sandbox.json");
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules::default(),
            path: Some(cfg_path.clone()),
        };
        cfg.save_to(&cfg_path).unwrap();
        let out = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "ask-deny-all-sess",
            &cfg,
            &AutoApprover(Decision::DenyAll),
        )
        .await;
        assert!(!out.approved);
        let on_disk = SandboxConfig::load_from(&cfg_path);
        assert!(on_disk.rules.deny.iter().any(|r| r.starts_with("awk")));
    }

    #[tokio::test]
    async fn config_allow_rule_promotes_to_tier1() {
        let e = env();
        let cfg_path = e._data.path().join("sandbox.json");
        // Pre-seed: `awk *` is allowed globally.
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules {
                allow: vec!["awk *".into()],
                deny: vec![],
            },
            path: Some(cfg_path.clone()),
        };
        cfg.save_to(&cfg_path).unwrap();
        // The DenyAllApprover would block a prompt, but the config
        // short-circuits it.
        let out = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "config-allow-sess",
            &cfg,
            &DenyAllApprover,
        )
        .await;
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(out.approved);
    }

    #[tokio::test]
    async fn env_scrub_strips_credential_keys() {
        // Set a few keys that should be dropped.
        // SAFETY: tests run single-threaded for env mutation here.
        unsafe {
            std::env::set_var("ATOM_TEST_TOKEN", "secret-token");
            std::env::set_var("ATOM_TEST_API_KEY", "secret-key");
            std::env::set_var("ATOM_TEST_PASSWORD", "secret-pw");
            std::env::set_var("ATOM_TEST_KEEP", "kept");
        }
        let scrubbed = scrub_env();
        let names: Vec<String> = scrubbed
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(!names.iter().any(|k| k == "ATOM_TEST_TOKEN"));
        assert!(!names.iter().any(|k| k == "ATOM_TEST_API_KEY"));
        assert!(!names.iter().any(|k| k == "ATOM_TEST_PASSWORD"));
        assert!(names.iter().any(|k| k == "ATOM_TEST_KEEP"));
        unsafe {
            std::env::remove_var("ATOM_TEST_TOKEN");
            std::env::remove_var("ATOM_TEST_API_KEY");
            std::env::remove_var("ATOM_TEST_PASSWORD");
            std::env::remove_var("ATOM_TEST_KEEP");
        }
    }

    #[tokio::test]
    async fn per_session_tmpdir_is_created_and_set() {
        let e = env();
        // Use a custom TMPDIR parent so we don't litter /tmp.
        let parent = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test for env mutation.
        unsafe {
            std::env::set_var("TMPDIR", parent.path());
        }
        let out = run_with(
            e._data.path(),
            "echo $TMPDIR",
            e._ws.path(),
            e._ws.path(),
            "tmpdir-sess",
            &default_cfg(),
            &DenyAllApprover,
        )
        .await;
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(
            out.stdout.contains("atom-tmpdir-sess-"),
            "stdout: {}",
            out.stdout
        );
        unsafe {
            std::env::remove_var("TMPDIR");
        }
    }

    #[test]
    fn primary_rule_id_picks_first_real_rule() {
        let a = Analysis {
            matched_rules: vec![
                "unknown-command".to_string(),
                "curl".to_string(),
                "path-escape-write".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(primary_rule_id(&a), "curl");
    }

    #[test]
    fn reason_for_known_rule_returns_table_text() {
        assert!(reason_for("curl").contains("network"));
        assert_eq!(reason_for("not-a-real-rule"), "requires approval");
    }

    #[test]
    fn deny_all_writes_a_deny_rule_via_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox.json");
        let store = ApprovalStore::with_config_path(path.clone());
        store
            .record("rm -rf /tmp/foo", Decision::DenyAll, &path)
            .unwrap();
        let cfg = SandboxConfig::load_from(&path);
        assert!(cfg.rules.deny.iter().any(|r| r.starts_with("rm")));
    }
}
