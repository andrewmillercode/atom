//! The execution pipeline everything calls:
//!
//! `analyze -> approval gate -> spawn -> audit`.
//!
//! The wide static allowlist sets the tiers: matching commands run
//! silently, everything else prompts. Guardrail commands (recursive rm,
//! sudo, …) are flagged instead of blocked — they always prompt, and a
//! session grant never covers them.
//!
//! Per-session tmpdir setup + subprocess env scrubbing happen inside
//! [`run_with`], the single entry point for both `run` (which uses
//! the data dir) and direct test calls.

use crate::approvals::{ApprovalRequest, ApprovalStore, Approver};
use crate::policy::{prefix_for_command, RuleMatch, SandboxConfig};
use crate::rules::{self, Analysis, Verdict};
use crate::seatbelt;
use atom_core::cancel::CancelToken;
use atom_core::session::store::data_dir;
use atom_core::util::sha256_hash;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// Hard wall-clock limit for the legacy blocking entry point ([`run`] /
/// [`run_with`], used by tests and callers that need a synchronous
/// result). The bash tool itself has no timeout: a command runs until
/// it exits and the turn waits; runaway protection is the user's Esc.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) const BASH: &str = "/bin/bash";

/// Process-global approval store backed by sandbox.json. Held behind a
/// `Lazy` so the disk read happens once on first use; per-process
/// callers who want a fresh view call [`ApprovalStore::reload`].
static APPROVAL_STORE: Lazy<ApprovalStore> = Lazy::new(ApprovalStore::new);

pub fn approval_store() -> &'static ApprovalStore {
    &APPROVAL_STORE
}

/// How a command was confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfineKind {
    None,
    /// Seatbelt profile via `sandbox-exec` (macOS). Serializes as
    /// `seatbelt` in the audit log; the legacy spellings still parse.
    #[serde(rename = "seatbelt", alias = "bwrap", alias = "seatbelt_or_bwrap")]
    SeatbeltOrBwrap,
}

impl ConfineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfineKind::None => "none",
            ConfineKind::SeatbeltOrBwrap => "seatbelt",
        }
    }

    /// Shown after a confined command fails, so a sandbox denial is
    /// distinguishable from the command's own error.
    pub fn failure_hint(&self) -> &'static str {
        match self {
            ConfineKind::None => "",
            ConfineKind::SeatbeltOrBwrap => {
                "\natom: this command ran under the seatbelt sandbox; if a permission error above is unexpected, set \"confine\": false in sandbox.json to run unconfined.\n"
            }
        }
    }
}

/// Result of one sandboxed execution.
#[derive(Debug)]
pub struct ExecOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub verdict: Analysis,
    pub approved: bool,
    /// How the command was confined; `SeatbeltOrBwrap` when the
    /// Seatbelt profile was applied.
    pub confined: ConfineKind,
    /// Set when the command is still running: the caller (the turn
    /// loop) parks on it and the tool result is recorded when it exits.
    pub pending: Option<PendingProcess>,
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
            pending: None,
        }
    }
}

/// A bash command that is still running. The bash tool returns one for
/// every command — the turn loop parks on it until it exits (no
/// timeout; runaway protection is the user's Esc) and records the
/// output as the original tool call's result. stdout/stderr are drained
/// continuously by spawned tasks so large output cannot fill the pipe
/// buffers and deadlock a long-running command.
pub struct PendingProcess {
    command: String,
    started: Instant,
    child: Child,
    status: Option<std::process::ExitStatus>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    output: PendingOutput,
    killed: bool,
    confined: ConfineKind,
}

/// Live view of a running command's output. The pipe readers append
/// every chunk to these shared buffers as it arrives, so the turn loop
/// can peek at partial output while the command is still running
/// (placeholder refresh) without touching the child handle.
#[derive(Clone, Default)]
pub struct PendingOutput(Arc<std::sync::Mutex<PartialOutput>>);

