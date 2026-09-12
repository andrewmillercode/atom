//! Seatbelt (`sandbox-exec`) confinement for bash commands.
//!
//! The profile denies everything, then grants what a local development
//! command needs: reads anywhere, writes inside the workspace, the
//! session scratch dir, system temp, and toolchain caches, plus
//! outbound network. Anti-self-escalation paths (`.git/hooks`,
//! `.git/config`, `$PATH` bins) are carved back out with later deny
//! rules; Seatbelt resolves a conflicting match by the last rule.
//!
//! macOS only. [`available`] is false elsewhere and callers fall back
//! to unconfined execution.

use std::path::{Path, PathBuf};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Whether this host can confine a command.
pub fn available() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists()
}

/// The Seatbelt profile for one command rooted at `workspace_root`
/// with an optional per-session scratch dir and any extra write roots
/// the command's own arguments resolve to (see `Analysis::outside_paths`).
pub fn profile(workspace_root: &Path, tmpdir: Option<&Path>, extra_writable: &[PathBuf]) -> String {
    let workspace = resolve(workspace_root);
    let mut write_roots = vec![
        workspace.clone(),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/var/folders"),
    ];
    if let Some(t) = tmpdir {
        write_roots.push(resolve(t));
    }
    if let Some(host_tmp) = std::env::var_os("TMPDIR").filter(|s| !s.is_empty()) {
        write_roots.push(resolve(&PathBuf::from(host_tmp)));
    }
    for path in extra_writable {
        write_roots.push(resolve(path));
    }
    // Toolchain and package-manager caches: installs and builds write
    // here with no path argument for static analysis to see.
    if let Some(home) = dirs::home_dir() {
        for rel in [
            ".cargo",
            ".rustup",
            ".cache",
            ".npm",
            ".bun",
            ".gradle",
            ".m2",
            ".nuget",
            ".gem",
            ".bundle",
            ".pyenv",
            ".rbenv",
            ".asdf",
            "go",
            "Library/Caches",
            "Library/pnpm",
        ] {
            write_roots.push(resolve(&home.join(rel)));
        }
    }
    for dir in [
        "/opt/homebrew/Cellar",
        "/opt/homebrew/opt",
        "/opt/homebrew/var",
        "/opt/homebrew/lib",
        "/opt/homebrew/etc",
        "/opt/homebrew/share",
        "/usr/local/Cellar",
        "/usr/local/opt",
        "/usr/local/var",
        "/usr/local/lib",
        "/usr/local/etc",
        "/usr/local/share",
    ] {
        write_roots.push(PathBuf::from(dir));
    }

    let mut protected = vec![workspace.join(".git/hooks"), workspace.join(".git/config")];
    // Every $PATH entry is a shim surface: writing one turns a later
    // bare `cargo` into attacker code. The toolchain dirs above are on
    // $PATH in a normal install, so this is what closes that hole.
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for entry in path_var.to_string_lossy().split(':') {
        if !entry.is_empty() {
            protected.push(resolve(Path::new(entry)));
        }
    }
    if let Some(home) = dirs::home_dir() {
        protected.push(home.join(".local/bin"));
        // Never writable, even when the command names them: a granted
        // path must not become a way to plant a hook, a shell startup
        // line, or a credential.
        protected.push(home.join(".ssh"));
        for rc in [
            ".zshrc",
            ".zshenv",
            ".zprofile",
            ".zlogin",
            ".bashrc",
            ".bash_profile",
            ".profile",
            ".config/fish",
        ] {
            protected.push(home.join(rc));
        }
    }
    protected.push(atom_core::config::config_dir());
    protected.push(atom_core::session::store::data_dir());
    protected.push(PathBuf::from("/opt/homebrew/bin"));
    protected.push(PathBuf::from("/usr/local/bin"));

    let mut out = String::from("(version 1)\n(deny default)\n");
    out.push_str("(allow process-exec*)\n");
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow file-read*)\n");
    out.push_str("(allow file-write*\n");
    for root in &write_roots {
        out.push_str(&format!("  (subpath {})\n", quote(root)));
    }
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/tty",
        "/dev/dtracehelper",
    ] {
        out.push_str(&format!("  (literal {})\n", quote(Path::new(dev))));
    }
    out.push_str("  (subpath \"/dev/fd\"))\n");
    out.push_str("(deny file-write*\n");
    for path in &protected {
        out.push_str(&format!("  (subpath {})\n", quote(path)));
    }
    out.push_str(")\n");
    out.push_str("(allow network-outbound)\n");
    out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
    out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
    out
}

