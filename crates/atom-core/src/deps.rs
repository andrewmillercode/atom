//! Startup tool dependency check for the atom client (macOS).
//!
//! The grep/glob tools shell out to `rg`, vector search shells out to
//! `uvx`, and visualize shells out to `merman-cli`, so these must exist
//! on PATH before the server or TUI starts.
//! `ensure_on_startup` runs very early in main(), while the terminal is
//! still in its normal (non-raw) state, and offers to install anything
//! missing via Homebrew. Detection is a PATH scan plus a `--version`
//! probe, so it is cheap enough to run on every launch.

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One external binary the program needs at runtime.
pub struct ToolDep {
    /// Display name ("ripgrep").
    pub name: &'static str,
    /// Executable name ("rg").
    pub bin: &'static str,
    /// What the tool is used for (shown in the prompt).
    pub why: &'static str,
    /// Homebrew package name.
    pub brew: &'static str,
    /// crates.io crate name for the `cargo install` fallback, if the
    /// binary is published there.
    pub cargo: Option<&'static str>,
    /// Human instructions shown when the auto-install fails.
    pub manual: &'static str,
}

/// The runtime tool dependencies. Keep in sync with the executors in
/// atom-tools (search.rs uses `rg`, vector_search.rs uses `uvx`,
/// visualize.rs uses `merman-cli`).
pub const REQUIRED_TOOLS: [ToolDep; 3] = [
    ToolDep {
        name: "ripgrep",
        bin: "rg",
        why: "grep/glob file search",
        brew: "ripgrep",
        cargo: Some("ripgrep"),
        manual: "brew install ripgrep (or: cargo install ripgrep)",
    },
    ToolDep {
        name: "uv",
        bin: "uvx",
        why: "vector search",
        brew: "uv",
        cargo: None,
        manual: "brew install uv (or: curl -LsSf https://astral.sh/uv/install.sh | sh)",
    },
    ToolDep {
        name: "merman-cli",
        bin: "merman-cli",
        why: "diagram rendering (visualize)",
        brew: "merman-cli",
        cargo: Some("merman-cli"),
        manual: "brew install merman-cli (or: cargo install merman-cli)",
    },
];

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Finds `prog` by scanning the given PATH, returning the first hit.
pub fn find_in_path_with(prog: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(prog))
        .find(|candidate| candidate.is_file())
}

/// Finds `prog` on the process PATH.
pub fn find_in_path(prog: &str) -> Option<PathBuf> {
    match std::env::var_os("PATH") {
        Some(p) => find_in_path_with(prog, &p),
        None => None,
    }
}

/// Common macOS install locations that are not always on PATH
/// (`~/.local/bin` from the uv installer, `~/.cargo/bin`, ...).
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs
}

/// Finds `prog` on PATH, then in the common macOS install locations.
pub fn find_tool(bin: &str) -> Option<PathBuf> {
    if let Some(p) = find_in_path(bin) {
        return Some(p);
    }
    tool_dirs()
        .into_iter()
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// True when `path` runs and prints a `--version` (rules out broken
/// installs, e.g. a stale empty file on PATH).
pub async fn tool_works(path: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--version").kill_on_drop(true);
    match tokio::time::timeout(VERSION_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out.status.success() && !out.stdout.is_empty(),
        _ => false,
    }
}

/// Resolves a required tool to a working binary, if one exists.
async fn tool_resolved(dep: &ToolDep) -> bool {
    match find_tool(dep.bin) {
        Some(path) => tool_works(&path).await,
        None => false,
    }
}

/// The tools that are missing or broken right now.
pub async fn missing_tools() -> Vec<&'static ToolDep> {
    let mut missing = Vec::new();
    for dep in &REQUIRED_TOOLS {
        if !tool_resolved(dep).await {
            missing.push(dep);
        }
    }
    missing
}

/// Prompts `[Y/n]` on `out`, reading the answer from `input`.
/// Empty line or EOF defaults to yes; anything else re-prompts.
pub fn confirm(prompt: &str, out: &mut impl Write, input: &mut impl BufRead) -> bool {
    loop {
        let _ = write!(out, "{prompt} [Y/n] ");
        let _ = out.flush();
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => return true, // EOF: default yes
            Ok(_) => match line.trim().to_ascii_lowercase().as_str() {
                "" | "y" | "yes" => return true,
                "n" | "no" => return false,
                _ => continue,
            },
        }
    }
}

