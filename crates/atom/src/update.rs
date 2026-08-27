//! Startup auto-updater for the `atom` client.
//!
//! On launch (before the deps check and TUI), atom checks GitHub for a
//! newer release and, if one exists, downloads and installs it, then
//! re-execs itself. All failures are non-fatal: the existing binary is
//! used and startup continues. A persistent store under the atom data
//! dir tracks the last check, the latest version seen, the version we
//! attempted, and the consecutive failure count, so a broken release is
//! blacklisted after three failures and a version is only ever attempted
//! once (loop breaker).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a cached "latest" version is trusted before re-checking.
const TTL_SECS: u64 = 1800;
/// How old a stale lock file must be before it is stolen.
const LOCK_STALE_SECS: u64 = 600;
/// Consecutive failures before a version is blacklisted.
const MAX_FAILURES: u32 = 3;

const RELEASES_URL: &str = "https://api.github.com/repos/andrewmillercode/atom/releases/latest";
const USER_AGENT: &str = "atom";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateState {
    #[serde(default)]
    checked_at: u64,
    #[serde(default)]
    latest_seen: String,
    #[serde(default)]
    attempted: String,
    #[serde(default)]
    failures: u32,
}

enum Decision {
    Run,
    Blacklisted,
    Attempt,
}

enum Lock {
    Acquired,
    Held,
}

/// Entry point called from main.rs as the first startup step. Never
/// aborts startup: any failure falls through to the existing binary.
pub async fn run() {
    if !gates_pass() {
        return;
    }
    let current = env!("CARGO_PKG_VERSION");
    let mut state = load_state();
    let now = unix_now();

    // Resolve the latest version, reusing the cached one within the TTL.
    let (latest, release) = if ttl_valid(&state, now) {
        (state.latest_seen.clone(), None)
    } else {
        match fetch_latest_release().await {
            Some(release) => {
                let Some(tag) = release.get("tag_name").and_then(|t| t.as_str()) else {
                    return;
                };
                let latest = tag.trim_start_matches('v').to_string();
                if latest.is_empty() {
                    return;
                }
                state.latest_seen = latest.clone();
                state.checked_at = now;
                let _ = save_state(&state);
                (latest, Some(release))
            }
            None => return, // network failure → continue with the existing binary
        }
    };

    match decide(current, &latest, &state.attempted, state.failures) {
        Decision::Run | Decision::Blacklisted => return,
        Decision::Attempt => {}
    }

    match attempt_update(&mut state, &latest, release.as_ref()).await {
        Ok(true) => {
            let exe = std::env::current_exe().unwrap_or_default();
            relaunch(&exe);
        }
        Ok(false) => {} // lock held by another updater → continue startup
        Err(e) => {
            state.failures = state.failures.saturating_add(1);
            let _ = save_state(&state);
            eprintln!("atom: auto-update failed: {e:#}");
        }
    }
}

/// Gates that skip the updater entirely (silently, no network).
fn gates_pass() -> bool {
    // Dev build: canonicalized path under target/debug or target/release.
    // Also protects `make install-dev` symlinks into ~/.local/bin.
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(canon) = exe.canonicalize() {
            let s = canon.to_string_lossy();
            if s.contains("/target/debug/") || s.contains("/target/release/") {
                return false;
            }
        }
    }
    if std::env::var("ATOM_NO_AUTOUPDATE").as_deref() == Ok("1") {
        return false;
    }
    if !atom_core::config::load().resolved_auto_update() {
        return false;
    }
    true
}

/// Whether the cached latest version is still fresh enough to reuse.
fn ttl_valid(state: &UpdateState, now: u64) -> bool {
    state.checked_at > 0
        && now.saturating_sub(state.checked_at) < TTL_SECS
        && parse_version(&state.latest_seen).is_some()
}

/// Fetches the latest release metadata from GitHub. Returns None on any
/// failure (network, non-200, bad JSON).
async fn fetch_latest_release() -> Option<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(RELEASES_URL)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

