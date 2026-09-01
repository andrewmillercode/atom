//! Background job registry for commands outliving a single tool call.
//!
//! The bash tool auto-escalates long-running commands into background
//! jobs (see [`crate::exec`]); the `jobs` tool inspects them here. The
//! registry is process-global: the `atoms` server owns every tool call,
//! so process-global state is session state. Jobs are keyed by session
//! id and keep their child handle so `kill` reaches the whole process
//! group.

use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// How long a `jobs wait` blocks by default.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Max finished jobs kept per session before pruning (oldest first).
const MAX_FINISHED_PER_SESSION: usize = 32;

/// Tail of output returned by check/wait, in bytes.
const OUTPUT_TAIL_BYTES: usize = 6000;

struct Job {
    id: String,
    session_id: String,
    command: String,
    out_path: PathBuf,
    started: Instant,
    /// Taken out while reaping/killing; `None` means the child is
    /// currently checked out (the tool call is waiting on it inline)
    /// or the job already finished.
    child: Option<Child>,
    exit: Option<i32>,
    /// Set by flush_wait (a mid-turn prompt): the in-flight tool call
    /// should stop waiting and return this job's id immediately.
    flush: bool,
}

static JOBS: Lazy<Mutex<Vec<Job>>> = Lazy::new(|| Mutex::new(Vec::new()));
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_job_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mix = nanos.wrapping_mul(0x9E3779B9) ^ (n << 32) ^ std::process::id() as u64;
    format!("j-{:08x}", mix as u32)
}

/// Log directory for a session's background job output:
/// `<dataDir>/jobs/<session>/`. Created eagerly so the spawn can
/// redirect both streams into the log file.
fn jobs_dir(data_dir_path: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    let dir = data_dir_path.join("jobs");
    let dir = if safe.is_empty() { dir } else { dir.join(safe) };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Spawn `cmd` as `/bin/bash -lc` with both output streams streaming
/// into a per-job log file, in its own process group, and register the
/// job. This is the single spawn-and-register path: every bash command
/// is a job from the moment it spawns. The returned child is owned by
/// the caller while it waits for an inline result; [`attach`] hands
/// ownership to the registry for a job that keeps running, [`remove`]
/// drops the entry when the result was delivered inline (log cleanup
/// is the caller's, since it reads the tail first). Callers must have
/// run the sandbox pipeline (analyze + approval) already.
pub fn spawn_registered(
    data_dir_path: &Path,
    session_id: &str,
    command: &str,
    cwd: &Path,
    tmpdir_env: Option<&Path>,
) -> Result<(String, PathBuf, Child), String> {
    let id = new_job_id();
    let out_path = jobs_dir(data_dir_path, session_id).join(format!("{id}.log"));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .map_err(|e| format!("atom: failed to open job log: {e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("atom: failed to open job log: {e}"))?;

    let mut cmd = Command::new(crate::exec::BASH);
    cmd.arg("-lc").arg(command);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));
    cmd.env_clear();
    for (k, v) in crate::exec::scrub_env() {
        cmd.env(k, v);
    }
    if let Some(t) = tmpdir_env {
        cmd.env("TMPDIR", t);
        cmd.env("TMP", t);
        cmd.env("TEMP", t);
    }
    // Own process group so a group-wide SIGKILL (jobs kill / turn
    // cancel) reaches `bash -lc "cargo build"`'s grandchildren too.
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .map_err(|e| format!("atom: failed to spawn: {e}"))?;
    register(session_id, &id, command.to_string(), out_path.clone(), None);
    Ok((id, out_path, child))
}

/// Register a job entry. `child` arrives later via [`attach`]: the
/// tool call holds it while waiting for an inline result, and nobody
/// else can reach the job in that window (tools run sequentially per
/// turn and jobs are session-scoped).
fn register(session_id: &str, id: &str, command: String, out_path: PathBuf, child: Option<Child>) {
    let job = Job {
        id: id.to_string(),
        session_id: session_id.to_string(),
        command,
        out_path,
        started: Instant::now(),
        child,
        exit: None,
        flush: false,
    };
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    prune(&mut jobs, session_id);
    jobs.push(job);
}

/// Hand a still-running child to the registry: the tool call gave up
/// waiting (head start over, or a mid-turn prompt accelerated the
/// handoff) and returned the job id.
pub fn attach(id: &str, child: Child) {
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
        job.child = Some(child);
    }
}

/// Drop a job entry — the result was delivered inline or the command
/// was killed. The caller cleans up the log file (it read the tail).
pub fn remove(id: &str) {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|j| j.id != id);
}

/// Flag the session's running jobs as "return now": in-flight bash
/// tool calls stop waiting for an inline result and hand back their
/// job id immediately (prompt acceleration; the registry is the
/// coordination point between /send injection and the tool wait).
pub fn flush_wait(session_id: &str) {
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    for job in jobs.iter_mut() {
        if job.session_id == session_id && job.exit.is_none() {
            job.flush = true;
        }
    }
}