/// Installs a missing dep by running `prog args...` with a timeout.
pub async fn run_cmd(prog: &Path, args: &[String], timeout: Duration) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&[out.stdout, out.stderr].concat())
            .trim()
            .to_string()),
    }
}

/// Chooses how to install `dep` given the PATH `path`: Homebrew first
/// (user-owned, no sudo), then cargo for deps published on crates.io,
/// then the official uv installer script. Returns the argv to run, or
/// an error explaining what to do when no installer is available.
pub fn install_command(dep: &ToolDep, path: &OsStr) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(brew) = find_in_path_with("brew", path) {
        return Ok((brew, vec!["install".to_string(), dep.brew.to_string()]));
    }
    if let Some(crate_name) = dep.cargo {
        if let Some(cargo) = find_in_path_with("cargo", path) {
            return Ok((cargo, vec!["install".to_string(), crate_name.to_string()]));
        }
        return Err(format!(
            "no Homebrew or cargo on PATH; install Homebrew from https://brew.sh, then: {}",
            dep.manual
        ));
    }
    // uvx: the official installer installs to ~/.local/bin, no sudo.
    Ok((
        PathBuf::from("/bin/sh"),
        vec![
            "-c".to_string(),
            "curl -LsSf https://astral.sh/uv/install.sh | sh".to_string(),
        ],
    ))
}

/// Installs one missing tool. RealInstallers shell out to brew/cargo/uv;
/// tests inject a fake.
#[async_trait::async_trait]
pub trait Installer: Send + Sync {
    async fn install(&self, dep: &ToolDep) -> Result<(), String>;
}

/// The real installer: Homebrew, falling back to cargo / the uv script.
pub struct RealInstaller;

#[async_trait::async_trait]
impl Installer for RealInstaller {
    async fn install(&self, dep: &ToolDep) -> Result<(), String> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let (prog, args) = install_command(dep, &path)?;
        run_cmd(&prog, &args, INSTALL_TIMEOUT).await
    }
}

