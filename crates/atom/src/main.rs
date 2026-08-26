//! atom binary entry point: flag parsing and client wiring, ported
//! from main.go's main(). The server lives in atom-server, the UI in
//! atom-tui.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LastModel {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    thinking: String,
}

fn last_model_path() -> PathBuf {
    atom_core::session::store::data_dir().join("last-model.json")
}

/// loadLastModel returns the saved last-used model, or None if missing,
/// unreadable, or corrupt.
fn load_last_model() -> Option<LastModel> {
    let b = std::fs::read(last_model_path()).ok()?;
    let lm: LastModel = serde_json::from_slice(&b).ok()?;
    if lm.model.is_empty() {
        return None;
    }
    Some(lm)
}

/// saveLastModel records the model as the last used one. An empty model
/// is ignored; an empty thinking keeps the previously saved level.
fn save_last_model_state(provider_name: &str, model: &str, thinking: &str) {
    if model.is_empty() {
        return;
    }
    let mut lm = LastModel {
        provider: provider_name.to_string(),
        model: model.to_string(),
        thinking: thinking.to_string(),
    };
    if lm.thinking.is_empty() {
        if let Some(prev) = load_last_model() {
            lm.thinking = prev.thinking;
        }
    }
    if let Ok(b) = serde_json::to_vec_pretty(&lm) {
        let _ = std::fs::create_dir_all(atom_core::session::store::data_dir());
        let _ = std::fs::write(last_model_path(), b);
    }
}

const USAGE: &str = "usage: atom [-model id] [-key key] [-url base] [-session id]
                     [-stats [-stats-days N]] [--output-test] [--hot] [-no-deps]";

fn help_text() -> String {
    format!(
        "{USAGE}\n\nversion: {}\nbuild: {}",
        env!("CARGO_PKG_VERSION"),
        env!("ATOM_BUILD")
    )
}

