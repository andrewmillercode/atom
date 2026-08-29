//! Protected-path floor (sandbox-v2).
//!
//! "No tier may write these without a prompt" — the anti-self-escalation
//! floor from the design doc. An agent that can edit its own gate owns the
//! gate, so writes into atom's own state, shell startup files, `$PATH`
//! entries, `~/.ssh`, and any repo's `.git/hooks/` always escalate to the
//! approval prompt, even when the destination sits inside the workspace.
//! No config rule can lower this floor.
//!
//! Symlinks are checked: the destination is resolved through its nearest
//! existing ancestor, so a write whose destination or any path component
//! resolves into a protected tree is protected wherever the link lives.

use std::path::{Path, PathBuf};

/// Shell startup files protected by name, whatever directory holds them
/// (in practice `$HOME`).
const SHELL_STARTUP_FILES: &[&str] = &[
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".fishrc",
];

/// Returns true when writing to `path` hits the protected-path floor.
///
/// `home` is the user's home directory (pass `None` when unknown — the
/// home-relative checks are then skipped). `data_dir` / `config_dir` are
/// atom's own state directories; `None` skips those checks. The caller is
/// expected to pass an already-absolute path; relative paths are resolved
/// against the process cwd.
pub fn is_protected_write(
    abs: &Path,
    home: Option<&Path>,
    data_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> bool {
    let resolved = resolve_with_missing(abs);
    // Resolve bases too: on macOS /tmp symlinks to /private/tmp, so a
    // lexical comparison against the unresolved base never matches.
    let base = |p: &Path| resolve_with_missing(p);

    // 1. atom's own state: dataDir() (sandbox.json, approvals.json,
    //    audit log) and the config dir (config.json, settings).
    if let Some(d) = data_dir {
        if resolved.starts_with(base(d)) {
            return true;
        }
    }
    if let Some(c) = config_dir {
        if resolved.starts_with(base(c)) {
            return true;
        }
    }

    // 2. Shell startup files (by file name, plus the fish config dir).
    if let Some(h) = home {
        let hn = base(h);
        if let Some(name) = resolved.file_name().and_then(|n| n.to_str()) {
            if SHELL_STARTUP_FILES.contains(&name)
                && resolved
                    .parent()
                    .is_some_and(|p| normalize(p).starts_with(&hn))
            {
                return true;
            }
        }
        if let Some(cfg) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            if resolved.starts_with(base(&cfg.join("fish"))) {
                return true;
            }
        }
        if resolved.starts_with(base(&hn.join(".config/fish"))) {
            return true;
        }
        // 3. ~/.ssh (writes; reads stay per the open posture).
        if resolved.starts_with(base(&hn.join(".ssh"))) {
            return true;
        }
    }

    // 4. Anything on $PATH: a write over a trusted binary turns later
    //    runs into attacker code.
    if let Some(paths) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&paths) {
            if entry.as_os_str().is_empty() {
                continue;
            }
            if resolved.starts_with(base(&entry)) {
                return true;
            }
        }
    }

    // 5. .git/hooks/ in any repo — a planted hook turns every later
    //    `git commit` into host-level code execution.
    if has_git_hooks_component(&resolved) {
        return true;
    }

    false
}

/// True when the path's components contain a `.git/hooks` segment pair.
fn has_git_hooks_component(p: &Path) -> bool {
    let comps: Vec<_> = p.components().map(|c| c.as_os_str()).collect();
    for pair in comps.windows(2) {
        if pair[0] == ".git" && pair[1] == "hooks" {
            return true;
        }
    }
    false
}

/// Canonicalize the nearest existing ancestor, then restore any missing
/// suffix. This catches symlinks that point into a protected tree while
/// still supporting paths that don't exist yet.
fn resolve_with_missing(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }
    let mut out = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for component in suffix.into_iter().rev() {
        out.push(component);
    }
    out
}

/// Lexical normalization (collapse `.` / `..` and duplicate separators)
/// without touching the filesystem.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            _ => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("atom-protected-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atom_state_dirs_always_protected() {
        let data = tmpdir().join("data");
        let cfg = tmpdir().join("cfg");
        assert!(is_protected_write(
            &data.join("sandbox.json"),
            None,
            Some(&data),
            Some(&cfg)
        ));
        assert!(is_protected_write(
            &data.join("approvals.json"),
            None,
            Some(&data),
            Some(&cfg)
        ));
        // The audit log is inside dataDir() too.
        assert!(is_protected_write(
            &data.join("sandbox-audit.log"),
            None,
            Some(&data),
            Some(&cfg)
        ));
        assert!(is_protected_write(
            &cfg.join("config.json"),
            None,
            Some(&data),
            Some(&cfg)
        ));
    }

    #[test]
    fn shell_startup_files_protected_under_home() {
        let home = tmpdir().join("home");
        assert!(is_protected_write(
            &home.join(".zshrc"),
            Some(&home),
            None,
            None
        ));
        assert!(is_protected_write(
            &home.join(".bash_profile"),
            Some(&home),
            None,
            None
        ));
        assert!(is_protected_write(
            &home.join(".config/fish/config.fish"),
            Some(&home),
            None,
            None
        ));
        // Same file name outside home is not the user's startup file.
        let proj = tmpdir().join("proj");
        assert!(!is_protected_write(
            &proj.join(".zshrc"),
            Some(&home),
            None,
            None
        ));
        // Unrelated file under home is fine.
        assert!(!is_protected_write(
            &home.join("notes.txt"),
            Some(&home),
            None,
            None
        ));
    }

    #[test]
    fn path_entries_protected() {
        let home = tmpdir().join("home");
        // ~/.local/bin is on this machine's PATH in practice, but don't
        // depend on the environment: point PATH at a known scratch dir.
        let bindir = tmpdir().join("bin");
        unsafe {
            std::env::set_var("PATH", &bindir);
        }
        assert!(is_protected_write(
            &bindir.join("shim"),
            Some(&home),
            None,
            None
        ));
    }

    #[test]
    fn ssh_writes_protected() {
        let home = tmpdir().join("home");
        assert!(is_protected_write(
            &home.join(".ssh/config"),
            Some(&home),
            None,
            None
        ));
        assert!(is_protected_write(
            &home.join(".ssh/newkey"),
            Some(&home),
            None,
            None
        ));
        // reads are not floor-gated
        assert!(!is_protected_write(
            &home.join(".sshelsewhere/x"),
            Some(&home),
            None,
            None
        ));
    }

    #[test]
    fn git_hooks_protected_anywhere() {
        assert!(is_protected_write(
            Path::new("/ws/repo/.git/hooks/pre-commit"),
            None,
            None,
            None
        ));
        // deeper nesting still matches
        assert!(is_protected_write(
            Path::new("/ws/.git/hooks/x/y"),
            None,
            None,
            None
        ));
        // not a hooks write
        assert!(!is_protected_write(
            Path::new("/ws/repo/.git/hooksfile"),
            None,
            None,
            None
        ));
    }

    #[test]
    fn symlink_resolving_into_protected_tree_is_protected() {
        let home = tmpdir().join("home2");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let link = tmpdir().join("ssh-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(home.join(".ssh"), &link).unwrap();
        assert!(is_protected_write(
            &link.join("authorized_keys"),
            Some(&home),
            None,
            None
        ));
    }

    #[test]
    fn ordinary_workspace_write_not_protected() {
        let proj = tmpdir().join("proj");
        assert!(!is_protected_write(
            &proj.join("src/lib.rs"),
            Some(&proj),
            Some(&proj.join(".atom-data")),
            Some(&proj.join(".atom-config"))
        ));
    }
}
