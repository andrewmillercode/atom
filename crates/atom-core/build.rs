use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let mut commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    if Command::new("git")
        .args(["diff", "--quiet"])
        .status()
        .is_ok_and(|status| !status.success())
    {
        commit.push_str("-dirty");
    }
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Cargo re-runs this script only when the package sources change, so
    // ATOM_BUILD_ID is stable across rebuilds of identical sources and
    // changes whenever the code does. atom, atoms, atomdev, and atomsdev
    // all link this one atom-core, so a single build shares one ID —
    // clients compare it with the server's via /api/capabilities to
    // recycle a server built from older code. Keep the ATOM_BUILD format
    // in sync with what the `-help` text advertises.
    println!("cargo:rustc-env=ATOM_BUILD={commit} ({profile})");
    println!("cargo:rustc-env=ATOM_BUILD_ID={commit}.{profile}.{stamp}");
}
