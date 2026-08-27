//! Build-flavor helpers: dev builds and release builds coexist on one
//! machine without touching each other.
//!
//! Dev builds (`cargo build`, `cargo run`, `make install-dev` — the
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_flavor() {
        assert_eq!(is_dev(), cfg!(debug_assertions));
        // `cargo test` runs the debug profile, so the dev names are the
        // ones under test; the release branch is compiled out here.
        if is_dev() {
            assert_eq!(client_name(), "atomdev");
            assert_eq!(server_name(), "atomsdev");
            assert_eq!(dir_leaf(), "atom-dev");
            assert_eq!(version_label(), format!("{} (dev)", env!("CARGO_PKG_VERSION")));
        } else {
            assert_eq!(client_name(), "atom");
            assert_eq!(server_name(), "atoms");
            assert_eq!(dir_leaf(), "atom");
            assert_eq!(version_label(), env!("CARGO_PKG_VERSION"));
        }
    }
}