/// The attempt sequence: write `attempted`, take the lock, download,
/// extract, validate, install. Returns Ok(true) when the update is
/// installed and the caller should relaunch, Ok(false) when another
/// updater holds the lock (skip), and Err on any failure in the
/// download/install steps.
async fn attempt_update(
    state: &mut UpdateState,
    latest: &str,
    release: Option<&serde_json::Value>,
) -> Result<bool> {
    let update_dir = data_dir().join("update");
    let stage_dir = update_dir.join("stage");
    let lock_path = update_dir.join("lock");

    // a. Record the attempted version before any download/network work so
    //    a crash mid-update still counts as "attempted" (once-per-version).
    state.attempted = latest.to_string();
    save_state(state)?;

    // b. Lock. A fresh lock means another updater is running; skip.
    match acquire_lock(&lock_path)? {
        Lock::Held => return Ok(false),
        Lock::Acquired => {}
    }

    // c. Print + download the tarball into the stage dir.
    let current = env!("CARGO_PKG_VERSION");
    eprintln!("atom: updating v{current} → v{latest}…");
    std::fs::create_dir_all(&stage_dir)?;
    let url = download_url(release, latest);
    let archive = stage_dir.join(format!(
        "atom-v{latest}-{}-{}.tar.gz",
        os_name(),
        arch_name()
    ));
    download(&url, &archive).await?;

    // d. Extract with the system tar binary.
    let status = std::process::Command::new("tar")
        .args([
            "-xzf",
            archive.to_str().context("archive path not UTF-8")?,
            "-C",
            stage_dir.to_str().context("stage dir not UTF-8")?,
        ])
        .status()
        .context("run tar")?;
    if !status.success() {
        anyhow::bail!("tar extraction failed");
    }

    // e. Validate the staged binary reports the target version.
    let out = std::process::Command::new(stage_dir.join("atom"))
        .arg("-version")
        .output()
        .context("run staged atom -version")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains(latest) {
        anyhow::bail!("staged binary reports a different version");
    }

    // f. Install both binaries over the live ones.
    let exe = std::env::current_exe().context("find own executable")?;
    let exe_dir = exe.parent().context("executable has no parent dir")?;
    install_binaries(&stage_dir, exe_dir)?;

    // g. Mark success and let the caller relaunch.
    state.failures = 0;
    save_state(state)?;
    eprintln!("atom: updated to v{latest}, restarting…");
    Ok(true)
}

/// Re-exec the canonicalized current executable with the original args.
/// Only returns if exec fails.
fn relaunch(exe: &Path) {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("atom: failed to relaunch: {err}");
}

/// Take the update lock, stealing a stale one. Returns Held when a fresh
/// lock exists (another updater is running).
fn acquire_lock(lock_path: &Path) -> std::io::Result<Lock> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(_) => Ok(Lock::Acquired),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let age = std::fs::metadata(lock_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .unwrap_or_default();
            if age > Duration::from_secs(LOCK_STALE_SECS) {
                // Stale lock from a crashed updater: steal it.
                std::fs::remove_file(lock_path)?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(lock_path)?;
                Ok(Lock::Acquired)
            } else {
                Ok(Lock::Held)
            }
        }
        Err(e) => Err(e),
    }
}

/// Resolve the tarball download URL. Prefers the matching asset from the
/// release metadata; falls back to constructing the URL from the tag.
fn download_url(release: Option<&serde_json::Value>, latest: &str) -> String {
    let suffix = asset_suffix();
    if let Some(release) = release {
        if let Some(assets) = release.get("assets").and_then(|a| a.as_array()) {
            for asset in assets {
                let name = asset.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name.ends_with(&suffix) {
                    if let Some(url) = asset.get("browser_download_url").and_then(|u| u.as_str()) {
                        return url.to_string();
                    }
                }
            }
        }
    }
    format!(
        "https://github.com/andrewmillercode/atom/releases/download/v{latest}/atom-v{latest}{suffix}"
    )
}

