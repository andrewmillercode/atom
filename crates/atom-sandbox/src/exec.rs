//! The execution pipeline everything calls:
//! analyze -> approval gate -> confinement -> audit.
//!
//! `SandboxMode::Off` skips the gate and confinement but still audits.

use crate::approvals::{ApprovalRequest, ApprovalStore, Approver, Decision};
use crate::policy::{SandboxConfig, SandboxMode};
use crate::rules::{self, Analysis, Verdict, RULES};
use crate::seatbelt::{self, bwrap_args, linux_confine, ConfineKind};
use atom_core::session::store::data_dir;
use atom_core::util::sha256_hash;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Hard wall-clock limit for one confined command.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(120);

const BASH: &str = "/bin/bash";

/// Process-global approval store so AllowSession grants persist across
/// calls within a session and globals survive restarts via approvals.json.
static APPROVAL_STORE: Lazy<ApprovalStore> = Lazy::new(ApprovalStore::new);

pub fn approval_store() -> &'static ApprovalStore {
    &APPROVAL_STORE
}

/// Result of one sandboxed execution.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// -1 when the command never ran (blocked/timeout kill without code).
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub verdict: Analysis,
    pub approved: bool,
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
        .find(|id| RULES.iter().any(|r| r.id == *id))
        .cloned()
        .unwrap_or_default()
}

fn reason_for(rule_id: &str) -> String {
    RULES
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

/// Whether a run may use outbound network inside its confinement profile.
///
/// The config's [`NetPolicy`] is the baseline; additionally, a command that
/// touches the network (`Analysis::uses_network`) and passed the approval
/// gate (`approved`) is granted network egress for that single run. This is
/// what makes the network approval prompt meaningful: approving curl/wget
/// actually lets them connect, while unapproved or non-network commands stay
/// confined by the policy.
pub fn net_allowed_for_run(cfg: &SandboxConfig, uses_network: bool, approved: bool) -> bool {
    cfg.net_allowed() || (uses_network && approved)
}

/// Like [`run`] with an explicit data dir for the audit log (tests use
/// unique temp dirs so parallel runs stay hermetic).
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
    let verdict = rules::analyze_full(cmd, workspace_root, cwd, cfg.strict());

    // Off mode: no gate, no confinement — still audited.
    if cfg.mode == SandboxMode::Off {
        let mut outcome = spawn_and_wait(None, cmd, cwd).await;
        outcome.verdict = verdict.clone();
        outcome.approved = false;
        outcome.confined = ConfineKind::None;
        audit(
            data_dir_path,
            session_id,
            cmd,
            &verdict,
            "off",
            ConfineKind::None,
            &outcome,
            None,
        );
        return outcome;
    }

    // 2. approval gate
    let mut approved = true;
    let decision;
    match verdict.verdict {
        Verdict::Deny => {
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
                Decision::Deny.as_str(),
                ConfineKind::None,
                &outcome,
                None,
            );
            return outcome;
        }
        Verdict::Ask => {
            let rule_id = primary_rule_id(&verdict);
            let key = crate::approvals::key_for(&rule_id, cmd, cwd);
            if APPROVAL_STORE.check(session_id, &key) {
                decision = Decision::AllowSession.as_str();
            } else {
                let req = ApprovalRequest {
                    session_id: session_id.to_string(),
                    command: cmd.to_string(),
                    cwd: cwd.to_path_buf(),
                    rule_id: rule_id.clone(),
                    reason: reason_for(&rule_id),
                };
                let d = APPROVAL_STORE.gate(&req, &key, approver).await;
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
                        ConfineKind::None,
                        &outcome,
                        None,
                    );
                    return outcome;
                }
            }
        }
        Verdict::Allow => decision = "allow",
    }

    // 3. confinement
    // An approved network command runs with network egress enabled even
    // when the sandbox config denies network by default; otherwise the
    // approval prompt would be pointless (the profile's `(deny network*)`
    // would still kill the request). Denied commands never reach this
    // point, so `approved` is the only extra signal needed.
    let net_allowed = net_allowed_for_run(cfg, verdict.uses_network, approved);
    let net_denied = !net_allowed;
    let profile = if seatbelt::seatbelt_available() {
        Some(seatbelt::generate_profile(
            cfg,
            workspace_root,
            &std::env::temp_dir(),
            data_dir_path,
            net_allowed,
        ))
    } else {
        None
    };

    let (confined, note): (ConfineKind, Option<String>) = match profile {
        Some(p) => match write_profile_file(&p) {
            Ok(tmp) => {
                let out = spawn_and_wait(
                    Some((seatbelt::SANDBOX_EXEC.into(), tmp.path().into())),
                    cmd,
                    cwd,
                )
                .await;
                drop(tmp);
                let mut o = out;
                o.verdict = verdict.clone();
                o.approved = approved;
                o.confined = ConfineKind::Seatbelt;
                audit(
                    data_dir_path,
                    session_id,
                    cmd,
                    &verdict,
                    decision,
                    ConfineKind::Seatbelt,
                    &o,
                    None,
                );
                return o;
            }
            Err(e) => (
                ConfineKind::None,
                Some(format!("failed to write seatbelt profile: {e}")),
            ),
        },
        None => {
            if cfg!(target_os = "linux")
                && linux_confine(workspace_root, &std::env::temp_dir(), net_denied)
                    == ConfineKind::Bwrap
            {
                let args = bwrap_args(
                    workspace_root,
                    &std::env::temp_dir(),
                    data_dir_path,
                    net_denied,
                );
                let out = spawn_bwrap(&args, cmd, cwd).await;
                let mut o = out;
                o.verdict = verdict.clone();
                o.approved = approved;
                o.confined = ConfineKind::Bwrap;
                audit(
                    data_dir_path,
                    session_id,
                    cmd,
                    &verdict,
                    decision,
                    ConfineKind::Bwrap,
                    &o,
                    None,
                );
                return o;
            }
            (
                ConfineKind::None,
                Some("no confinement mechanism available".into()),
            )
        }
    };

    // Unconfined fallback path (still gated + audited).
    let mut outcome = spawn_and_wait(None, cmd, cwd).await;
    outcome.verdict = verdict.clone();
    outcome.approved = approved;
    outcome.confined = confined;
    audit(
        data_dir_path,
        session_id,
        cmd,
        &verdict,
        decision,
        confined,
        &outcome,
        note.as_deref(),
    );
    outcome
}

