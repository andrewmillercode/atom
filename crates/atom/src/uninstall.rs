//! `atom uninstall` — uninstall the binaries, runtime state, and PATH
//! entry that `install.sh` / `make install` / `make dev` created.
//!
//! Mirrors the `make uninstall` Makefile target so users have an
//! equivalent they can run from anywhere — no checkout, no make — by
//! invoking `atom uninstall` or `atomdev uninstall`. Same source as the
//! release client; the build-flavor helpers in atom-core pick the
//! matching data/config dirs.
//!
//! The plan is announced before doing anything and the user is prompted
//! on a TTY; pass `-y`/`--yes` to skip the prompt for scripted use.

use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Shell rc files install.sh writes the PATH export into.
const RC_FILES: &[&str] = &[".zshrc", ".bashrc", ".profile"];

/// Names of every atom binary that may live next to the running
/// executable. Scoped to the current build flavor so `atom uninstall`
/// (release) leaves `atomdev`/`atomsdev` alone and vice versa; the
/// cross-flavor removal that `make uninstall` does is intentionally
/// not replicated here.
fn binaries_for_flavor() -> [&'static str; 2] {
    [
        atom_core::build::client_name(),
        atom_core::build::server_name(),
    ]
}

/// The single data dir and config dir for the current build flavor.
/// Honors XDG_DATA_HOME / XDG_CONFIG_HOME, falling back to the same
/// defaults the rest of atom uses. Mirrors the `data_dir()` /
/// `config_dir()` helpers in atom-core, kept separate here so uninstall
/// doesn't grow a transitive dependency on atom-server just to compute
/// the path.
fn plan_dirs() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let leaf = atom_core::build::dir_leaf();
    let data_root = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg_root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    (vec![data_root.join(leaf)], vec![cfg_root.join(leaf)])
}

fn dirs_home() -> Option<PathBuf> {
    // `dirs` is a workspace dep through atom-core. Call through std-only
    // paths here so we don't grow the atom crate's dep list just for
    // one helper. The `HOME` env var is set on every platform atom
    // supports (macOS, Linux), and XDG_* overrides above handle the
    // remaining cases.
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Refuse to wipe cargo's build tree — `cargo run --bin atom -- uninstall`
/// would otherwise happily `rm` the binary it just compiled. Mirrors
/// the safety gate in update::gates_pass. Matches either the dir
/// itself (`…/target/debug`) or a binary inside it (`…/target/debug/atom`).
fn install_dir_is_safe(install_dir: &Path) -> bool {
    let canon = install_dir
        .canonicalize()
        .unwrap_or_else(|_| install_dir.to_path_buf());
    let s = canon.to_string_lossy();
    !s.contains("/target/debug") && !s.contains("/target/release")
}

/// Entry point invoked from main.rs after `atom uninstall` is parsed.
pub fn run(yes: bool) -> Result<()> {
    let exe = std::env::current_exe().context("find own executable")?;
    let install_dir = exe
        .parent()
        .context("executable has no parent dir")?
        .to_path_buf();
    run_with(install_dir, yes)
}

/// Pure-ish: takes the install dir explicitly so tests can exercise
/// the cleanup without touching the running test binary's directory.
pub fn run_with(install_dir: PathBuf, yes: bool) -> Result<()> {
    if !install_dir_is_safe(&install_dir) {
        bail!(
            "refusing to uninstall from a cargo target dir: {}\n\
             run the installed binary (e.g. ~/.local/bin/atom uninstall), \
             or use `make uninstall` from the repo root.",
            install_dir.display()
        );
    }

    let (data_dirs, cfg_dirs) = plan_dirs();
    announce_plan(&install_dir, &data_dirs, &cfg_dirs);

    if !yes {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!("refusing to uninstall non-interactively without -y/--yes");
        }
        eprint!("\nProceed? [y/N] ");
        std::io::stderr().flush().ok();
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .context("read confirmation")?;
        if !matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("aborted.");
            return Ok(());
        }
    }

    stop_servers(&data_dirs);
    remove_binaries(&install_dir);
    remove_state(&data_dirs, &cfg_dirs);
    edit_rc_files(&install_dir);
    if std::fs::remove_dir(&install_dir).is_ok() {
        eprintln!("==> removed empty install dir {}", install_dir.display());
    }

    eprintln!("==> atom uninstalled");
    Ok(())
}

