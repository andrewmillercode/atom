//! `atoms` — the atom session-server binary.
//!
//! This is a thin entry point that runs the background HTTP server
//! (the same code that `atom --serve` used to invoke). Shipping it as a
//! separate binary keeps the background process named "atoms"
//! ("atomsdev" for dev installs) in `ps` / Activity Monitor: process
//! names follow the executable's own resolved path, and macOS resolves
//! symlinks and even hardlinks away — so a single binary plus symlink
//! names cannot produce two display names.
//!
//! The binary refuses to start unless the `_ATOM_LAUNCH` env var is set
//! to the expected token — this prevents users from running it directly.

use anyhow::Result;

/// The launch token that `atom` sets when spawning this binary.
/// Not a security boundary — just a guard against accidental invocation.
pub const LAUNCH_TOKEN: &str = "managed";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if std::env::var("_ATOM_LAUNCH").as_deref() != Ok(LAUNCH_TOKEN) {
        eprintln!("atoms: this process is managed by `atom` and cannot be run directly.");
        eprintln!("       use `atom` to start a session — the server launches automatically.");
        std::process::exit(1);
    }

    // Clear the token so child processes don't inherit it.
    std::env::remove_var("_ATOM_LAUNCH");

    if let Err(e) = run().await {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Deps check: headless mode, warn only.
    atom_core::deps::ensure_on_startup(false, &atom_core::deps::RealInstaller).await;

    atom_server::http::run_server().await
}