/// Write an SBPL profile to a fresh 0600 file under temp_dir().
/// Caller must keep the guard alive until the child has spawned and
/// read it; dropping deletes the file.
fn write_profile_file(profile: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .prefix("atom-sbx-")
        .rand_bytes(8)
        .tempfile_in(std::env::temp_dir())?;
    f.write_all(profile.as_bytes())?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

/// Spawn `/bin/bash -lc cmd`, optionally wrapped in
/// `/usr/bin/sandbox-exec -f <profile>`, capturing stdout/stderr
/// separately with a 120s timeout that kills the child.
async fn spawn_and_wait(confine: Option<(String, PathBuf)>, cmd: &str, cwd: &Path) -> ExecOutcome {
    let mut command = match confine {
        Some((frontend, profile)) => {
            let mut c = Command::new(&frontend);
            c.arg("-f").arg(&profile);
            c.arg(BASH).arg("-lc").arg(cmd);
            c
        }
        None => {
            let mut c = Command::new(BASH);
            c.arg("-lc").arg(cmd);
            c
        }
    };
    command.current_dir(cwd);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    run_child(command).await
}

async fn spawn_bwrap(args: &[String], cmd: &str, cwd: &Path) -> ExecOutcome {
    let mut command = Command::new("bwrap");
    command.args(args);
    command
        .arg("--")
        .arg(BASH)
        .arg("-lc")
        .arg(cmd)
        .current_dir(cwd);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    run_child(command).await
}

async fn run_child(mut command: Command) -> ExecOutcome {
    // kill_on_drop makes a dropped timeout future kill the child.
    command.kill_on_drop(true);
    let child = command.spawn();
    let child = match child {
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
        // Timeout: the future was dropped, which killed the child
        // (kill_on_drop); partial output is lost by design.
        Err(_) => ExecOutcome {
            exit_code: -1,
            timed_out: true,
            ..Default::default()
        },
    }
}

/// Append one JSONL audit record to dataDir()/sandbox-audit.log.
/// Best-effort: audit failures never fail the command result.
#[allow(clippy::too_many_arguments)]
fn audit(
    data_dir_path: &Path,
    session_id: &str,
    cmd: &str,
    verdict: &Analysis,
    decision: &str,
    confined: ConfineKind,
    outcome: &ExecOutcome,
    note: Option<&str>,
) {
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        "session_id": session_id,
        "cmd_sha256": sha256_hash(cmd.as_bytes()),
        "verdict": verdict.verdict.as_str(),
        "decision": decision,
        "confined": confined.as_str(),
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
    use crate::policy::{NetPolicy, SandboxConfig};
    use std::time::Instant;

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

    fn workspace_cfg() -> SandboxConfig {
        SandboxConfig {
            mode: SandboxMode::Workspace,
            network: NetPolicy::Deny,
            ..Default::default()
        }
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn echo_hi_runs_confined_and_captures_stdout() {
        let e = env();
        let out = run_in(&e, "echo hi", &workspace_cfg(), &DenyAllApprover).await;
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim_end(), "hi");
        assert!(!out.timed_out);
        assert_eq!(out.confined, ConfineKind::Seatbelt);
        assert_eq!(out.verdict.verdict, Verdict::Allow);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["confined"], "seatbelt");
        assert_eq!(rec["decision"], "allow");
        assert_eq!(rec["exit_code"], 0);
        assert_eq!(rec["timed_out"], false);
        assert!(rec["cmd_sha256"].as_str().unwrap().len() == 64);
        assert!(rec["ts"].as_str().unwrap().contains('T'));
        assert_eq!(rec["session_id"], format!("sess-{}", std::process::id()));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn touch_home_blocked_by_confinement() {
        let tag = format!("atom-sbx-probe-{}-{}", std::process::id(), uuid_like(),);
        let probe = dirs::home_dir().unwrap().join(&tag);
        let _ = std::fs::remove_file(&probe);

        // Approver allows the Ask so the kernel layer is what stops us.
        let e = env();
        let out = run_in(
            &e,
            &format!("touch ~/{tag}"),
            &workspace_cfg(),
            &AutoApprover(Decision::AllowOnce),
        )
        .await;
        assert_ne!(
            out.exit_code, 0,
            "home write must fail confined: {:?}",
            out.stdout
        );
        assert!(
            !probe.exists(),
            "probe file must not exist after confined touch"
        );
        assert_eq!(out.confined, ConfineKind::Seatbelt);

        let _ = std::fs::remove_file(&probe);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn curl_blocked_at_approval_gate_when_not_approved() {
        let e = env();
        let started = Instant::now();
        let out = run_in(
            &e,
            "curl -sS --max-time 15 http://example.com -o /dev/null",
            &workspace_cfg(),
            &DenyAllApprover,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(out.verdict.uses_network);
        assert!(!out.approved);
        assert_ne!(out.exit_code, 0, "unapproved curl must not run");
        assert!(
            out.stderr.contains("not approved"),
            "stderr: {}",
            out.stderr
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "curl should fail fast at the gate, took {elapsed:?}"
        );
    }

    #[test]
    fn net_allowed_for_run_matrix() {
        use crate::policy::NetPolicy;
        let deny = SandboxConfig {
            network: NetPolicy::Deny,
            ..Default::default()
        };
        let ask = SandboxConfig {
            network: NetPolicy::Ask,
            ..Default::default()
        };
        let allow = SandboxConfig {
            network: NetPolicy::Allow,
            ..Default::default()
        };
        // Config baseline: Allow always grants network.
        assert!(net_allowed_for_run(&allow, false, false));
        assert!(net_allowed_for_run(&allow, true, true));
        // Non-network commands never gain egress, approved or not.
        assert!(!net_allowed_for_run(&deny, false, true));
        assert!(!net_allowed_for_run(&ask, false, false));
        // Network commands need approval: unapproved stay denied...
        assert!(!net_allowed_for_run(&deny, true, false));
        assert!(!net_allowed_for_run(&ask, true, false));
        // ...but approval grants egress for that run (the fix for the
        // approval prompt that could never actually reach the network).
        assert!(net_allowed_for_run(&deny, true, true));
        assert!(net_allowed_for_run(&ask, true, true));
    }

    #[tokio::test]
    async fn off_mode_runs_unsandboxed_and_audits() {
        let e = env();
        let cfg = SandboxConfig {
            mode: SandboxMode::Off,
            ..Default::default()
        };
        let out = run_in(&e, "echo off-run", &cfg, &DenyAllApprover).await;
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim_end(), "off-run");
        assert_eq!(out.confined, ConfineKind::None);
        assert!(!out.approved);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["decision"], "off");
        assert_eq!(rec["confined"], "none");
        assert_eq!(rec["exit_code"], 0);
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
        let out = run_in(&e, "sudo reboot", &workspace_cfg(), &Panic).await;
        assert_eq!(out.exit_code, -1);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("blocked by sandbox policy"));
        assert!(!out.approved);
        assert_eq!(out.confined, ConfineKind::None);

        let log = std::fs::read_to_string(e._data.path().join("sandbox-audit.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(rec["decision"], "deny");
        assert_eq!(rec["exit_code"], -1);
    }

    #[tokio::test]
    async fn ask_with_session_grant_skips_prompt_on_second_call() {
        let e = env();
        let cfg = workspace_cfg();
        let session = "ask-sess-1";
        let approver = AutoApprover(Decision::AllowSession);

        // First call: prompts (global store has no grant for this fresh key).
        let first = run_with(
            e._data.path(),
            "wget --version",
            e._ws.path(),
            e._ws.path(),
            session,
            &cfg,
            &approver,
        )
        .await;
        assert!(first.approved);

        // The grant recorded by call one lives in the process-global store.
        let rule_id = primary_rule_id(&first.verdict);
        let key = crate::approvals::key_for(&rule_id, "wget --version", e._ws.path());
        assert!(
            approval_store().check(session, &key),
            "session grant should be reusable"
        );
    }

    fn uuid_like() -> u128 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        ((std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos())
            << 8)
            | n as u128
    }
}