#[derive(Default)]
struct PartialOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl PendingOutput {
    /// Everything written so far: stdout then stderr, matching the order
    /// [`PendingExit::output`] uses at collection time.
    pub fn text(&self) -> String {
        let out = self.0.lock().unwrap_or_else(|p| p.into_inner());
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// Appends one chunk to the buffer. The pipe readers call this per
    /// read; tests use it to seed partial output.
    pub fn extend(&self, stdout: bool, chunk: &[u8]) {
        let mut out = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if stdout {
            out.stdout.extend_from_slice(chunk);
        } else {
            out.stderr.extend_from_slice(chunk);
        }
    }
}

impl std::fmt::Debug for PendingProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingProcess")
            .field("command", &self.command)
            .field("started", &self.started)
            .field("killed", &self.killed)
            .finish_non_exhaustive()
    }
}

impl PendingProcess {
    /// The command this process is running (for placeholder results).
    pub fn command(&self) -> &str {
        &self.command
    }

    /// When the command was spawned (for placeholder results).
    pub fn started(&self) -> Instant {
        self.started
    }

    /// How long the command has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// A cloneable handle to the output written so far — the turn loop
    /// keeps one on the pending command to refresh placeholders while
    /// this process runs.
    pub fn output_handle(&self) -> PendingOutput {
        self.output.clone()
    }

    /// Was the process killed (Esc) rather than exiting on its own.
    pub fn killed(&self) -> bool {
        self.killed
    }

    /// SIGKILL the process group and reap the child. The child was
    /// spawned with process_group(0), so its pgid == its pid and the
    /// group kill reaches `bash -lc "cargo test"`'s grandchildren too.
    pub async fn kill(&mut self) {
        let pid = self.child.id().unwrap_or(0) as i32;
        if pid > 0 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill().await;
        self.status = self.child.wait().await.ok();
        self.killed = true;
    }

    /// Waits for the process to exit on its own (no timeout — a long
    /// test suite runs until it is done).
    pub async fn wait_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        self.status = status.as_ref().ok().copied();
        status
    }

    /// Joins the concurrent pipe readers and returns the exit. Safe
    /// after [`kill`] or a normal [`wait_exit`]: both streams close when
    /// the process (group) dies, so these joins return promptly.
    pub async fn collect(self) -> PendingExit {
        let _ = self.stdout_task.await;
        let _ = self.stderr_task.await;
        let exit_code = self
            .status
            .as_ref()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);
        let output = {
            let buf = self.output.0.lock().unwrap_or_else(|p| p.into_inner());
            (
                String::from_utf8_lossy(&buf.stdout).into_owned(),
                String::from_utf8_lossy(&buf.stderr).into_owned(),
            )
        };
        PendingExit {
            exit_code,
            output: format!("{}{}", output.0, output.1),
            killed: self.killed,
            confined: self.confined,
        }
    }

    /// Convenience for direct callers (tests): wait for exit or the kill
    /// token, then collect.
    pub async fn run_until_done(mut self, kill: CancelToken) -> PendingExit {
        tokio::select! {
            biased;
            _ = kill.cancelled() => self.kill().await,
            _ = self.wait_exit() => {}
        }
        self.collect().await
    }
}

/// The exit of a pending command: its exit code, combined output, and
/// whether it was killed by the user (Esc) instead of exiting itself.
pub struct PendingExit {
    pub exit_code: i32,
    pub output: String,
    pub killed: bool,
    /// The confinement applied to the command that produced this exit.
    pub confined: ConfineKind,
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
    // 1-2. analyze → approval gate.
    let authed = match authorize(
        data_dir_path,
        cmd,
        cwd,
        workspace_root,
        session_id,
        cfg,
        approver,
    )
    .await
    {
        Err(blocked) => return blocked,
        Ok(authed) => authed,
    };

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
    let profile = confine_profile(
        &authed.verdict,
        workspace_root,
        tmpdir.as_ref().map(|t| t.path()),
        cfg,
    );