/// Whether a mid-turn prompt asked this job's tool call to return now.
pub fn flushed(id: &str) -> bool {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|j| j.id == id && j.flush)
}

/// Drop the oldest finished jobs of a session once the cap is reached.
/// Running jobs are never pruned.
fn prune(jobs: &mut Vec<Job>, session_id: &str) {
    let finished: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.session_id == session_id && j.exit.is_some())
        .map(|(i, _)| i)
        .collect();
    if finished.len() < MAX_FINISHED_PER_SESSION {
        return;
    }
    let excess = finished.len() - MAX_FINISHED_PER_SESSION;
    for idx in finished.into_iter().take(excess) {
        jobs.remove(idx);
    }
}

/// Non-blocking sweep: reap exited children so their exit codes stick.
fn reap(jobs: &mut [Job]) {
    for job in jobs.iter_mut() {
        if job.exit.is_some() {
            continue;
        }
        if let Some(child) = job.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                job.exit = Some(status.code().unwrap_or(-1));
            }
        }
    }
}

fn tail(out_path: &std::path::Path) -> String {
    match std::fs::read(out_path) {
        Ok(bytes) => {
            if bytes.len() > OUTPUT_TAIL_BYTES {
                let mut start = bytes.len() - OUTPUT_TAIL_BYTES;
                while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
                    start += 1; // don't split a UTF-8 sequence
                }
                return format!("…{}", String::from_utf8_lossy(&bytes[start..]));
            }
            String::from_utf8_lossy(&bytes).to_string()
        }
        Err(_) => String::new(),
    }
}

fn elapsed_label(job: &Job) -> String {
    let secs = job.started.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

fn indent_command(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
    }
    text.replace('\n', "\n  ")
}

fn status_line(job: &Job) -> String {
    let state = match job.exit {
        Some(code) => format!("exited {code}"),
        None => format!("running ({})", elapsed_label(job)),
    };
    format!(
        "{}\n  status: {}\n  command: {}\n  output:\n  {}",
        job.id,
        state,
        indent_command(job.command.clone()),
        tail(&job.out_path).trim_end()
    )
}

/// Indices of the caller's session's jobs (empty ids = all of them).
/// Assumes the JOBS lock is already held — call only under the lock.
fn selected_locked(jobs: &[Job], session_id: &str, ids: &[String]) -> Vec<usize> {
    jobs.iter()
        .enumerate()
        .filter(|(_, j)| j.session_id == session_id && (ids.is_empty() || ids.contains(&j.id)))
        .map(|(i, _)| i)
        .collect()
}

/// Status for the caller's session's jobs (empty ids = all of them).
pub fn check(session_id: &str, ids: &[String]) -> Vec<String> {
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    reap(&mut jobs);
    selected_locked(&jobs, session_id, ids)
        .into_iter()
        .map(|i| status_line(&jobs[i]))
        .collect()
}

/// Block until every selected job exits, or until `timeout` elapses.
/// Returns the final status lines either way.
pub async fn wait(session_id: &str, ids: &[String], timeout: Duration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
            reap(&mut jobs);
            let picks = selected_locked(&jobs, session_id, ids);
            let all_done = !picks.is_empty() && picks.iter().all(|i| jobs[*i].exit.is_some());
            if all_done {
                return picks.iter().map(|i| status_line(&jobs[*i])).collect();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return check(session_id, ids);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SIGKILL the job's whole process group, then reap the child.
async fn kill_child(child: &mut Child) -> i32 {
    let pid = child.id().unwrap_or(0) as i32;
    if pid > 0 {
        // Negative pid targets the process group (the child was spawned
        // with process_group(0), so its pgid == its pid).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    -9
}

/// Kill selected jobs (empty ids = all of the session's). Returns one
/// status block per job with its final output tail.
pub async fn kill(session_id: &str, ids: &[String]) -> Vec<String> {
    // Check the children out under the lock, kill them outside it
    // (std::sync::MutexGuard must never be held across an await).
    let checked_out: Vec<(usize, Child)> = {
        let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
        reap(&mut jobs);
        selected_locked(&jobs, session_id, ids)
            .into_iter()
            .filter_map(|i| {
                if jobs[i].exit.is_some() {
                    return None;
                }
                jobs[i].child.take().map(|child| (i, child))
            })
            .collect()
    };
    let mut exited: Vec<(usize, i32)> = Vec::new();
    for (i, mut child) in checked_out {
        let code = kill_child(&mut child).await;
        exited.push((i, code));
    }
    let mut results: Vec<String> = Vec::new();
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    for i in selected_locked(&jobs, session_id, ids) {
        match exited.iter().find(|(idx, _)| *idx == i) {
            Some((_, code)) => {
                jobs[i].exit = Some(*code);
                results.push(format!(
                    "{}\n  status: killed ({})\n  command: {}\n  output:\n  {}",
                    jobs[i].id,
                    code,
                    indent_command(jobs[i].command.clone()),
                    tail(&jobs[i].out_path).trim_end()
                ));
            }
            None => results.push(status_line(&jobs[i])),
        }
    }
    results
}
