//! Layer 3 — kernel confinement.
//!
//! macOS: generates SBPL profiles for `/usr/bin/sandbox-exec -f`.
//! Linux: best-effort bubblewrap confinement; otherwise no confinement
//! and the audit log records the fallback.

use crate::policy::SandboxConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How a command was actually confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfineKind {
    /// Ran unsandboxed (Off mode or no mechanism available).
    None,
    /// macOS Seatbelt profile via /usr/bin/sandbox-exec.
    Seatbelt,
    /// Linux bubblewrap namespace jail.
    Bwrap,
}

impl ConfineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfineKind::None => "none",
            ConfineKind::Seatbelt => "seatbelt",
            ConfineKind::Bwrap => "bwrap",
        }
    }
}

/// Path of the macOS Seatbelt front-end.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// True when this machine can run commands under Seatbelt.
pub fn seatbelt_available() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    Path::new(SANDBOX_EXEC).exists()
}

/// SBPL string literal escaping (quotes and backslashes).
fn sbpl_str(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// macOS exposes /tmp and /var as symlinks into /private; Seatbelt
/// matches resolved vnodes, so emit both spellings for coverage.
fn subpath_filters(path: &Path, out: &mut BTreeSet<String>) {
    out.insert(format!("(subpath {})", sbpl_str(path)));
    let raw = path.to_string_lossy();
    if raw.starts_with("/tmp/") || raw.starts_with("/var/") {
        let priv_ = PathBuf::from(format!("/private{raw}"));
        out.insert(format!("(subpath {})", sbpl_str(&priv_)));
    }
}

/// Generate an SBPL profile for `sandbox-exec -f`.
///
/// Shape: deny-by-default; broad reads (minus ~/.ssh and ~/.gnupg);
/// writes restricted to the workspace subtree, the system temp dir,
/// /dev/null, atom's data dir and cfg.extra_writable; explicit hard
/// denies for `.git/hooks` and $HOME outside the writable set; network
/// denied unless `net_allowed` — and when allowed, Unix-socket egress
/// stays denied except the mDNSResponder socket, so DNS resolution
/// keeps working for approved network commands.
pub fn generate_profile(
    cfg: &SandboxConfig,
    workspace_root: &Path,
    tmpdir: &Path,
    data_dir_path: &Path,
    net_allowed: bool,
) -> String {
    let mut p = String::new();
    p.push_str("; atom sandbox profile (generated)\n");
    p.push_str("(version 1)\n");
    p.push_str("(deny default)\n");

    // Process basics so bash and children can run inside the sandbox.
    p.push_str("\n; process basics\n");
    p.push_str("(allow process-exec)\n");
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow signal (target same-sandbox))\n");
    p.push_str("(allow sysctl-read)\n");
    p.push_str("(allow mach-lookup)\n");

    // Reads are broad by design; sensitive material is denied explicitly.
    p.push_str("\n; reads\n");
    p.push_str("(allow file-read*)\n");
    if let Some(home) = dirs::home_dir() {
        p.push_str(&format!(
            "(deny file-read* (subpath {}))\n",
            sbpl_str(&home.join(".ssh"))
        ));
        p.push_str(&format!(
            "(deny file-read* (subpath {}))\n",
            sbpl_str(&home.join(".gnupg"))
        ));
        for extra in &cfg.extra_readonly {
            p.push_str(&format!(
                "(allow file-read* (subpath {}))\n",
                sbpl_str(extra)
            ));
        }
    }

    // Writes: strict allowlist.
    p.push_str("\n; write allowlist\n");
    let mut filters = BTreeSet::new();
    for root in [
        workspace_root.to_path_buf(),
        tmpdir.to_path_buf(),
        data_dir_path.to_path_buf(),
    ]
    .into_iter()
    .chain(cfg.extra_writable.iter().cloned())
    {
        subpath_filters(&root, &mut filters);
    }
    filters.insert("(literal \"/dev/null\")".to_string());
    p.push_str("(allow file-write*\n");
    for f in &filters {
        p.push_str(&format!("    {f}\n"));
    }
    p.push_str(")\n");

    // Hard denies that hold regardless of any allow above.
    p.push_str("\n; hard denies\n");
    p.push_str("(deny file-write* (regex #\"\\.git/hooks/\"))\n");
    if let Some(home) = dirs::home_dir() {
        let mut nots = vec![format!(
            "(require-not (subpath {}))",
            sbpl_str(workspace_root)
        )];
        for root in [tmpdir.to_path_buf(), data_dir_path.to_path_buf()]
            .into_iter()
            .chain(cfg.extra_writable.iter().cloned())
        {
            nots.push(format!("(require-not (subpath {}))", sbpl_str(&root)));
        }
        p.push_str(&format!(
            "(deny file-write* (require-all (subpath {}) {}))\n",
            sbpl_str(&home),
            nots.join(" ")
        ));
    }

    // Network stance. Unix-socket egress stays denied even when network
    // is allowed (keeps local daemons like Docker out of reach), but the
    // mDNSResponder socket is carved out: macOS resolves hostnames via a
    // connect(2) on it, so without this exception an approved curl/wget
    // could open TCP yet never resolve. Seatbelt gives the *last* matching
    // rule precedence, so these allows override the AF_UNIX deny above for
    // those exact paths only. Both /var and /private/var spellings are
    // emitted because /var is a symlink into /private and Seatbelt matches
    // on the resolved vnode.
    p.push_str("\n; network\n");
    if net_allowed {
        p.push_str("(allow network*)\n");
        p.push_str("(deny network-outbound (socket-domain AF_UNIX))\n");
        p.push_str("; DNS: mDNSResponder unix socket\n");
        p.push_str("(allow network-outbound (path \"/var/run/mDNSResponder\"))\n");
        p.push_str("(allow network-outbound (path \"/private/var/run/mDNSResponder\"))\n");
    } else {
        p.push_str("(deny network*)\n");
    }
    p
}

