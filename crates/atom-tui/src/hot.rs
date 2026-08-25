//! The `--hot` development loop watches the Rust workspace, rebuilds the
//! runnable client, and execs the exact artifact reported by Cargo.

use anyhow::{anyhow, Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::events::AppMsg;
use crate::overlays::{OverlayKind, PickerKind};

const HOT_DEBOUNCE: Duration = Duration::from_millis(90);
const HOT_HANDOFF_ENV: &str = "ATOM_HOT_HANDOFF";

#[derive(Debug, Clone)]
pub struct HotBuild {
    pub executable: PathBuf,
    pub elapsed: Duration,
}

/// Client state carried across a hot exec. Server-backed collections are
/// re-fetched by the replacement process rather than serialized here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HotState {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub scroll_y: usize,
    #[serde(default)]
    pub following: bool,
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub cursor_line: usize,
    #[serde(default)]
    pub cursor_col: usize,
    #[serde(default)]
    pub input_cursor: Option<usize>,
    #[serde(default)]
    pub input_selection: Option<(usize, usize)>,
    #[serde(default)]
    pub input_scroll_y: usize,
    #[serde(default)]
    pub show_reasoning: bool,
    #[serde(default)]
    pub thinking_pref: String,
    #[serde(default)]
    pub overlay: Option<OverlayKind>,
    #[serde(default)]
    pub overlay_q: String,
    #[serde(default)]
    pub overlay_q_sel: bool,
    #[serde(default)]
    pub overlay_sel: usize,
    #[serde(default)]
    pub overlay_scroll: usize,
    #[serde(default)]
    pub menu_visible: bool,
    #[serde(default)]
    pub menu_sel: usize,
    #[serde(default)]
    pub menu_virtual: bool,
    #[serde(default)]
    pub manage_visible: bool,
    #[serde(default)]
    pub manage_sel: usize,
    #[serde(default)]
    pub picker_kind: PickerKind,
    #[serde(default)]
    pub picker_sel: usize,
    #[serde(default)]
    pub context_visible: bool,
    #[serde(default)]
    pub context_sel: usize,
    #[serde(default)]
    pub reasoning_visible: bool,
    #[serde(default)]
    pub reasoning_sel: usize,
    #[serde(default)]
    pub reload_ms: Option<u64>,
}

pub fn default_hot_state_path() -> PathBuf {
    std::env::temp_dir().join("atom-hot-state.json")
}

/// Returns the Cargo workspace containing atom-tui.
pub fn hot_watch_dir() -> Option<PathBuf> {
    let manifest = option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from);
    if let Some(dir) = manifest {
        let root = dir.parent()?.parent()?.to_path_buf();
        if root.join("Cargo.toml").exists() {
            return Some(root);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join("Cargo.toml").exists() && d.join("crates").is_dir() {
            return Some(d);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

fn is_rebuild_event(root: &Path, event: &Event) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|path| is_rebuild_path(root, path))
}

fn is_rebuild_path(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.components().any(|part| match part {
        Component::Normal(name) => name == "target" || name == ".git",
        _ => false,
    }) {
        return false;
    }
    let name = relative.file_name().and_then(|name| name.to_str());
    matches!(name, Some("Cargo.toml" | "Cargo.lock" | "build.rs"))
        || relative.extension().and_then(|ext| ext.to_str()) == Some("rs")
}

fn is_theme_event(root: &Path, event: &Event) -> bool {
    event
        .paths
        .iter()
        .any(|path| path == &root.join("ui/theme.toml"))
        && matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        )
}

pub fn load_hot_theme() -> Result<()> {
    let root = hot_watch_dir().ok_or_else(|| anyhow!("hot reload: no cargo workspace found"))?;
    atom_core::render::colors::load_theme_file(&root.join("ui/theme.toml"))
        .map_err(|error| anyhow!("theme: {error}"))
}