/// The tarball asset-name suffix, e.g. "-Darwin-arm64.tar.gz".
fn asset_suffix() -> String {
    format!("-{}-{}.tar.gz", os_name(), arch_name())
}

fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else {
        "Linux"
    }
}

fn arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    }
}

/// Download a URL to a file with a generous timeout.
async fn download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let resp = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;
    let bytes = resp.bytes().await?;
    std::fs::write(dest, bytes)?;
    Ok(())
}

/// Move both staged binaries over the live ones. POSIX rename over a
/// running binary is safe; if the stage dir is on a different filesystem
/// (EXDEV), fall back to copy + rename per binary.
fn install_binaries(stage: &Path, exe_dir: &Path) -> std::io::Result<()> {
    for name in ["atom", "atoms"] {
        let staged = stage.join(name);
        let live = exe_dir.join(name);
        match std::fs::rename(&staged, &live) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                // Different filesystem: copy to a temp file next to the
                // target, then rename. Never write the live path directly:
                // copy+truncate is non-atomic and ETXTBSYs on Linux when
                // the binary is running.
                let tmp = exe_dir.join(format!(".{}.new-{}", name, std::process::id()));
                std::fs::copy(&staged, &tmp)?;
                std::fs::rename(&tmp, &live)?;
                std::fs::remove_file(&staged)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Persistent store.
// ---------------------------------------------------------------------------

fn state_path() -> PathBuf {
    data_dir().join("update").join("state.json")
}

fn data_dir() -> PathBuf {
    atom_core::session::store::data_dir()
}

fn load_state() -> UpdateState {
    load_state_from(&state_path())
}

fn load_state_from(path: &Path) -> UpdateState {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return UpdateState::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_state(state: &UpdateState) -> std::io::Result<()> {
    save_state_to(&state_path(), state)
}

/// Atomic write: temp file + rename, mirroring config.rs save_to.
fn save_state_to(path: &Path, state: &UpdateState) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state path has no parent",
        ));
    };
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(state)?;
    let tmp = parent.join(format!(
        ".state.json.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
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

// ---------------------------------------------------------------------------
// Version comparison and decision (pure, unit-testable).
// ---------------------------------------------------------------------------

/// Parse a version string into up to three u64 components, missing = 0.
/// Any non-numeric component in the first three makes it incomparable
/// (None).
fn parse_version(v: &str) -> Option<[u64; 3]> {
    let v = v.trim_start_matches('v');
    let mut parts = v.split('.');
    let mut out = [0u64; 3];
    for slot in out.iter_mut() {
        match parts.next() {
            Some(p) => *slot = p.parse().ok()?,
            None => break,
        }
    }
    Some(out)
}

fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let a = parse_version(a)?;
    let b = parse_version(b)?;
    Some(a.cmp(&b))
}

/// Decide what to do given the current version and the store state.
fn decide(current: &str, latest_seen: &str, attempted: &str, failures: u32) -> Decision {
    let Some(cmp) = compare_versions(current, latest_seen) else {
        // Incomparable → skip the update.
        return Decision::Run;
    };
    if cmp != Ordering::Less {
        // latest <= current → up to date.
        return Decision::Run;
    }
    // latest > current.
    if latest_seen == attempted && failures == 0 {
        // LOOP BREAKER: the previous exec did not take effect; retrying
        // would loop install→exec forever. This is the once-per-version
        // guarantee. (A crash between writing `attempted` and the download
        // also lands here — accepted, conservative.)
        return Decision::Run;
    }
    if failures >= MAX_FAILURES {
        // Blacklisted: skip this version until a newer latest_seen appears.
        return Decision::Blacklisted;
    }
    Decision::Attempt
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let unique = format!(
            "atom-update-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn version_compare() {
        assert_eq!(compare_versions("0.1.1", "0.1.1"), Some(Ordering::Equal));
        assert_eq!(compare_versions("0.1.1", "0.1.2"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.1.2", "0.1.1"), Some(Ordering::Greater));
        // Leading 'v' is stripped.
        assert_eq!(compare_versions("v0.1.1", "0.1.1"), Some(Ordering::Equal));
        // Missing components count as 0.
        assert_eq!(compare_versions("0.1", "0.1.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1", "1.0.0"), Some(Ordering::Equal));
        // Extra components beyond the third are ignored.
        assert_eq!(compare_versions("0.1.1.9", "0.1.1"), Some(Ordering::Equal));
        // Non-numeric → incomparable.
        assert_eq!(compare_versions("0.1.x", "0.1.1"), None);
        assert_eq!(compare_versions("0.1.1", "latest"), None);
    }

    #[test]
    fn decide_up_to_date() {
        // latest <= current → Run (no update).
        assert!(matches!(decide("0.1.1", "0.1.0", "", 0), Decision::Run));
        assert!(matches!(decide("0.1.1", "0.1.1", "", 0), Decision::Run));
        // Incomparable → Run.
        assert!(matches!(decide("0.1.1", "garbage", "", 0), Decision::Run));
    }

    #[test]
    fn decide_loop_breaker() {
        // latest > current, latest == attempted, failures == 0 → Run.
        assert!(matches!(
            decide("0.1.0", "0.1.1", "0.1.1", 0),
            Decision::Run
        ));
    }

    #[test]
    fn decide_blacklisted() {
        // latest > current, failures >= 3 → Blacklisted.
        assert!(matches!(
            decide("0.1.0", "0.1.1", "0.1.0", 3),
            Decision::Blacklisted
        ));
        assert!(matches!(
            decide("0.1.0", "0.1.1", "0.1.1", 3),
            Decision::Blacklisted
        ));
    }

    #[test]
    fn decide_attempt() {
        // latest > current, not loop-breaker, failures < 3 → Attempt.
        assert!(matches!(
            decide("0.1.0", "0.1.1", "0.1.0", 0),
            Decision::Attempt
        ));
        assert!(matches!(
            decide("0.1.0", "0.1.1", "0.1.0", 2),
            Decision::Attempt
        ));
    }

    #[test]
    fn store_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("update/state.json");
        let state = UpdateState {
            checked_at: 1234,
            latest_seen: "0.1.1".into(),
            attempted: "0.1.1".into(),
            failures: 2,
        };
        save_state_to(&path, &state).unwrap();
        assert_eq!(load_state_from(&path), state);
        // No stray temp files left behind.
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        // Missing file → empty default.
        assert_eq!(
            load_state_from(&dir.join("nope.json")),
            UpdateState::default()
        );
        // Corrupt file → empty default.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_state_from(&path), UpdateState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_name_mapping() {
        let suffix = asset_suffix();
        assert!(suffix.starts_with('-'));
        assert!(suffix.ends_with(".tar.gz"));
        // The suffix must match the platform's os/arch names.
        let expected_os = if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        };
        let expected_arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        assert_eq!(suffix, format!("-{expected_os}-{expected_arch}.tar.gz"));

        // download_url picks the matching asset from the release metadata.
        let release = serde_json::json!({
            "assets": [
                {"name": "atom-v0.1.1-Linux-x86_64.tar.gz", "browser_download_url": "https://x/linux"},
                {"name": format!("atom-v0.1.1{suffix}"), "browser_download_url": "https://x/match"},
            ]
        });
        assert_eq!(download_url(Some(&release), "0.1.1"), "https://x/match");

        // Fallback constructs the URL from the tag when no asset matches.
        assert_eq!(
            download_url(None, "0.1.1"),
            format!("https://github.com/andrewmillercode/atom/releases/download/v0.1.1/atom-v0.1.1{suffix}")
        );
    }
}