    let mut outcome = spawn_and_wait(tmpdir.as_ref().map(|t| t.path()), &profile, cmd, cwd).await;
    outcome.verdict = authed.verdict.clone();
    outcome.approved = authed.approved;
    outcome.confined = confine_kind(&profile);
    if outcome.exit_code != 0 && !outcome.timed_out {
        outcome.stderr.push_str(outcome.confined.failure_hint());
    }
    audit(
        data_dir_path,
        session_id,
        cmd,
        &authed.verdict,
        authed.decision,
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

/// Head of the pipeline: static analysis, then the approval gate.
/// Flagged commands gate like any Tier 2 command, carrying their
/// guardrail reason into the prompt.
#[allow(clippy::result_large_err)]
async fn authorize(
    data_dir_path: &Path,
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    session_id: &str,
    cfg: &SandboxConfig,
    approver: &dyn Approver,
) -> Result<Authed, ExecOutcome> {
    // 1. analyze
    let verdict = rules::analyze_full(cmd, workspace_root, cwd);

    // 2. approval gate: Tier 1 → Allow (no prompt), Tier 2 → prompt.
    let mut approved = true;
    let decision;
    match verdict.verdict {
        Verdict::Allow => decision = "allow",
        Verdict::Ask => {
            // User rules can promote an unflagged Tier 2 command back to
            // Tier 1 (allow rule), or pin it with the rule name as
            // reason (deny rule). Flagged commands skip the promotion —
            // they always reach the gate.
            let promoted =
                !verdict.flagged && matches!(cfg.classify(cmd), Some(RuleMatch::Allow(_)));
            if promoted {
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
                    workspace_root: workspace_root.to_path_buf(),
                    rule_id: rule_id.clone(),
                    reason: reason.clone(),
                    accept_all_preview: Some(prefix_for_command(cmd)),
                    flagged: verdict.flagged,
                };
                let d = APPROVAL_STORE.gate(&req, approver).await;
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
                    return Err(outcome);
                }
            }
        }
    }
    Ok(Authed {
        verdict,
        approved,
        decision,
    })
}

/// Verdict + approval decision from [`authorize`].
struct Authed {
    verdict: Analysis,
    approved: bool,
    decision: &'static str,
}