struct Args {
    model: String,
    key: String,
    url: String,
    session: String,
    stats: bool,
    stats_days: i64,
    output_test: bool,
    hot: bool,
    hot_state: Option<String>,
    no_deps: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        model: String::new(),
        key: String::new(),
        url: String::new(),
        session: String::new(),
        stats: false,
        stats_days: 0,
        output_test: false,
        hot: false,
        hot_state: None,
        no_deps: false,
    };
    // -key defaults to $OLLAMA_API_KEY like the Go flag does.
    a.key = std::env::var("OLLAMA_API_KEY").unwrap_or_default();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let name = name.trim_start_matches('-').to_string();
        let mut next_val = || -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| anyhow!("flag needs an argument: -{name}"))
        };
        match name.as_str() {
            "model" => a.model = next_val()?,
            "key" => a.key = next_val()?,
            "url" => a.url = next_val()?,
            "session" => a.session = next_val()?,
            "stats" => a.stats = true,
            "stats-days" => a.stats_days = next_val()?.parse().context("bad -stats-days")?,
            "output-test" | "outputtest" => a.output_test = true,
            "hot" => a.hot = true,
            "hot-state" => a.hot_state = Some(next_val()?),
            "no-deps" => a.no_deps = true,
            "h" | "help" => {
                println!("{}", help_text());
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown flag: -{other}\n{USAGE}")),
        }
    }
    Ok(a)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = parse_args()?;

    // Ensure required tool dependencies (rg, uvx, merman-cli) exist
    // before the server spawns or the TUI takes over the terminal, while
    // it is still in a clean, non-raw state. Interactive on a TTY client;
    // headless (-serve) warns only, unless ATOM_DEPS_AUTOINSTALL=1.
    if !args.no_deps && !args.output_test && !args.stats {
        let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
        atom_core::deps::ensure_on_startup(interactive, &atom_core::deps::RealInstaller).await;
    }

    // Output-test mode: canned session demo, no server or key involved.
    if args.output_test {
        return atom_tui::run_output_test(args.hot, args.hot_state.as_deref().map(PathBuf::from))
            .await;
    }

    // Client mode: ensure a server is running, then connect.
    atom_server::client::ensure_server()
        .await
        .context("could not start server")?;
    // Hold a connection for the life of this process so the 5s idle
    // shutdown cannot fire while we sit in menus with no event stream.
    tokio::spawn(atom_server::client::hold_server_alive());

    // Stats mode: aggregated token usage report, then exit.
    if args.stats {
        let path = if args.stats_days > 0 {
            format!("/api/stats?days={}", args.stats_days)
        } else {
            "/api/stats?days=0".to_string()
        };
        let v = atom_server::client::get(&path)
            .await
            .context("could not fetch stats")?;
        let report: atom_core::session::stats::StatsReport =
            serde_json::from_value(v).context("could not parse stats")?;
        let color = is_terminal();
        for line in atom_core::session::stats::render_stats(&report, 0, color) {
            println!("{line}");
        }
        return Ok(());
    }

    // Resolve the model, provider, API key, and base URL.
    let explicit_endpoint = !args.key.is_empty() || !args.url.is_empty();
    let providers = if explicit_endpoint {
        Vec::new()
    } else {
        atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
        atom_core::providers::providers::build_providers().await
    };
    let mut sel_provider = atom_core::providers::providers::Provider {
        name: String::new(),
        id: String::new(),
        base_url: String::new(),
        key: String::new(),
        reasoning_field: String::new(),
    };
    let mut sel_model = String::new();

    // No flags at all: default to the last used model, else open the
    // model selector on startup (empty sel_model).
    if args.key.is_empty()
        && args.url.is_empty()
        && args.model.is_empty()
        && args.session.is_empty()
    {
        let mut defaulted = false;
        if let Some(lm) = load_last_model() {
            if let Some(p) =
                atom_core::providers::providers::provider_by_name(&providers, &lm.provider)
            {
                sel_provider = p;
                sel_model = lm.model;
                defaulted = true;
            }
        }
        if !defaulted {
            return launch_tui(providers, sel_provider, sel_model, args, None).await;
        }
    }

    if explicit_endpoint {
        // Explicit flags take priority: single ad-hoc provider.
        let mut base = args.url.clone();
        if base.is_empty() {
            base = if args.key.is_empty() {
                "http://localhost:11434/v1".into()
            } else {
                "https://ollama.com/v1".into()
            };
        }
        if args.key.is_empty() && base.contains("ollama.com") {
            return Err(anyhow!(
                "no API key. Get one at https://ollama.com/settings/keys, then export \
                 OLLAMA_API_KEY, pass -key, or save it to ~/.local/share/atom/providers/ollama-cloud."
            ));
        }
        let model = if args.model.is_empty() {
            "deepseek-v4-flash:cloud".to_string()
        } else {
            args.model.clone()
        };
        sel_provider = atom_core::providers::providers::Provider {
            reasoning_field: atom_core::providers::providers::reasoning_field_for_url(&base),
            name: atom_core::providers::providers::provider_name_for_url(&base),
            base_url: base.trim_end_matches('/').to_string(),
            key: args.key.clone(),
            ..Default::default()
        };
        sel_model = model;
    } else if !args.model.is_empty() {
        // -model without -key/-url: find the hosting provider, else local.
        sel_provider =
            match atom_core::providers::providers::find_provider_for_model(&providers, &args.model)
                .await
            {
                Some(p) => p,
                None => atom_core::providers::providers::Provider {
                    name: "ollama-local".into(),
                    base_url: "http://localhost:11434/v1".into(),
                    key: String::new(),
                    reasoning_field: "reasoning".into(),
                    ..Default::default()
                },
            };
        sel_model = args.model.clone();
    }

    // An explicitly chosen model becomes the default for future launches.
    if !sel_model.is_empty() {
        let thinking = load_last_model().map(|m| m.thinking).unwrap_or_default();
        save_last_model_state(&sel_provider.name, &sel_model, &thinking);
    }

    // Create or resume a session.
    let session = if !args.session.is_empty() {
        let v = atom_server::client::get(&format!("/api/sessions/{}", args.session))
            .await
            .context("could not resume session")?;
        let s: atom_core::session::store::Session =
            serde_json::from_value(v).context("could not parse session")?;
        s.info()
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut create = serde_json::json!({
            "provider": sel_provider.name,
            "model": sel_model,
            "cwd": cwd,
        });
        if let Some(lm) = load_last_model() {
            if !lm.thinking.is_empty() {
                create["thinking"] = serde_json::json!(lm.thinking);
            }
        }
        let v = atom_server::client::post("/api/sessions", &create)
            .await
            .context("could not create session")?;
        serde_json::from_value(v).context("could not parse session")?
    };

    launch_tui(providers, sel_provider, sel_model, args, Some(session)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_identifies_version_and_build() {
        let help = help_text();
        assert!(help.contains(env!("CARGO_PKG_VERSION")));
        assert!(help.contains(env!("ATOM_BUILD")));
    }
}

async fn launch_tui(
    providers: Vec<atom_core::providers::providers::Provider>,
    sel_provider: atom_core::providers::providers::Provider,
    sel_model: String,
    args: Args,
    session: Option<atom_core::session::store::SessionInfo>,
) -> Result<()> {
    let opts = atom_tui::app::RunOptions {
        providers,
        sel_provider,
        sel_model,
        session: session.unwrap_or_else(zero_session),
        hot_state_path: args.hot_state.map(PathBuf::from),
    };
    atom_tui::run(opts, args.hot).await
}

/// Zero-value SessionInfo like Go's SessionInfo{}.
fn zero_session() -> atom_core::session::store::SessionInfo {
    let epoch = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
    atom_core::session::store::SessionInfo {
        id: String::new(),
        title: String::new(),
        model: String::new(),
        provider: String::new(),
        message_count: 0,
        usage: None,
        parent_id: String::new(),
        thinking: String::new(),
        cancelled: false,
        status: atom_core::session::store::DelegateStatus::Done,
        batch_id: String::new(),
        batch_index: 0,
        created_at: epoch,
        updated_at: epoch,
    }
}

fn is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}