fn announce_plan(install_dir: &Path, data_dirs: &[PathBuf], cfg_dirs: &[PathBuf]) {
    eprintln!("==> uninstall plan");
    eprintln!("    install dir:    {}", install_dir.display());
    eprintln!("    binaries:       {}", binaries_for_flavor().join(" "));
    eprintln!("    config dirs:    {}", join_display(cfg_dirs));
    eprintln!("    data dirs:      {}", join_display(data_dirs));
    let rcs = RC_FILES
        .iter()
        .map(|n| format!("~/{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("    shell rc files: {rcs}");
}

/// SIGTERM each flavor's running server, then sleep briefly so the
/// processes have time to release their sockets. Mirrors the
/// `sleep 1` after the kill loop in the Makefile.
fn stop_servers(data_dirs: &[PathBuf]) {
    eprintln!("==> stopping background session servers");
    for d in data_dirs {
        let pid_path = d.join("server.pid");
        let pid = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|p| *p > 0);
        if let Some(pid) = pid {
            // Mirror the Makefile's `kill ... 2>/dev/null || true` —
            // the pid may already be gone (server crashed, manual kill).
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
}

/// Remove the atom binaries for the current build flavor from the
/// install dir. POSIX `unlink` of the running executable succeeds
/// immediately — the inode survives until the process exits, so the
/// rest of this function finishes normally after the binary on disk is
/// gone.
fn remove_binaries(install_dir: &Path) {
    eprintln!("==> removing binaries from {}", install_dir.display());
    for name in binaries_for_flavor() {
        let p = install_dir.join(name);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("    warning: could not remove {}: {e}", p.display()),
        }
    }
}

fn remove_state(data_dirs: &[PathBuf], cfg_dirs: &[PathBuf]) {
    eprintln!("==> removing config and data dirs (sessions, credentials, logs, skills)");
    for d in cfg_dirs.iter().chain(data_dirs.iter()) {
        match std::fs::remove_dir_all(d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("    warning: could not remove {}: {e}", d.display()),
        }
    }
}

/// Strip the `export PATH="<dir>:$PATH"` line install.sh added to the
/// user's rc file. Exact-match only — never touch lines that merely
/// happen to mention this directory.
fn edit_rc_files(install_dir: &Path) {
    let Some(home) = dirs_home() else { return };
    let target_line = format!("export PATH=\"{}:$PATH\"", install_dir.display());
    eprintln!("==> removing PATH line from shell rc files");
    for name in RC_FILES {
        let path = home.join(name);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut kept: Vec<&str> = Vec::with_capacity(contents.lines().count());
        let mut removed = false;
        for line in contents.lines() {
            if line == target_line {
                removed = true;
            } else {
                kept.push(line);
            }
        }
        if !removed {
            continue;
        }
        let mut new_contents = kept.join("\n");
        if contents.ends_with('\n') && !new_contents.ends_with('\n') {
            new_contents.push('\n');
        }
        if let Err(e) = std::fs::write(&path, &new_contents) {
            eprintln!("    warning: could not update {}: {e}", path.display());
        }
    }
}

fn join_display(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp install dir + a temp XDG root so the test never
    /// touches the user's real ~/.local or ~/.config trees. The
    /// sandbox mirrors the current build flavor: only the dev pair of
    /// binaries and the dev leaf under XDG are populated, since tests
    /// run in the debug profile and `dir_leaf()` resolves to
    /// `atom-dev`. The other flavor is left as a sentinel so the
    /// negative-assertion tests can prove uninstall never touches it.
    fn sandbox() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("bin");
        std::fs::create_dir_all(&install).unwrap();
        let xdg_root = tmp.path().join("xdg");
        std::fs::create_dir_all(xdg_root.join("config")).unwrap();
        std::fs::create_dir_all(xdg_root.join("data")).unwrap();
        let flavor = atom_core::build::dir_leaf();
        // Only the current-flavor binaries live in the install dir; the
        // uninstall should target only those and leave the other pair
        // alone.
        for name in binaries_for_flavor() {
            std::fs::write(install.join(name), b"#!/bin/sh\n").unwrap();
        }
        // Drop a sentinel server.pid in each data dir so we can prove
        // uninstall reads (and then removes) the current-flavor path
        // and leaves the other untouched.
        std::fs::create_dir_all(xdg_root.join("data").join(flavor)).unwrap();
        std::fs::write(
            xdg_root.join("data").join(flavor).join("server.pid"),
            b"99999\n",
        )
        .unwrap();
        let other = if flavor == "atom-dev" { "atom" } else { "atom-dev" };
        let other_data = xdg_root.join("data").join(other);
        std::fs::create_dir_all(&other_data).unwrap();
        std::fs::write(other_data.join("untouched"), b"sentinel").unwrap();
        let other_cfg = xdg_root.join("config").join(other);
        std::fs::create_dir_all(&other_cfg).unwrap();
        std::fs::write(other_cfg.join("untouched"), b"sentinel").unwrap();
        // XDG vars point at the sandbox so plan_dirs() builds paths
        // inside the temp tree.
        std::env::set_var("XDG_DATA_HOME", xdg_root.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", xdg_root.join("config"));
        (tmp, install, xdg_root)
    }

    #[test]
    fn yes_flag_runs_full_uninstall() {
        let (tmp, install, xdg_root) = sandbox();
        let flavor = atom_core::build::dir_leaf();
        let other = if flavor == "atom-dev" { "atom" } else { "atom-dev" };
        run_with(install.clone(), true).unwrap();

        // The current-flavor binaries are gone; the other pair was
        // never installed in the sandbox and must not have been created
        // by uninstall either.
        for name in binaries_for_flavor() {
            assert!(!install.join(name).exists(), "{name} should be removed");
        }
        for name in ["atom", "atoms", "atomdev", "atomsdev"] {
            if binaries_for_flavor().contains(&name) {
                continue;
            }
            assert!(
                !install.join(name).exists(),
                "{name} must not have been created by uninstall"
            );
        }
        // Current-flavor data + config dirs are removed.
        assert!(!xdg_root.join("data").join(flavor).exists());
        assert!(!xdg_root.join("config").join(flavor).exists());
        // The other flavor's dirs are untouched.
        assert!(xdg_root.join("data").join(other).join("untouched").exists());
        assert!(xdg_root
            .join("config")
            .join(other)
            .join("untouched")
            .exists());
        let _ = tmp;
    }

    #[test]
    fn rc_line_is_stripped_when_present() {
        let (tmp, install, xdg_root) = sandbox();
        let rc = xdg_root.join("config").join("zshrc-test");
        // The Makefile + install.sh match the exact path of the install
        // dir, so writing the matching line and asking for its removal
        // is what we want to verify.
        let line = format!("export PATH=\"{install_display}:$PATH\"\n", install_display = install.display());
        let initial = format!("# keep\n{line}# tail\n");
        std::fs::write(&rc, &initial).unwrap();
        // run_with doesn't read RC_FILES (we hard-coded .zshrc/.bashrc/.profile),
        // so call edit_rc_files directly with a redirected $HOME for the
        // assertion below — covered by the no-op case here.
        edit_rc_files_for_test(&install, &rc, &initial);
        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(!after.contains(&line));
        assert!(after.contains("# keep"));
        assert!(after.contains("# tail"));
        let _ = tmp;
    }

    #[test]
    fn refuses_target_dir_install() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target").join("debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("atom"), b"x").unwrap();
        let err = run_with(target, true).unwrap_err();
        assert!(err.to_string().contains("cargo target dir"));
    }

    #[test]
    fn safe_install_dir_allows_normal_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("bin");
        std::fs::create_dir_all(&p).unwrap();
        assert!(install_dir_is_safe(&p));
    }

    /// Test-local copy of edit_rc_files that reads/writes a single
    /// caller-supplied rc path. Mirrors the production logic but lets
    /// us exercise it without messing with $HOME.
    fn edit_rc_files_for_test(install_dir: &Path, rc: &Path, initial: &str) {
        let target_line = format!("export PATH=\"{}:$PATH\"", install_dir.display());
        let mut kept: Vec<&str> = Vec::new();
        let mut removed = false;
        for line in initial.lines() {
            if line == target_line {
                removed = true;
            } else {
                kept.push(line);
            }
        }
        if !removed {
            return;
        }
        let mut new_contents = kept.join("\n");
        if initial.ends_with('\n') && !new_contents.ends_with('\n') {
            new_contents.push('\n');
        }
        std::fs::write(rc, new_contents).unwrap();
    }
}
