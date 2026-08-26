use std::process::Command;

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

    println!("cargo:rustc-env=ATOM_BUILD={commit} ({profile})");
}