/// The bash tool's entry point: like [`run`] but returning a
/// [`PendingProcess`] for the command — every bash call is pending
/// until the command exits. The turn loop parks on it; Esc (via the
/// kill token) stops the whole process group. There is no timeout:
/// a long test suite runs until it is done.
pub async fn run_tool(
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    session_id: &str,
    cfg: &SandboxConfig,
    approver: &dyn Approver,
) -> ExecOutcome {
    run_tool_with(
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

/// [`run_tool`] with an explicit data dir (hermetic tests).
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_with(
    data_dir_path: &Path,
    cmd: &str,
    cwd: &Path,
    workspace_root: &Path,
    session_id: &str,
    cfg: &SandboxConfig,
    approver: &dyn Approver,
) -> ExecOutcome {
    let authed = match authorize(
        data_dir_path,
        cmd,
        cwd,
        workspace_root,
        session_id,
        cfg,
        approver,
    )
    .await
    {
        Err(blocked) => return blocked,
        Ok(authed) => authed,
    };

    let tmpdir = match setup_session_tmpdir(session_id, data_dir_path) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("atom: failed to create session tmpdir: {e}");
            None
        }
    };

    // Spawn piped: the reader tasks drain both streams concurrently so
    // output larger than the 64KB pipe buffers cannot deadlock the
    // command. Own process group so Esc reaches the grandchildren.
    let profile = confine_profile(
        &authed.verdict,
        workspace_root,
        tmpdir.as_ref().map(|t| t.path()),
        cfg,
    );
    let confined = confine_kind(&profile);
    let mut command = confined_command(profile.as_deref(), cmd);
    command.current_dir(cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.env_clear();
    for (k, v) in scrub_env() {
        command.env(k, v);
    }
    if let Some(t) = tmpdir.as_ref().map(|t| t.path()) {
        command.env("TMPDIR", t);
        command.env("TMP", t);
        command.env("TEMP", t);
    }
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let outcome = match command.spawn() {
        Ok(mut child) => {
            // Drain the pipes from the start: wait_with_output-style
            // collection happens later, when the turn loop parks on the
            // child and it finally exits.
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let output = PendingOutput::default();
            let stdout_buf = output.clone();
            let stderr_buf = output.clone();
            let stdout_task = tokio::spawn(async move {
                if let Some(mut pipe) = stdout {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => stdout_buf.extend(true, &chunk[..n]),
                        }
                    }
                }
            });
            let stderr_task = tokio::spawn(async move {
                if let Some(mut pipe) = stderr {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => stderr_buf.extend(false, &chunk[..n]),
                        }
                    }
                }
            });
            ExecOutcome {
                exit_code: 0,
                approved: authed.approved,
                verdict: authed.verdict.clone(),
                confined,
                pending: Some(PendingProcess {
                    command: cmd.to_string(),
                    started: Instant::now(),
                    child,
                    status: None,
                    stdout_task,
                    stderr_task,
                    output,
                    killed: false,
                    confined,
                }),
                ..Default::default()
            }
        }
        Err(e) => ExecOutcome {
            exit_code: -1,
            stderr: format!("atom: failed to spawn: {e}\n"),
            approved: authed.approved,
            verdict: authed.verdict.clone(),
            confined,
            ..Default::default()
        },
    };
    let note = if outcome.pending.is_some() {
        Some("pending")
    } else {
        None
    };
    audit(
        data_dir_path,
        session_id,
        cmd,
        &authed.verdict,
        authed.decision,
        &outcome,
        note,
    );
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

/// The Seatbelt profile for a command that should be confined, or
/// `None` to run it as-is. Everything unflagged is confined, with the
/// command's own outside-workspace argument paths granted as extra
/// write roots. Flagged guardrails stay unconfined only when the flag
/// came from the privilege rules (`sudo`, `kill`, …), which cannot run
/// confined; a flag from the path analysis (an approved outside write)
/// still runs confined with its named paths granted. `confine: false`
/// in `sandbox.json` disables the whole layer.
fn confine_profile(
    verdict: &Analysis,
    workspace_root: &Path,
    tmpdir: Option<&Path>,
    cfg: &SandboxConfig,
) -> Option<String> {
    if !cfg.confine || !seatbelt::available() {
        return None;
    }
    if verdict.flagged {
        let flagged_by_privilege = verdict
            .matched_rules
            .iter()
            .any(|id| rules::RULES.iter().any(|r| r.id == *id && r.flagged));
        if flagged_by_privilege {
            return None;
        }
    }
    Some(seatbelt::profile(
        workspace_root,
        tmpdir,
        &verdict.outside_paths,
    ))
}

fn confine_kind(profile: &Option<String>) -> ConfineKind {
    if profile.is_some() {
        ConfineKind::SeatbeltOrBwrap
    } else {
        ConfineKind::None
    }
}

/// `/bin/bash -lc cmd`, wrapped in `sandbox-exec` when a profile is
/// present.
fn confined_command(profile: Option<&str>, cmd: &str) -> Command {
    let mut command = match profile {
        Some(profile) => {
            let mut c = Command::new(seatbelt::SANDBOX_EXEC);
            c.arg("-p").arg(profile).arg(BASH);
            c
        }
        None => Command::new(BASH),
    };
    command.arg("-lc").arg(cmd);
    command
}