/// Watches relevant source files. Saves that arrive while Cargo is running
/// are coalesced into one follow-up build before a successful artifact is
/// handed to the UI.
pub async fn watch_sources(tx: tokio::sync::mpsc::UnboundedSender<AppMsg>) {
    let Some(dir) = hot_watch_dir() else {
        let _ = tx.send(AppMsg::HotRebuilt(Err(
            "hot reload: no cargo workspace found; run from the rust/ workspace".into(),
        )));
        return;
    };

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = match notify::recommended_watcher(move |event: notify::Result<Event>| {
        let _ = event_tx.send(event);
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = tx.send(AppMsg::HotRebuilt(Err(format!(
                "hot reload: start watcher: {error}"
            ))));
            return;
        }
    };
    if let Err(error) = watcher.watch(&dir, RecursiveMode::Recursive) {
        let _ = tx.send(AppMsg::HotRebuilt(Err(format!(
            "hot reload: watch workspace: {error}"
        ))));
        return;
    }

    while let Some(event) = event_rx.recv().await {
        let event = match event {
            Ok(event) if is_rebuild_event(&dir, &event) || is_theme_event(&dir, &event) => event,
            Ok(_) => continue,
            Err(error) => {
                let _ = tx.send(AppMsg::HotRebuilt(Err(format!(
                    "hot reload: watcher: {error}"
                ))));
                continue;
            }
        };
        let detected = Instant::now();
        let mut source_changed = is_rebuild_event(&dir, &event);
        let mut theme_changed = is_theme_event(&dir, &event);

        // Trailing debounce: each relevant save restarts the short timer.
        let mut deadline = tokio::time::Instant::now() + HOT_DEBOUNCE;
        loop {
            match tokio::time::timeout_at(deadline, event_rx.recv()).await {
                Ok(Some(Ok(next)))
                    if is_rebuild_event(&dir, &next) || is_theme_event(&dir, &next) =>
                {
                    source_changed |= is_rebuild_event(&dir, &next);
                    theme_changed |= is_theme_event(&dir, &next);
                    deadline = tokio::time::Instant::now() + HOT_DEBOUNCE;
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => {
                    let _ = tx.send(AppMsg::HotRebuilt(Err(format!(
                        "hot reload: watcher: {error}"
                    ))));
                }
                Ok(None) => return,
                Err(_) => break,
            }
        }

        if theme_changed {
            send_theme_reload(&tx, &dir, detected);
        }
        if !source_changed {
            continue;
        }

        loop {
            let build_dir = dir.clone();
            let result = tokio::task::spawn_blocking(move || hot_rebuild(&build_dir))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));

            let mut changed_during_build = false;
            let mut theme_changed_during_build = false;
            while let Ok(next) = event_rx.try_recv() {
                match next {
                    Ok(event) => {
                        changed_during_build |= is_rebuild_event(&dir, &event);
                        theme_changed_during_build |= is_theme_event(&dir, &event);
                    }
                    Err(error) => {
                        let _ = tx.send(AppMsg::HotRebuilt(Err(format!(
                            "hot reload: watcher: {error}"
                        ))));
                    }
                }
            }
            if theme_changed_during_build {
                send_theme_reload(&tx, &dir, detected);
            }
            if changed_during_build {
                continue;
            }

            match result {
                Ok(mut build) => {
                    build.elapsed = detected.elapsed();
                    let _ = tx.send(AppMsg::HotRebuilt(Ok(build)));
                }
                Err(output) => {
                    let message = truncate_bytes(&output, 1200);
                    let _ = tx.send(AppMsg::HotRebuilt(Err(format!("build failed: {message}"))));
                }
            }
            break;
        }
    }
}

fn send_theme_reload(
    tx: &tokio::sync::mpsc::UnboundedSender<AppMsg>,
    dir: &Path,
    detected: Instant,
) {
    let result = atom_core::render::colors::load_theme_file(&dir.join("ui/theme.toml"))
        .map(|()| detected.elapsed())
        .map_err(|error| format!("theme: {error}"));
    let _ = tx.send(AppMsg::ThemeReloaded(result));
}

fn hot_rebuild(dir: &Path) -> std::result::Result<HotBuild, String> {
    let started = Instant::now();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "-p",
            "atom",
            "--bin",
            "atom",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(dir)
        .output()
        .map_err(|error| error.to_string())?;

    let (executable, diagnostics) = parse_cargo_output(&output.stdout, &output.stderr);
    if !output.status.success() {
        return Err(if diagnostics.trim().is_empty() {
            format!("cargo exited with {}", output.status)
        } else {
            diagnostics
        });
    }
    let executable = executable
        .ok_or_else(|| "cargo succeeded but did not report the atom executable".to_string())?;
    Ok(HotBuild {
        executable,
        elapsed: started.elapsed(),
    })
}

fn parse_cargo_output(stdout: &[u8], stderr: &[u8]) -> (Option<PathBuf>, String) {
    let mut executable = None;
    let mut diagnostics = String::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match message.get("reason").and_then(|value| value.as_str()) {
            Some("compiler-artifact")
                if message
                    .pointer("/target/name")
                    .and_then(|value| value.as_str())
                    == Some("atom") =>
            {
                if let Some(path) = message.get("executable").and_then(|value| value.as_str()) {
                    executable = Some(PathBuf::from(path));
                }
            }
            Some("compiler-message") => {
                if let Some(rendered) = message
                    .pointer("/message/rendered")
                    .and_then(|value| value.as_str())
                {
                    diagnostics.push_str(rendered);
                }
            }
            _ => {}
        }
    }
    diagnostics.push_str(&String::from_utf8_lossy(stderr));
    (executable, diagnostics.trim().to_string())
}

fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

pub fn write_state(path: &Path, state: &HotState) -> Result<()> {
    let bytes = serde_json::to_vec(state)?;
    std::fs::write(path, bytes).context("write hot state")?;
    Ok(())
}

pub fn load_state(path: &Path) -> Option<HotState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| {
        arg == &format!("-{name}")
            || arg == &format!("--{name}")
            || arg.starts_with(&format!("-{name}="))
            || arg.starts_with(&format!("--{name}="))
    })
}

pub fn inherited_terminal() -> bool {
    std::env::var_os(HOT_HANDOFF_ENV).is_some()
}

/// Re-executes the freshly built Cargo artifact. Raw mode and the alternate
/// screen intentionally remain active so the replacement can redraw without
/// flashing the shell's screen in between processes.
pub fn restart_self(path: &Path, session_id: &str, state_path: &Path) -> Result<()> {
    use std::ffi::CString;

    let mut args: Vec<String> = std::env::args().collect();
    args[0] = path.to_string_lossy().into_owned();
    if !session_id.is_empty() && !has_flag(&args, "session") {
        args.push("--session".into());
        args.push(session_id.to_string());
    }
    if !state_path.as_os_str().is_empty() && !has_flag(&args, "hot-state") {
        args.push("--hot-state".into());
        args.push(state_path.to_string_lossy().into_owned());
    }
    std::env::set_var(HOT_HANDOFF_ENV, "1");

    let program = CString::new(args[0].clone())?;
    let cargs: Vec<CString> = args
        .iter()
        .map(|arg| CString::new(arg.clone()))
        .collect::<Result<_, _>>()?;
    let mut pointers: Vec<*const libc::c_char> = cargs.iter().map(|arg| arg.as_ptr()).collect();
    pointers.push(std::ptr::null());
    unsafe {
        libc::execv(program.as_ptr(), pointers.as_ptr());
    }
    std::env::remove_var(HOT_HANDOFF_ENV);
    Err(anyhow!("execv failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_paths_exclude_generated_files() {
        let root = Path::new("/workspace");
        assert!(is_rebuild_path(
            root,
            Path::new("/workspace/crates/ui/src/view.rs")
        ));
        assert!(is_rebuild_path(root, Path::new("/workspace/Cargo.lock")));
        assert!(is_rebuild_path(
            root,
            Path::new("/workspace/crates/ui/build.rs")
        ));
        assert!(!is_rebuild_path(
            root,
            Path::new("/workspace/target/debug/build.rs")
        ));
        assert!(!is_rebuild_path(root, Path::new("/workspace/README.md")));
    }

    #[test]
    fn theme_changes_do_not_trigger_cargo() {
        let root = Path::new("/workspace");
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(root.join("ui/theme.toml"));
        assert!(is_theme_event(root, &event));
        assert!(!is_rebuild_event(root, &event));
    }

    #[test]
    fn cargo_output_selects_atom_executable_and_diagnostics() {
        let stdout =
            br#"{"reason":"compiler-artifact","target":{"name":"other"},"executable":"/tmp/other"}
{"reason":"compiler-message","message":{"rendered":"warning: useful\n"}}
{"reason":"compiler-artifact","target":{"name":"atom"},"executable":"/tmp/atom"}
"#;
        let (executable, diagnostics) = parse_cargo_output(stdout, b"Finished dev\n");
        assert_eq!(executable, Some(PathBuf::from("/tmp/atom")));
        assert!(diagnostics.contains("warning: useful"));
        assert!(diagnostics.contains("Finished dev"));
    }

    #[test]
    fn state_round_trip_preserves_transient_ui() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.json");
        let state = HotState {
            session_id: "session-1".into(),
            input: "draft".into(),
            input_cursor: Some(2),
            input_selection: Some((1, 4)),
            overlay: Some(OverlayKind::Session),
            overlay_q: "query".into(),
            overlay_sel: 3,
            menu_visible: true,
            picker_kind: PickerKind::Skills,
            ..HotState::default()
        };
        write_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.session_id, state.session_id);
        assert_eq!(loaded.input_cursor, Some(2));
        assert_eq!(loaded.input_selection, Some((1, 4)));
        assert_eq!(loaded.overlay, Some(OverlayKind::Session));
        assert_eq!(loaded.picker_kind, PickerKind::Skills);
    }
}