/// Runs the startup check. Called from main() before the server spawns
/// and before the TUI takes over the terminal, so the prompt appears on
/// a clean screen.
///
/// - `interactive`: when true (TTY client), prompt the user; when false
///   (headless `-serve`, piped stdin), only install when
///   `ATOM_DEPS_AUTOINSTALL=1` is set and otherwise just warn.
/// - `ATOM_SKIP_DEPS=1` bypasses the check entirely.
///
/// Never blocks startup fatally: failures degrade to the existing
/// "install X" tool errors.
pub async fn ensure_on_startup(interactive: bool, installer: &dyn Installer) {
    if std::env::var_os("ATOM_SKIP_DEPS").is_some() {
        return;
    }
    let missing = missing_tools().await;
    if missing.is_empty() {
        return;
    }

    let list = missing
        .iter()
        .map(|dep| format!("{} ({})", dep.name, dep.why))
        .collect::<Vec<_>>()
        .join(", ");
    println!("[atom] first-run setup: missing required tools — {list}");

    let auto = std::env::var("ATOM_DEPS_AUTOINSTALL").as_deref() == Ok("1");
    let want_install = if auto {
        true
    } else if interactive {
        confirm(
            "[atom] install missing tools now?",
            &mut std::io::stdout(),
            &mut std::io::stdin().lock(),
        )
    } else {
        false
    };

    if !want_install {
        let names = missing
            .iter()
            .map(|dep| dep.name)
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "[atom] continuing without {names}: the affected tools will report \
             unavailable until installed."
        );
        for dep in &missing {
            println!("[atom]   install later: {}", dep.manual);
        }
        return;
    }

    for dep in &missing {
        match installer.install(dep).await {
            Ok(()) if tool_resolved(dep).await => {
                println!("[atom] installed {} ({})", dep.name, dep.bin);
            }
            Ok(()) => {
                println!(
                    "[atom] {} installed but does not run; trying: {}",
                    dep.name, dep.manual
                );
            }
            Err(e) => {
                println!("[atom] failed to install {}: {e}", dep.name);
                println!("[atom]   install manually: {}", dep.manual);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_in_path_with_scans_dirs_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rg");
        std::fs::write(&bin, b"fake").unwrap();
        let joined =
            std::env::join_paths([dir.path(), std::path::Path::new("nonexistent")]).unwrap();
        assert_eq!(find_in_path_with("rg", &joined), Some(bin));
        assert_eq!(find_in_path_with("uvx", &joined), None);
    }

    #[test]
    fn find_tool_falls_back_to_install_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("uvx");
        std::fs::write(&bin, b"fake").unwrap();
        assert_eq!(find_in_dirs("uvx", &[dir.path().to_path_buf()]), Some(bin));
        assert_eq!(find_in_dirs("rg", &[dir.path().to_path_buf()]), None);
    }

    #[test]
    fn confirm_parses_answers() {
        let mut out = Vec::new();
        assert!(confirm("install?", &mut out, &mut &b"\n"[..]));
        assert!(confirm("install?", &mut out, &mut &b"y\n"[..]));
        assert!(confirm("install?", &mut out, &mut &b"YES\n"[..]));
        assert!(!confirm("install?", &mut out, &mut &b"n\n"[..]));
        assert!(!confirm("install?", &mut out, &mut &b"no\n"[..]));
        // Invalid input re-prompts, EOF defaults to yes.
        assert!(confirm("install?", &mut out, &mut &b"maybe\nyes\n"[..]));
        assert!(confirm("install?", &mut out, &mut &b""[..]));
        assert!(confirm("install?", &mut out, &mut &b"x\n"[..]));
    }

    #[test]
    fn tool_works_accepts_working_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rg");
        std::fs::write(&bin, "#!/bin/sh\necho 'ripgrep 14.1.0'\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let ok = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool_works(&bin));
        assert!(ok);
    }

    #[test]
    fn tool_works_rejects_broken_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rg");
        std::fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let ok = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool_works(&bin));
        assert!(!ok);
    }

    #[test]
    fn run_cmd_reports_success_and_failure() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sh = PathBuf::from("/bin/sh");
        let ok = rt.block_on(run_cmd(
            &sh,
            &["-c".into(), "exit 0".into()],
            Duration::from_secs(5),
        ));
        assert!(ok.is_ok());
        let bad = rt.block_on(run_cmd(
            &sh,
            &["-c".into(), "exit 3".into()],
            Duration::from_secs(5),
        ));
        assert!(bad.is_err());
    }

    #[test]
    fn run_cmd_times_out_and_kills() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sh = PathBuf::from("/bin/sh");
        let start = std::time::Instant::now();
        let res = rt.block_on(run_cmd(
            &sh,
            &["-c".into(), "sleep 30".into()],
            Duration::from_millis(100),
        ));
        assert!(res.is_err());
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn install_command_prefers_brew() {
        let dir = tempfile::tempdir().unwrap();
        let brew = dir.path().join("brew");
        std::fs::write(&brew, b"fake").unwrap();
        let path = dir.path().as_os_str();
        let (prog, args) = install_command(&REQUIRED_TOOLS[0], path).unwrap();
        assert_eq!(prog, brew);
        assert_eq!(args, vec!["install".to_string(), "ripgrep".to_string()]);
    }

    #[test]
    fn install_command_falls_back_for_uv_without_brew() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().as_os_str(); // empty dir: no brew, no cargo
        let (prog, args) = install_command(&REQUIRED_TOOLS[1], path).unwrap();
        assert_eq!(prog, PathBuf::from("/bin/sh"));
        assert!(args[1].contains("astral.sh/uv/install.sh"));
    }

    #[test]
    fn install_command_falls_back_to_cargo_for_merman() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("cargo");
        std::fs::write(&cargo, b"fake").unwrap();
        let path = dir.path().as_os_str(); // no brew on this PATH
        let (prog, args) = install_command(&REQUIRED_TOOLS[2], path).unwrap();
        assert_eq!(prog, cargo);
        assert_eq!(args, vec!["install".to_string(), "merman-cli".to_string()]);
    }

    #[test]
    fn install_command_errors_without_any_installer_for_rg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().as_os_str();
        let res = install_command(&REQUIRED_TOOLS[0], path);
        let err = res.unwrap_err();
        assert!(err.contains("Homebrew"), "unexpected: {err}");
    }

    // Small helper so the tests above don't touch the real HOME dirs.
    fn find_in_dirs(prog: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
        dirs.iter().map(|dir| dir.join(prog)).find(|p| p.is_file())
    }
}