/// Canonicalize so `/tmp` symlinks and `..` chains match the paths the
/// kernel sees. Missing paths pass through unchanged.
fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(profile: &str, script: &str) -> std::process::Output {
        Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("sandbox-exec runs")
    }

    #[test]
    fn profile_names_the_workspace_and_hooks_deny() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), None, &[]);
        assert!(p.contains("(deny default)"));
        assert!(p.contains(&format!(
            "(subpath \"{}\")",
            dir.path().canonicalize().unwrap().display()
        )));
        assert!(p.contains(".git/hooks"));
        assert!(p.contains("(allow network-outbound)"));
    }

    #[test]
    fn extra_writable_roots_are_granted() {
        let dir = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), None, &[extra.path().to_path_buf()]);
        assert!(p.contains(&format!(
            "(subpath \"{}\")",
            extra.path().canonicalize().unwrap().display()
        )));
    }

    #[test]
    fn every_path_entry_is_protected() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), None, &[]);
        // The default PATH always has /usr/bin; more importantly this
        // asserts the deny list is derived from PATH at all.
        let path = std::env::var("PATH").unwrap();
        for entry in path.split(':').filter(|e| !e.is_empty()).take(3) {
            let resolved = Path::new(entry)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(entry));
            assert!(
                p.contains(&format!("(subpath \"{}\")", resolved.display())),
                "PATH entry {entry} missing from the deny list"
            );
        }
    }

    #[test]
    fn workspace_writes_allowed_outside_writes_denied() {
        if !available() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let p = profile(ws.path(), None, &[]);
        let inside = run(&p, &format!("echo ok > {}/a.txt", ws.path().display()));
        assert!(inside.status.success(), "workspace write: {inside:?}");
        // A `$HOME` path outside the cache allowlist. The parent exists
        // (created unconfined) so a failure means the profile denied
        // the write, not that the directory was missing.
        let home = dirs::home_dir().expect("home dir");
        let probe = home.join(format!(".atom-seatbelt-probe-{}", std::process::id()));
        std::fs::create_dir_all(&probe).unwrap();
        let escape = run(&p, &format!("echo bad > {}/b.txt", probe.display()));
        let leaked = probe.join("b.txt").exists();
        let _ = std::fs::remove_dir_all(&probe);
        assert!(!escape.status.success(), "outside write must fail");
        assert!(!leaked, "no file may land outside the workspace");
    }

    #[test]
    fn git_hooks_writes_are_denied() {
        if !available() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".git/hooks")).unwrap();
        let p = profile(ws.path(), None, &[]);
        let hooks = run(
            &p,
            &format!("echo bad > {}/.git/hooks/pre-commit", ws.path().display()),
        );
        assert!(!hooks.status.success(), "hook write must fail");
    }

    #[test]
    fn path_bin_writes_are_denied() {
        if !available() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let p = profile(ws.path(), None, &[]);
        // Pick a writable PATH dir under $HOME so a failure is the
        // profile, not the filesystem. Skipped when $PATH has none.
        let Some(bin) = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|e| !e.is_empty())
            .map(PathBuf::from)
            .find(|e| e.starts_with(dirs::home_dir().unwrap_or_default()) && e.is_dir())
        else {
            return;
        };
        let probe = bin.join(format!(".atom-probe-{}", std::process::id()));
        let out = run(&p, &format!("echo bad > {}", probe.display()));
        let leaked = probe.exists();
        let _ = std::fs::remove_file(&probe);
        assert!(!out.status.success(), "PATH bin write must fail: {out:?}");
        assert!(!leaked, "no shim may be written into a PATH dir");
    }

    #[test]
    fn extra_writable_root_is_usable() {
        if !available() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let p = profile(ws.path(), None, &[extra.path().to_path_buf()]);
        let out = run(
            &p,
            &format!("echo ok > {}/granted.txt", extra.path().display()),
        );
        assert!(out.status.success(), "granted root write: {out:?}");
    }
}