/// Locate an executable named `name` across PATH.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Best-effort Linux confinement: report whether bwrap is usable. The
/// caller proceeds with ConfineKind::None (audited) when it is absent.
pub fn linux_confine(_workspace_root: &Path, _tmpdir: &Path, _net_denied: bool) -> ConfineKind {
    if !cfg!(target_os = "linux") {
        return ConfineKind::None;
    }
    if find_in_path("bwrap").is_some() {
        ConfineKind::Bwrap
    } else {
        ConfineKind::None
    }
}

/// Build bwrap argv (caller appends `-- <program> <args...>`): read-only
/// bind of the host, writable workspace/tmp/data binds, optional net
/// unshare, and fresh proc/dev mounts.
pub fn bwrap_args(
    workspace_root: &Path,
    tmpdir: &Path,
    data_dir_path: &Path,
    net_denied: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev-bind".into(),
        "/dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--bind".into(),
        workspace_root.display().to_string(),
        workspace_root.display().to_string(),
        "--bind".into(),
        tmpdir.display().to_string(),
        tmpdir.display().to_string(),
        "--bind".into(),
        data_dir_path.display().to_string(),
        data_dir_path.display().to_string(),
    ];
    if net_denied {
        args.push("--unshare-net".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{NetPolicy, SandboxConfig, SandboxMode};

    fn gen(cfg: &SandboxConfig, ws: &Path, net_allowed: bool) -> String {
        generate_profile(
            cfg,
            ws,
            Path::new("/tmp"),
            Path::new("/data/atom"),
            net_allowed,
        )
    }

    #[test]
    fn golden_profile_contains_required_directives() {
        let cfg = SandboxConfig::default();
        let ws = Path::new("/Users/dev/proj");
        let p = gen(&cfg, ws, false);

        assert!(p.contains("(version 1)"));
        assert!(p.contains("(deny default)"));
        assert!(p.contains(r#"(deny file-write* (regex #"\.git/hooks/"))"#));

        // Workspace subtree writable...
        assert!(p.contains("(subpath \"/Users/dev/proj\")"));
        // ...as are temp, /dev/null and the atom data dir.
        assert!(p.contains("(subpath \"/tmp\")"));
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(p.contains("(subpath \"/data/atom\")"));

        // Home outside the writable set is explicitly denied.
        let home = dirs::home_dir().unwrap();
        assert!(p.contains(&format!("(require-all (subpath \"{}\")", home.display())));

        // Sensitive reads denied.
        assert!(p.contains(&format!("(subpath \"{}\")", home.join(".ssh").display())));
        assert!(p.contains(&format!("(subpath \"{}\")", home.join(".gnupg").display())));

        // Network denied by default.
        assert!(p.contains("(deny network*)"));
    }

    #[test]
    fn profile_reacts_to_config_changes() {
        let ws = Path::new("/Users/dev/proj");
        let mut cfg = SandboxConfig::default();

        let denied = gen(&cfg, ws, false);
        assert!(!denied.contains("(allow network*)"));

        cfg.network = NetPolicy::Allow;
        let allowed = gen(&cfg, ws, true);
        assert!(allowed.contains("(allow network*)"));
        assert!(allowed.contains("(socket-domain AF_UNIX)"));

        cfg.mode = SandboxMode::Strict;
        cfg.network = NetPolicy::Deny;
        cfg.extra_writable = vec![PathBuf::from("/opt/cache")];
        let strict = gen(&cfg, ws, false);
        assert!(strict.contains("(subpath \"/opt/cache\")"));
        assert_ne!(strict, denied);
    }

    #[test]
    fn dns_socket_carved_out_only_when_network_allowed() {
        let ws = Path::new("/Users/dev/proj");
        let cfg = SandboxConfig::default();

        // Network denied: no DNS carve-out, everything network is denied.
        let denied = gen(&cfg, ws, false);
        assert!(denied.contains("(deny network*)"));
        assert!(!denied.contains("mDNSResponder"));

        // Network allowed: egress opens, Unix-socket egress stays denied
        // except the mDNSResponder socket that macOS DNS resolution needs.
        let allowed = gen(&cfg, ws, true);
        assert!(allowed.contains("(allow network*)"));
        assert!(allowed.contains("(allow network-outbound (path \"/var/run/mDNSResponder\"))"));
        assert!(
            allowed.contains("(allow network-outbound (path \"/private/var/run/mDNSResponder\"))")
        );
        assert!(allowed.contains("(deny network-outbound (socket-domain AF_UNIX))"));
        // Seatbelt: last matching rule wins, so the carve-out must come
        // after the AF_UNIX deny to actually override it.
        let deny_pos = allowed
            .find("(deny network-outbound (socket-domain AF_UNIX))")
            .unwrap();
        let dns_pos = allowed
            .find("(allow network-outbound (path \"/var/run/mDNSResponder\"))")
            .unwrap();
        assert!(dns_pos > deny_pos);
    }

    #[test]
    fn macos_symlinked_tmp_gets_private_variant() {
        let cfg = SandboxConfig::default();
        let p = generate_profile(
            &cfg,
            Path::new("/ws"),
            Path::new("/tmp/atom-test"),
            Path::new("/data"),
            false,
        );
        assert!(p.contains("(subpath \"/private/tmp/atom-test\")"));
    }

    #[test]
    fn seatbelt_available_on_this_mac() {
        if cfg!(target_os = "macos") {
            assert!(seatbelt_available(), "/usr/bin/sandbox-exec missing?");
        } else {
            assert!(!seatbelt_available());
        }
    }

    #[test]
    fn bwrap_args_shape() {
        let args = bwrap_args(Path::new("/ws"), Path::new("/tmp/t"), Path::new("/d"), true);
        assert_eq!(args[0], "--ro-bind");
        assert!(args.contains(&"--unshare-net".to_string()));
        let open = bwrap_args(
            Path::new("/ws"),
            Path::new("/tmp/t"),
            Path::new("/d"),
            false,
        );
        assert!(!open.contains(&"--unshare-net".to_string()));
    }
}