/// Spawn `/bin/bash -lc cmd`, capturing stdout/stderr separately with
/// a 120s timeout. `tmpdir` (if set) is exported as `$TMPDIR` so tools
/// that read it (cargo, go, sccache, …) Just Work without
/// configuration. `profile` (if set) confines the whole process tree
/// under Seatbelt.
async fn spawn_and_wait(
    tmpdir: Option<&Path>,
    profile: &Option<String>,
    cmd: &str,
    cwd: &Path,
) -> ExecOutcome {
    let mut command = confined_command(profile.as_deref(), cmd);
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
        "flagged": verdict.flagged,
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

    /// A real toolchain command under the real profile. `cargo build`
    /// with no dependencies writes only `target/` in the workspace, so
    /// a failure here means the profile is too tight, not that the
    /// network or a cache was missing.
    #[tokio::test]
    async fn cargo_build_runs_confined() {
        if !crate::seatbelt::available() {
            return;
        }
        let e = env();
        let ws = e._ws.path();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            "[package]\nname = \"seatbelt-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() { println!(\"ok\"); }\n").unwrap();
        let out = run_in(
            &e,
            "cargo build --offline",
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert_eq!(
            out.confined,
            ConfineKind::SeatbeltOrBwrap,
            "the smoke build must be confined"
        );
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(ws.join("target").exists(), "build wrote its artifact");
    }

    /// A sandbox denial must be distinguishable from the command's own
    /// failure in the output the user sees. The target is written as
    /// `$HOME/...` so static analysis cannot see it as a granted path;
    /// the shell expands it at run time and the profile denies it.
    #[tokio::test]
    async fn confinement_failure_is_attributed() {
        if !crate::seatbelt::available() {
            return;
        }
        let e = env();
        let cmd = "echo bad > ${HOME}/.atom-exec-probe/x";
        // Create the target dir so a failure means the sandbox denied
        // the write, not that the directory was missing. `${HOME}` is
        // deliberately the brace form: static analysis cannot resolve
        // it, so the path is not granted and the profile must deny it.
        let real = dirs::home_dir().unwrap().join(".atom-exec-probe");
        let _ = std::fs::remove_dir_all(&real);
        if std::fs::create_dir_all(&real).is_err() {
            return;
        }
        let out = run_in(&e, cmd, &default_cfg(), &AutoApprover(Decision::AllowOnce)).await;
        let leaked = real.join("x").exists();
        let _ = std::fs::remove_dir_all(&real);
        assert!(!leaked, "no file may land outside the workspace");
        assert_eq!(out.confined, ConfineKind::SeatbeltOrBwrap);
        assert_ne!(out.exit_code, 0, "outside write must fail");
        assert!(
            out.stderr.contains("seatbelt sandbox"),
            "failure must name the sandbox: {}",
            out.stderr
        );
    }

    /// An approved outside write keeps its sandbox: the profile grants
    /// the named path, so the write lands and confinement stays on.
    #[tokio::test]
    async fn approved_outside_write_stays_confined() {
        if !crate::seatbelt::available() {
            return;
        }
        let e = env();
        let target = dirs::home_dir()
            .unwrap()
            .join(format!(".atom-flag-probe-{}", std::process::id()));
        let cmd = format!("echo x > {}", target.display());
        let out = run_in(&e, &cmd, &default_cfg(), &AutoApprover(Decision::AllowOnce)).await;
        let existed = target.exists();
        let _ = std::fs::remove_file(&target);
        assert_eq!(
            out.confined,
            ConfineKind::SeatbeltOrBwrap,
            "a write flag must not lift confinement"
        );
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(existed, "the granted path must be writable");
    }

    /// A privilege guardrail (`kill`) runs unconfined after approval.
    #[tokio::test]
    async fn privilege_guardrail_runs_unconfined() {
        let e = env();
        let out = run_in(
            &e,
            "kill -0 $$",
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert_eq!(out.confined, ConfineKind::None);
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
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
        run_in_session(
            e,
            cmd,
            format!("sess-{}", std::process::id()).as_str(),
            cfg,
            approver,
        )
        .await
    }

    async fn run_in_session(
        e: &TestEnv,
        cmd: &str,
        session_id: &str,
        cfg: &SandboxConfig,
        approver: &dyn Approver,
    ) -> ExecOutcome {
        run_with(
            e._data.path(),
            cmd,
            e._ws.path(),
            e._ws.path(),
            session_id,
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
        let expected = if crate::seatbelt::available() {
            ConfineKind::SeatbeltOrBwrap
        } else {
            ConfineKind::None
        };
        assert_eq!(out.confined, expected);
        assert_eq!(out.verdict.verdict, Verdict::Allow);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["confined"], expected.as_str());
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

    // ---- pending-process model ----

    fn unique_session() -> String {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("pend-{}-{n}", std::process::id())
    }

    /// Every bash call comes back pending; the turn loop parks on it
    /// until the command exits and records the output as the result.
    #[tokio::test]
    async fn pending_command_completes_with_full_output() {
        let e = env();
        let out = run_tool_with(
            e._data.path(),
            "echo starting; sleep 1; echo done",
            e._ws.path(),
            e._ws.path(),
            &unique_session(),
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert_eq!(out.exit_code, 0);
        assert!(out.approved);
        let pp = out.pending.expect("bash returns a pending process");
        assert_eq!(pp.command(), "echo starting; sleep 1; echo done");

        let exit = pp.run_until_done(CancelToken::new()).await;
        assert!(!exit.killed);
        assert_eq!(exit.exit_code, 0);
        assert!(exit.output.contains("starting") && exit.output.contains("done"));
    }

    /// Partial output is peekable while the command is still running:
    /// the turn loop refreshes placeholders from this live view without
    /// any status tool.
    #[tokio::test]
    async fn pending_output_is_peekable_mid_run() {
        let e = env();
        let out = run_tool_with(
            e._data.path(),
            "echo partial-marker; sleep 5; echo never-reached",
            e._ws.path(),
            e._ws.path(),
            &unique_session(),
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        let mut pp = out.pending.expect("pending process");
        let peek = pp.output_handle();

        // Poll until the marker shows up, well before the 5s sleep ends.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if peek.text().contains("partial-marker") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "output never became peekable"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!peek.text().contains("never-reached"));

        pp.kill().await;
        let exit = pp.collect().await;
        assert!(exit.killed);
        assert!(exit.output.contains("partial-marker"));
    }

    /// Esc (the kill token) stops the whole process group promptly —
    /// including the command's children, not just the immediate bash.
    #[tokio::test]
    async fn esc_kills_pending_command_process_group() {
        let e = env();
        let out = run_tool_with(
            e._data.path(),
            "sh -c 'sleep 30' & wait", // grandchild under the tool's bash
            e._ws.path(),
            e._ws.path(),
            &unique_session(),
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        let mut pp = out.pending.expect("pending process");
        let token = CancelToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            t2.cancel();
        });
        let started = std::time::Instant::now();
        let exit = pp.run_until_done(token).await;
        assert!(exit.killed, "expected a killed exit");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "kill should be prompt, took {:?}",
            started.elapsed()
        );
    }

    /// Output far beyond the 64KB pipe buffers must not deadlock: the
    /// reader tasks drain the pipes from spawn time.
    #[tokio::test]
    async fn huge_output_does_not_deadlock() {
        let e = env();
        let out = run_tool_with(
            e._data.path(),
            "yes | head -c 300000",
            e._ws.path(),
            e._ws.path(),
            &unique_session(),
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        let pp = out.pending.expect("pending process");
        let exit = tokio::time::timeout(
            Duration::from_secs(30),
            pp.run_until_done(CancelToken::new()),
        )
        .await
        .expect("drained pipes must not deadlock");
        assert_eq!(exit.exit_code, 0, "output: {}", exit.output.len());
        assert_eq!(exit.output.len(), 300000);
    }

    /// A long command is pending with no timeout: the tool call returns
    /// immediately while the command keeps running.
    #[tokio::test]
    async fn tool_call_returns_before_command_finishes() {
        let e = env();
        let started = std::time::Instant::now();
        let out = run_tool_with(
            e._data.path(),
            "sleep 2",
            e._ws.path(),
            e._ws.path(),
            &unique_session(),
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "tool call must not wait for the command: {:?}",
            started.elapsed()
        );
        let pp = out.pending.expect("pending process");
        let exit = pp.run_until_done(CancelToken::new()).await;
        assert_eq!(exit.exit_code, 0);
    }

    #[tokio::test]
    async fn guardrail_with_deny_all_is_not_approved() {
        let e = env();
        // No more hard block: the guardrail command reaches the
        // approval gate, and a denying user refuses it.
        let out = run_in(&e, "sudo reboot", &default_cfg(), &DenyAllApprover).await;
        assert_eq!(out.exit_code, -1);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("not approved"), "{}", out.stderr);
        assert!(!out.approved);
        assert_eq!(out.confined, ConfineKind::None);
        assert!(out.verdict.flagged);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["decision"], "deny_once");
        assert_eq!(rec["exit_code"], -1);
        assert_eq!(rec["flagged"], true);
    }

    #[tokio::test]
    async fn guardrail_runs_when_user_allows_once() {
        let e = env();
        // The user is the only authority: allow it and it runs.
        let out = run_in(
            &e,
            "kill -0 $$",
            &default_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert!(out.approved);
        assert!(out.verdict.flagged, "kill must be flagged");
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["flagged"], true);
        assert_eq!(rec["decision"], "allow_once");
    }

    #[tokio::test]
    async fn session_grant_does_not_cover_flagged_command() {
        let e = env();
        // An unflagged `rm` grant must not cover the flagged one.
        let sid = format!("flag-grant-{}", std::process::id());
        approval_store().record(&sid, "rm -rf build", false, Decision::AllowSession);
        let out = run_in_session(&e, "rm -rf ~", &sid, &default_cfg(), &DenyAllApprover).await;
        assert!(!out.approved, "flagged command must not inherit a grant");
        assert!(out.stderr.contains("not approved"), "{}", out.stderr);
        assert!(out.verdict.flagged);
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
    async fn allow_session_grants_family_for_that_session_only() {
        let e = env();
        let cfg_path = e._data.path().join("sandbox.json");
        let cfg = SandboxConfig {
            version: crate::policy::VERSION,
            rules: Rules::default(),
            confine: true,
            path: Some(cfg_path.clone()),
        };
        cfg.save_to(&cfg_path).unwrap();
        // First ask in sess-a: prompt + AllowSession → runs and grants
        // the family for this session.
        let first = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "grant-sess-a",
            &cfg,
            &AutoApprover(Decision::AllowSession),
        )
        .await;
        assert!(first.approved);
        // Nothing landed on disk — session grants are memory-only.
        let on_disk = SandboxConfig::load_from(&cfg_path);
        assert!(on_disk.rules.allow.is_empty(), "{:?}", on_disk.rules.allow);
        assert!(on_disk.rules.deny.is_empty());
        // Second call in the SAME session: the grant skips the prompt
        // (DenyAllApprover would fail any prompt).
        let second = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "grant-sess-a",
            &cfg,
            &DenyAllApprover,
        )
        .await;
        assert!(second.approved, "stderr: {}", second.stderr);
        // A different session id does not inherit the grant.
        let other = run_with(
            e._data.path(),
            "awk 'BEGIN{print 1}'",
            e._ws.path(),
            e._ws.path(),
            "grant-sess-b",
            &cfg,
            &DenyAllApprover,
        )
        .await;
        assert!(!other.approved);
        assert!(other.stderr.contains("not approved"), "{}", other.stderr);
        // And the failed cross-session attempt still wrote nothing.
        assert!(SandboxConfig::load_from(&cfg_path).rules.allow.is_empty());
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
            confine: true,
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
        // Note: no TMPDIR mutation here — it is process-global and would
        // make other tests' tempdirs nest inside `parent`, which this
        // test then deletes. HOST_TMPDIR is computed once per process
        // anyway, so the assertion below doesn't depend on it.
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
}
