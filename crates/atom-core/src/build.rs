//! Build-flavor helpers: dev builds and release builds coexist on one
//! machine without touching each other.
//!
//! Dev builds (`cargo build`, `cargo run`, `make dev` — the
//! debug profile) run as `atomdev` / `atomsdev` and keep their state in
//! `atom-dev` data/config directories. Release builds (`cargo build
//! --release`, `make install`, install.sh) run as `atom` / `atoms` with
//! the plain `atom` directories. Everything that names a binary or a
//! state directory goes through this module so the two flavors never
//! mix.

/// True when built with the debug profile.
pub const fn is_dev() -> bool {
    cfg!(debug_assertions)
}

/// Leaf name for atom's data and config directories:
/// `~/.local/share/atom-dev` + `~/.config/atom-dev` in dev builds,
/// `~/.local/share/atom` + `~/.config/atom` in release builds.
pub const fn dir_leaf() -> &'static str {
    if is_dev() {
        "atom-dev"
    } else {
        "atom"
    }
}

/// The client binary name: `atomdev` in dev builds, `atom` in release.
pub const fn client_name() -> &'static str {
    if is_dev() {
        "atomdev"
    } else {
        "atom"
    }
}

/// The server binary name: `atomsdev` in dev builds, `atoms` in release.
pub const fn server_name() -> &'static str {
    if is_dev() {
        "atomsdev"
    } else {
        "atoms"
    }
}

/// The version string shown by `-version` and `-help`, e.g. "0.1.1" or
/// "0.1.1 (dev)". The auto-updater matches the plain release string with
/// `contains`, so the dev marker must stay a suffix.
pub fn version_label() -> String {
    if is_dev() {
        format!("{} (dev)", env!("CARGO_PKG_VERSION"))
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

/// Human-facing build string for `-help`, e.g. "e4321875670b-dirty
/// (debug)" — set by atom-core's build script.
pub fn build_label() -> &'static str {
    env!("ATOM_BUILD")
}

/// Identity of this exact build: commit, profile, and the time the
/// build script last ran (cargo reruns the script only when the sources
/// change, so this tracks the code, not each cargo invocation). Clients
/// and servers exchange it over /api/capabilities: a running server
/// whose build_id differs from the client's was built from older code
/// and is recycled, even when CARGO_PKG_VERSION is unchanged.
pub fn build_id() -> &'static str {
    env!("ATOM_BUILD_ID")
}

/// Marker file `make dev` leaves beside the atomdev/atomsdev copies
/// (`.atomdev-source` in the install dir), containing the absolute path
/// of the cargo target dir that feeds the install. Dev builds read it
/// to detect and repair stale installs: `atomdev` warns when cargo has
/// built a newer artifact, and `find_server_binary` refreshes the
/// atomsdev copy before spawning it. Returns None when the marker is
/// missing or dangling (release installs, plain `cargo run`) — callers
/// treat that as "no source known" and skip the staleness logic.
pub fn dev_target_dir(exe_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let txt = std::fs::read_to_string(exe_dir.join(".atomdev-source")).ok()?;
    let dir = std::path::PathBuf::from(txt.trim());
    dir.is_dir().then_some(dir)
}

/// The cargo-built debug artifact (`atom` or `atoms`) recorded by the
/// `.atomdev-source` marker in `exe_dir`, when a dev install marker is
/// present and its target dir exists.
pub fn dev_debug_artifact(exe_dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    dev_target_dir(exe_dir).map(|t| t.join("debug").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_target_dir_reads_marker_only_when_dir_exists() {
        let dir = std::env::temp_dir().join(format!("atom-build-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // No marker: no source known.
        assert!(dev_target_dir(&dir).is_none());

        // Marker pointing at a real dir resolves to it (whitespace trimmed).
        std::fs::write(dir.join(".atomdev-source"), format!("{}\n", dir.display())).unwrap();
        assert_eq!(dev_target_dir(&dir).as_deref(), Some(dir.as_path()));
        assert_eq!(
            dev_debug_artifact(&dir, "atoms").as_deref(),
            Some(dir.join("debug").join("atoms").as_path())
        );

        // Dangling marker (target dir deleted): treated as absent.
        std::fs::write(dir.join(".atomdev-source"), "/nonexistent/target").unwrap();
        assert!(dev_target_dir(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_follow_the_flavor() {
        assert_eq!(is_dev(), cfg!(debug_assertions));
        // `cargo test` runs the debug profile, so the dev names are the
        // ones under test; the release branch is compiled out here.
        if is_dev() {
            assert_eq!(client_name(), "atomdev");
            assert_eq!(server_name(), "atomsdev");
            assert_eq!(dir_leaf(), "atom-dev");
            assert_eq!(
                version_label(),
                format!("{} (dev)", env!("CARGO_PKG_VERSION"))
            );
        } else {
            assert_eq!(client_name(), "atom");
            assert_eq!(server_name(), "atoms");
            assert_eq!(dir_leaf(), "atom");
            assert_eq!(version_label(), env!("CARGO_PKG_VERSION"));
        }
    }
}
