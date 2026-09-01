//! overlays.rs holds the picker/overlay state logic: the slash-command
//! catalog and matching rules, session grouping ("Today"/"Yesterday"),
//! filtered counts, list-scroll math, and mouse hit-testing for both
//! full-screen overlays and the footer menus. Drawing lives in view.rs.

use atom_core::providers::providers::ModelEntry;
use atom_core::session::store::SessionInfo;
use std::collections::HashSet;

use crate::app::App;
use crate::prompt::wrap_plain;

pub use atom_core::providers::providers::filter_provider_entries;

// ---------------------------------------------------------------------------
// Overlay kinds.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OverlayKind {
    Model,
    Session,
    Stats,
    Providers,
    ProviderMethod,
    ProviderKey,
    Settings,
    WebSearch,
    WebFetch,
    Theme,
    /// Dev-only /profile overlay: startup time plus CPU/RSS/VSZ/etime
    /// snapshots for the client and the background `atoms` server.
    /// The slash command is registered only when [`atom_core::build::
    /// is_dev`] is true (handled at the call site in
    /// [`crate::overlays::COMMANDS`]).
    Profile,
    /// /fork: pick a user message in the current session to fork from.
    /// Rendered via the reusable fullscreen view template in
    /// [`crate::fullscreen_view`].
    Fork,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelPickerPurpose {
    #[default]
    Chat,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PickerKind {
    #[default]
    None,
    Mcp,
    Skills,
}

/// pickerItem is one row in the /mcps or /skills footer menu.
#[derive(Debug, Clone)]
pub struct PickerItem {
    pub title: String,
    pub meta: String,
    /// Set for MCP rows so Enter on an auth-required server can trigger
    /// the OAuth flow instead of just inserting the slash text.
    pub mcp_server: Option<String>,
}

/// command is a slash command the user can type in the chat.
#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
    /// "" built-in, "skill", or "mcp"
    pub kind: &'static str,
}

pub const COMMANDS: [Command; 17] = [
    Command {
        name: "/new",
        desc: "start a new session",
        kind: "",
    },
    Command {
        name: "/sessions",
        desc: "list all sessions",
        kind: "",
    },
    Command {
        name: "/fork",
        desc: "fork this session from a user message",
        kind: "",
    },
    Command {
        name: "/settings",
        desc: "configure Atom",
        kind: "",
    },
    Command {
        name: "/model",
        desc: "switch model",
        kind: "",
    },
    Command {
        name: "/providers",
        desc: "manage providers",
        kind: "",
    },
    Command {
        name: "/mcps",
        desc: "list MCP servers",
        kind: "",
    },
    Command {
        name: "/skills",
        desc: "list skills",
        kind: "",
    },
    Command {
        name: "/stats",
        desc: "show token usage stats",
        kind: "",
    },
    Command {
        name: "/context",
        desc: "show context breakdown",
        kind: "",
    },
    Command {
        name: "/subagents",
        desc: "show subagent status",
        kind: "",
    },
    Command {
        name: "/compact",
        desc: "summarize conversation context",
        kind: "",
    },
    Command {
        name: "/reasoning",
        desc: "select reasoning level",
        kind: "",
    },
    Command {
        name: "/theme",
        desc: "select color theme",
        kind: "",
    },
    Command {
        name: "/thinking",
        desc: "toggle expanded thinking blocks",
        kind: "",
    },
    Command {
        name: "/profile",
        desc: "show startup time and CPU/memory usage",
        kind: "",
    },
    Command {
        name: "/quit",
        desc: "exit",
        kind: "",
    },
];

impl Command {
    pub fn catalog_insert(&self) -> bool {
        self.kind == "skill" || self.kind == "mcp"
    }
}

fn command_from(name: String, desc: String, kind: &str) -> DynamicCommand {
    DynamicCommand {
        name,
        desc,
        kind: kind.to_string(),
        dynamic: true,
    }
}

/// A runtime-resolved command row (built-ins plus skill/MCP extras).
#[derive(Debug, Clone)]
pub struct DynamicCommand {
    pub name: String,
    pub desc: String,
    pub kind: String,
    pub dynamic: bool,
}

impl DynamicCommand {
    pub fn builtin(c: &Command) -> Self {
        DynamicCommand {
            name: c.name.to_string(),
            desc: c.desc.to_string(),
            kind: c.kind.to_string(),
            dynamic: false,
        }
    }

    pub fn catalog_insert(&self) -> bool {
        self.kind == "skill" || self.kind == "mcp"
    }
}

/// commandsAfterMinPrefix returns extras whose names start with typed,
/// once typed itself starts with minPrefix.
fn commands_after_min_prefix(
    typed: &str,
    min_prefix: &str,
    extras: &[DynamicCommand],
) -> Vec<DynamicCommand> {
    if min_prefix.is_empty() || !typed.starts_with(min_prefix) {
        return Vec::new();
    }
    if min_prefix == "/" && typed == "/" {
        return Vec::new();
    }
    extras
        .iter()
        .filter(|c| c.name.starts_with(typed))
        .cloned()
        .collect()
}

/// Discovers the cwd-specific skill and MCP slash commands once. The
/// selected MCP web-search backend remains reachable only through the
/// stable built-in `web_search` tool, so its duplicate server command is
/// omitted from the catalog.
pub fn discover_commands(cwd: &str) -> Vec<DynamicCommand> {
    let skills = atom_tools::skills::discover_skills(cwd);
    let mut out: Vec<DynamicCommand> = skills
        .iter()
        .map(|(name, sk)| command_from(format!("/{name}"), sk.description.clone(), "skill"))
        .collect();
    let selected = atom_core::config::load().resolved_web_search();
    let cfgs = atom_tools::mcp::load_mcp_configs(cwd);
    out.extend(
        cfgs.iter()
            .filter(|(name, _)| name.as_str() != selected.server)
            .map(|(name, cfg)| {
                let meta =
                    atom_tools::mcp_oauth::mcp_auth_display(cfg, name).unwrap_or_else(|| {
                        if !cfg.command.trim().is_empty() {
                            cfg.command.trim().to_string()
                        } else {
                            cfg.url.trim().to_string()
                        }
                    });
                command_from(format!("/{name}"), meta, "mcp")
            }),
    );
    out
}

/// Commands shown before the user types anything past "/". The rest of
/// the catalog appears once a character narrows the query.
const DEFAULT_COMMANDS: [&str; 7] = [
    "/new",
    "/sessions",
    "/model",
    "/providers",
    "/subagents",
    "/compact",
    "/quit",
];

/// Resolves the visible slash rows from an in-memory discovery snapshot:
/// built-ins first, then skills/MCP past "/", then /stats N at "/st".
/// An un-narrowed "/" shows only DEFAULT_COMMANDS; a single typed
/// character opens up the full catalog.
pub fn match_commands(typed: &str, dynamic_commands: &[DynamicCommand]) -> Vec<DynamicCommand> {
    // /profile is a dev-only diagnostic. Filtering at match time keeps
    // the catalog list out of release builds without two parallel
    // COMMANDS arrays or a second set of slash handlers.
    let visible_commands: &'static [Command] = if atom_core::build::is_dev() {
        &COMMANDS
    } else {
        // Compile-time filter isn't ergonomic in stable Rust, so walk
        // the array once at startup and cache the release-only view.
        // The COMMANDS array only has one entry to skip.
        static RELEASE_COMMANDS: std::sync::OnceLock<Vec<Command>> = std::sync::OnceLock::new();
        RELEASE_COMMANDS.get_or_init(|| {
            COMMANDS
                .iter()
                .filter(|&c| c.name != "/profile")
                .cloned()
                .collect()
        })
    };
    let builtins = if typed == "/" {
        visible_commands
            .iter()
            .filter(|c| DEFAULT_COMMANDS.contains(&c.name))
            .map(DynamicCommand::builtin)
            .collect::<Vec<_>>()
    } else {
        visible_commands
            .iter()
            .filter(|c| c.name.starts_with(typed))
            .map(DynamicCommand::builtin)
            .collect::<Vec<_>>()
    };
    let mut out = builtins;
    out.extend(commands_after_min_prefix(typed, "/", dynamic_commands));
    let stats_30 = [DynamicCommand {
        name: "/stats 30".to_string(),
        desc: "show token usage for the last 30 days".to_string(),
        kind: String::new(),
        dynamic: false,
    }];
    out.extend(commands_after_min_prefix(typed, "/st", &stats_30));
    out
}

/// parseStatsDays reads the optional window from a /stats command.
pub fn parse_stats_days(text: &str) -> i64 {
    let Some(rest) = text.strip_prefix("/stats") else {
        return 0;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return 0;
    }
    // get(..4) instead of [..4]: typed text may contain multi-byte runes,
    // and a fixed byte cut can land mid-rune and panic on slice.
    let rest = if rest
        .get(..4)
        .is_some_and(|s| s.eq_ignore_ascii_case("days"))
    {
        rest[4..].trim()
    } else {
        rest
    };
    rest.parse::<i64>().ok().filter(|n| *n > 0).unwrap_or(0)
}

pub fn is_slash_query(s: &str) -> bool {
    let s = s.trim();
    s.starts_with('/') && !s.contains([' ', '\t', '\n'])
}

/// Returns true when the submitted text looks like an absolute file path
/// (e.g. `/Users/me/project/Cargo.toml`) rather than a slash command.
/// Heuristic: starts with `/`, has at least two path segments, and the
/// leaf contains a `.` (extension) or the path resolves to an existing
/// file.
pub fn looks_like_file_path(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    // Must have more than one segment (not just "/foo")
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return false;
    }
    // Leaf has an extension (very common for real paths)
    if let Some(leaf) = segments.last() {
        if leaf.contains('.') {
            return true;
        }
    }
    // Fallback: check if the path exists on disk
    std::path::Path::new(trimmed).exists()
}

/// isCatalogPrompt reports whether a leading "/name" names a discovered
/// skill or MCP server (sent as a prompt instead of run locally).
pub fn is_catalog_prompt(text: &str, dynamic_commands: &[DynamicCommand]) -> bool {
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.is_empty() || !is_picker_use_text(fields[0]) {
        return false;
    }
    dynamic_commands
        .iter()
        .any(|command| command.catalog_insert() && command.name == fields[0])
}

fn is_picker_use_text(field: &str) -> bool {
    is_slash_query(field)
}

// ---------------------------------------------------------------------------
// Filtering and counts.
// ---------------------------------------------------------------------------

pub fn filter_entries(entries: &[ModelEntry], query: &str) -> Vec<ModelEntry> {
    atom_core::providers::providers::filter_entries(entries, query)
}

/// filterSessions matches case-insensitively across id/model/title;
/// every space-separated word must appear somewhere.
pub fn filter_sessions<'a>(sessions: &'a [SessionInfo], query: &str) -> Vec<&'a SessionInfo> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return sessions.iter().collect();
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    sessions
        .iter()
        .filter(|s| {
            words.iter().all(|w| {
                s.id.contains(w)
                    || s.model.to_lowercase().contains(w)
                    || s.title.to_lowercase().contains(w)
            })
        })
        .collect()
}

pub fn overlay_has_query(kind: Option<OverlayKind>) -> bool {
    matches!(
        kind,
        Some(OverlayKind::Model)
            | Some(OverlayKind::Session)
            | Some(OverlayKind::Providers)
            | Some(OverlayKind::ProviderKey)
            | Some(OverlayKind::Theme)
            | Some(OverlayKind::Fork)
    )
}

/// overlayCount returns the number of filtered items in the overlay.
pub fn overlay_count(app: &App) -> usize {
    match app.overlay {
        Some(OverlayKind::Model) => model_rows(app).len(),
        Some(OverlayKind::Session) => session_rows(app).len(),
        Some(OverlayKind::Providers) => atom_core::providers::providers::filter_provider_entries(
            &app.overlay_providers,
            &app.overlay_q,
        )
        .len(),
        Some(OverlayKind::ProviderMethod) => 2,
        Some(OverlayKind::Settings) => 5,
        Some(OverlayKind::WebSearch) => web_search_rows(app).len(),
        Some(OverlayKind::WebFetch) => web_fetch_rows(app).len(),
        Some(OverlayKind::Theme) => theme_rows()
            .iter()
            .filter(|e| filter_theme_match(e, &app.overlay_q))
            .count(),
        Some(OverlayKind::Fork) => fork_rows(app).len(),
        Some(OverlayKind::Profile) => profile_overlay_rows(app),
        _ => 0,
    }
}

pub fn settings_labels(app: &App) -> Vec<String> {
    let compaction = app.atom_config.resolved_compaction();
    let web = app.atom_config.resolved_web_search();
    let fetch = app.atom_config.resolved_web_fetch();
    vec![
        format!(
            "Compaction model  {} / {}",
            compaction.provider, compaction.model
        ),
        format!(
            "Auto-compaction  {}",
            if compaction.resolved_enabled() {
                "on"
            } else {
                "off"
            }
        ),
        format!("Web search provider  {}", web.server),
        format!("Web fetch provider  {}", fetch.server),
        if app.settings_onboarding {
            "Continue with defaults / finish setup".into()
        } else {
            "Done".into()
        },
    ]
}

/// themeRows lists selectable themes with their id and source label.
pub fn theme_rows() -> Vec<atom_core::render::colors::ThemeEntry> {
    atom_core::render::colors::available_themes()
}

/// filterThemeMatch reports whether the theme row matches the overlay
/// search query. An empty query matches every row. Matches are
/// case-insensitive substring checks against both the display name and
/// the stable theme id, so typing "dark" finds "Solarized Dark" and
/// typing "solar" finds it by id.
pub fn filter_theme_match(entry: &atom_core::render::colors::ThemeEntry, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    entry.name.to_lowercase().contains(&q) || entry.id.to_lowercase().contains(&q)
}

pub fn web_search_rows(app: &App) -> Vec<(String, String, String)> {
    let mut rows: Vec<(String, String, String)> = atom_core::config::bundled_web_search_profiles()
        .into_iter()
        .map(|profile| {
            let auth = match profile.auth {
                atom_core::config::WebSearchAuth::Optional => "anonymous · optional key",
                atom_core::config::WebSearchAuth::Required => "API key required",
            };
            (profile.id, profile.name, auth.into())
        })
        .collect();
    for (name, cfg) in atom_tools::mcp::load_mcp_configs(&app.cwd) {
        if rows.iter().any(|row| row.0 == name) {
            continue;
        }
        let meta = if cfg.url.is_empty() {
            "custom MCP".to_string()
        } else {
            cfg.url
        };
        rows.push((name.clone(), name, meta));
    }
    rows
}

pub fn web_fetch_rows(_app: &App) -> Vec<(String, String, String)> {
    let rows: Vec<(String, String, String)> = atom_core::config::bundled_web_fetch_profiles()
        .into_iter()
        .map(|profile| {
            let auth = match profile.auth {
                atom_core::config::WebSearchAuth::Optional => "anonymous · optional key",
                atom_core::config::WebSearchAuth::Required => "API key required",
            };
            (profile.id, profile.name, auth.into())
        })
        .collect();
    rows
}

// ---------------------------------------------------------------------------
// Scroll math (direct ports).
// ---------------------------------------------------------------------------

/// overlayKeepVisible returns a new start index so row `sel` is fully
/// visible within maxLines visual lines (sticky scrolling).
pub fn overlay_keep_visible(
    scroll: usize,
    sel: usize,
    max_lines: usize,
    n: usize,
    line_count: impl Fn(usize) -> usize,
) -> usize {
    if n == 0 {
        return 0;
    }
    let sel = sel.min(n - 1);
    let max_lines = max_lines.max(1);
    let lc = |i: usize| line_count(i).max(1);
    let span = |from: usize, to: usize| -> usize { (from..=to).map(lc).sum() };

    let mut scroll = scroll.min(n - 1);
    if sel < scroll {
        scroll = sel;
    } else if span(scroll, sel) > max_lines {
        // Pin selection at the bottom: largest start <= sel that fits.
        scroll = sel;
        let mut used = lc(sel);
        let mut i = sel;
        while i > 0 {
            i -= 1;
            let h = lc(i);
            if used + h > max_lines {
                break;
            }
            used += h;
            scroll = i;
        }
    }

    // Don't leave empty space at the bottom when content is short.
    while scroll > 0 && span(scroll, n - 1) < max_lines {
        if span(scroll - 1, sel) > max_lines {
            break;
        }
        scroll -= 1;
    }
    scroll.min(sel)
}

pub fn visual_line_count(s: &str) -> usize {
    if s.is_empty() {
        return 1;
    }
    s.split('\n').count()
}

/// Wrapped line count for a plain-text label at width.
pub fn wrapped_label_line_count(label: &str, width: usize, selected: bool, marker: &str) -> usize {
    let text = format!("{marker}{label}");
    if selected {
        wrap_plain(&format!("▸ {text}"), width.max(1)).len().max(1)
    } else {
        wrap_plain(&text, width.max(1)).len().max(1)
    }
}

// ---------------------------------------------------------------------------
// Model picker rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ModelRow {
    pub label: String,
    pub entry: Option<ModelEntry>,
}

fn model_key(entry: &ModelEntry) -> (String, String) {
    (entry.provider.name.clone(), entry.model.clone())
}

pub fn model_rows(app: &App) -> Vec<ModelRow> {
    let filtered = filter_entries(&app.overlay_entries, &app.overlay_q);
    let mut rows = Vec::new();
    let mut shown = HashSet::new();

    let mut pinned = Vec::new();
    for wanted in &app.picker_settings.favorites {
        if let Some(entry) = filtered
            .iter()
            .find(|entry| entry.provider.name == wanted.provider && entry.model == wanted.model)
        {
            shown.insert(model_key(entry));
            pinned.push(entry.clone());
        }
    }
    if !pinned.is_empty() {
        rows.push(ModelRow {
            label: "Pinned".to_string(),
            entry: None,
        });
        rows.extend(pinned.into_iter().map(|entry| ModelRow {
            label: String::new(),
            entry: Some(entry),
        }));
    }

    let mut recent = Vec::new();
    for wanted in &app.picker_settings.recents {
        if let Some(entry) = filtered.iter().find(|entry| {
            entry.provider.name == wanted.provider
                && entry.model == wanted.model
                && !shown.contains(&model_key(entry))
        }) {
            shown.insert(model_key(entry));
            recent.push(entry.clone());
        }
    }
    if !recent.is_empty() {
        rows.push(ModelRow {
            label: "Recent".to_string(),
            entry: None,
        });
        rows.extend(recent.into_iter().map(|entry| ModelRow {
            label: String::new(),
            entry: Some(entry),
        }));
    }

    let remaining: Vec<ModelEntry> = filtered
        .into_iter()
        .filter(|entry| !shown.contains(&model_key(entry)))
        .collect();
    if !remaining.is_empty() {
        rows.push(ModelRow {
            label: "Models".to_string(),
            entry: None,
        });
        rows.extend(remaining.into_iter().map(|entry| ModelRow {
            label: String::new(),
            entry: Some(entry),
        }));
    }
    rows
}

pub fn first_model_row(app: &App) -> usize {
    model_rows(app)
        .iter()
        .position(|row| row.entry.is_some())
        .unwrap_or_else(|| model_rows(app).len())
}

pub fn selected_model(app: &App) -> Option<ModelEntry> {
    model_rows(app)
        .get(app.overlay_sel)
        .and_then(|row| row.entry.clone())
}

pub fn move_model_sel(app: &mut App, dir: i32) {
    let rows = model_rows(app);
    let mut i = app.overlay_sel as i32 + dir;
    while i >= 0 && i < rows.len() as i32 {
        if rows[i as usize].entry.is_some() {
            app.overlay_sel = i as usize;
            sync_model_scroll(app);
            return;
        }
        i += dir;
    }
}

pub fn sync_model_scroll(app: &mut App) {
    sync_overlay_scroll(app);
}

// ---------------------------------------------------------------------------
// Fork picker rows.
// ---------------------------------------------------------------------------

/// One row in the /fork overlay. The SessionLatest variant is the
/// "fork from latest" sentinel (no position); UserMessage rows carry the
/// position of the source message so the server can truncate up to (and
/// excluding) that point.
#[derive(Debug, Clone)]
pub struct ForkRow {
    pub kind: ForkRowKind,
    pub label: String,
    /// When set, formatted HH:MM in the user's local time. Rendered as
    /// the row's trailing tag.
    pub timestamp: String,
    /// Position in the source session's messages array for UserMessage
    /// rows; None for the SessionLatest sentinel and Header rows.
    pub position: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkRowKind {
    Header,
    SessionLatest,
    UserMessage,
}

/// Returns the rendered fork rows: one Header, the SessionLatest
/// sentinel, a User-messages Header, and one row per user message
/// filtered by case-insensitive substring match against the label.
/// The SessionLatest row is always present (pin it at the top) so the
/// user can always fork from the full transcript.
pub fn fork_rows(app: &App) -> Vec<ForkRow> {
    let mut rows: Vec<ForkRow> = Vec::new();
    rows.push(ForkRow {
        kind: ForkRowKind::Header,
        label: "Entire Session".into(),
        timestamp: String::new(),
        position: None,
    });
    rows.push(ForkRow {
        kind: ForkRowKind::SessionLatest,
        label: fork_session_latest_label(app),
        timestamp: fork_session_latest_trailing(app),
        position: None,
    });
    let user_count = app.overlay_fork_user_messages.len();
    if user_count > 0 {
        rows.push(ForkRow {
            kind: ForkRowKind::Header,
            label: "From Message".into(),
            timestamp: String::new(),
            position: None,
        });
        let q = app.overlay_q.to_lowercase();
        for msg in &app.overlay_fork_user_messages {
            if !q.is_empty() && !msg.preview.to_lowercase().contains(&q) {
                continue;
            }
            rows.push(ForkRow {
                kind: ForkRowKind::UserMessage,
                label: msg.preview.clone(),
                timestamp: msg.timestamp.clone(),
                position: Some(msg.position),
            });
        }
    }
    rows
}

/// firstForkRow: the index of the SessionLatest row, which the overlay
/// opens on. 0 when the source session has no user messages.
pub fn first_fork_row() -> usize {
    1
}

/// moveForkSel skips Header rows so ↑/↓ always lands on a pickable row.
pub fn move_fork_sel(rows: &[ForkRow], sel: usize, dir: i32) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let mut i = sel as i32 + dir;
    while i >= 0 && i < rows.len() as i32 {
        if rows[i as usize].kind != ForkRowKind::Header {
            return i as usize;
        }
        i += dir;
    }
    sel.min(rows.len().saturating_sub(1))
}

fn fork_session_latest_label(app: &App) -> String {
    let title = if app.session.title.is_empty() {
        "this session".to_string()
    } else {
        app.session.title.clone()
    };
    format!("Fork from latest — {title}")
}

fn fork_session_latest_trailing(app: &App) -> String {
    let model = if app.session.model.is_empty() {
        app.sel_model.clone()
    } else {
        app.session.model.clone()
    };
    let count = app.session.message_count;
    let msg = if count == 1 { "msg" } else { "msgs" };
    if model.is_empty() {
        format!("{count} {msg}")
    } else {
        format!("{model} · {count} {msg}")
    }
}

/// Muted right-aligned tag marking subagent sessions in the picker.
pub const SUBAGENT_TAG: &str = "Subagent";
/// Columns the tag occupies: the tag plus the gap before it.
pub const SUBAGENT_TAG_WIDTH: usize = SUBAGENT_TAG.len() + 2;

pub fn day_label(t: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Duration, Local, TimeZone};
    let local = Local.from_utc_datetime(&t.naive_utc());
    let today = Local::now();
    let d_today = today.date_naive();
    let d_t = local.date_naive();
    if d_t == d_today {
        return "Today".to_string();
    }
    if d_t == d_today - Duration::days(1) {
        return "Yesterday".to_string();
    }
    local.format("%A, %b %-d").to_string()
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub date: bool,
    pub label: String,
    pub sess: Option<SessionInfo>,
}

/// sessionRows flattens filtered sessions into picker rows with date
/// group headers between them (sessions arrive sorted by UpdatedAt).
pub fn session_rows(app: &App) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    let filtered = filter_sessions(&app.overlay_sessions, &app.overlay_q);
    let mut pinned_ids = HashSet::new();
    let mut pinned = Vec::new();
    for id in &app.picker_settings.pinned_sessions {
        if let Some(session) = filtered.iter().find(|session| session.id == *id) {
            pinned_ids.insert(session.id.clone());
            pinned.push((*session).clone());
        }
    }
    rows.push(SessionRow {
        date: true,
        label: if pinned.is_empty() {
            "Pinned - None".to_string()
        } else {
            "Pinned".to_string()
        },
        sess: None,
    });
    rows.extend(pinned.into_iter().map(|session| SessionRow {
        date: false,
        label: session.title.clone(),
        sess: Some(session),
    }));

    let mut last = String::new();
    for s in filtered {
        if pinned_ids.contains(&s.id) {
            continue;
        }
        let h = day_label(s.updated_at);
        if h != last {
            last = h.clone();
            rows.push(SessionRow {
                date: true,
                label: h,
                sess: None,
            });
        }
        rows.push(SessionRow {
            date: false,
            label: s.title.clone(),
            sess: Some((*s).clone()),
        });
    }
    rows
}

pub fn first_session_row(app: &App) -> usize {
    session_rows(app)
        .iter()
        .position(|r| !r.date)
        .unwrap_or_else(|| session_rows(app).len())
}

/// moveSessionSel skips date header rows.
pub fn move_session_sel(app: &mut App, dir: i32) {
    let rows = session_rows(app);
    let mut i = app.overlay_sel as i32 + dir;
    while i >= 0 && i < rows.len() as i32 {
        if !rows[i as usize].date {
            app.overlay_sel = i as usize;
            sync_session_scroll(app);
            return;
        }
        i += dir;
    }
}

/// Per-row visual line counts for the session picker; shared by the
/// scroll math and the renderer so wrapping stays in sync.
pub fn sync_session_scroll(app: &mut App) {
    sync_overlay_scroll(app);
}

pub fn stats_scroll_max(app: &App) -> usize {
    let Some(report) = &app.overlay_stats else {
        return 0;
    };
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let height = crate::fullscreen_view::content_height(app.height.max(1) as usize);
    let data = overlay_view_data(app, OverlayKind::Stats);
    let spec = overlay_spec(app, OverlayKind::Stats, &data);
    let lines = atom_core::session::stats::render_stats(report, width as i32, true);
    let visible = crate::fullscreen_view::list_visible_rows(&spec, width, height);
    lines.len().saturating_sub(visible)
}

/// Number of raw render lines produced by the profile overlay — used
/// to clamp Up/Down so the cursor can't scroll past the end. Mirrors
/// `stats_scroll_max`.
pub fn profile_scroll_max(app: &App) -> usize {
    let Some(report) = &app.overlay_profile else {
        return 0;
    };
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let height = crate::fullscreen_view::content_height(app.height.max(1) as usize);
    let data = overlay_view_data(app, OverlayKind::Profile);
    let spec = overlay_spec(app, OverlayKind::Profile, &data);
    let lines = crate::profile::render_profile(report, std::time::SystemTime::now());
    let visible = crate::fullscreen_view::list_visible_rows(&spec, width, height);
    lines.len().saturating_sub(visible)
}

/// profileOverlayRows is the count of profile raw lines — exposed as
/// a helper for callers that want to read it directly (the App's key
/// handler, scroll math).
fn profile_overlay_rows(app: &App) -> usize {
    let Some(report) = &app.overlay_profile else {
        return 0;
    };
    crate::profile::render_profile(report, std::time::SystemTime::now()).len()
}

// ---------------------------------------------------------------------------
// Footer menu geometry (slash/manage/picker/context).
// ---------------------------------------------------------------------------

fn footer_menu_rows(app: &App) -> usize {
    if app.menu_visible {
        let typed = app.menu_typed();
        let typed = typed.as_str();
        return match_commands(typed, &app.slash_commands).len();
    }
    if app.manage_visible {
        return app.manage_agents.len() + 1;
    }
    if !matches!(app.picker_kind, PickerKind::None) {
        return app.picker_items.len() + 1;
    }
    if app.context_visible {
        return app.context_rows.len() + 1;
    }
    if app.reasoning_visible {
        return app.thinking_levels.len() + 1;
    }
    if app.at_menu_visible {
        return app.at_menu_items.len();
    }
    0
}

/// Rows reserved above the prompt for the active menu, including its divider.
/// Keep one conversation row visible even on very short terminals.
pub fn footer_menu_height(app: &App) -> usize {
    let rows = footer_menu_rows(app);
    if rows == 0 {
        return 0;
    }
    rows.saturating_add(1)
        .min(app.base_viewport_height().saturating_sub(1))
}

/// footerMenuSel is the selected row and how many title rows pin when clipped.
pub fn footer_menu_sel(app: &App) -> (usize, usize) {
    if app.menu_visible {
        return (app.menu_sel, 0);
    }
    if app.manage_visible {
        return (app.manage_sel + 1, 1);
    }
    if !matches!(app.picker_kind, PickerKind::None) {
        return (app.picker_sel + 1, 1);
    }
    if app.context_visible {
        return (app.context_sel + 1, 1);
    }
    if app.reasoning_visible {
        return (app.reasoning_sel + 1, 1);
    }
    if app.at_menu_visible {
        return (app.at_menu_sel, 0);
    }
    (0, 0)
}

/// footerMenuWindow fits an n-row menu into maxH viewport rows keeping
/// sel visible; pinned titles stay the first visible row.
pub fn footer_menu_window(
    n: usize,
    max_h: usize,
    sel: usize,
    title_rows: usize,
) -> (usize, usize, bool, usize) {
    if n == 0 || max_h == 0 {
        return (0, 0, false, 0);
    }
    if n <= max_h {
        return (0, n, title_rows > 0, title_rows);
    }
    if title_rows > 0 {
        let vis_items = max_h.saturating_sub(1);
        let item_count = n - title_rows;
        let item_sel = sel
            .saturating_sub(title_rows)
            .min(item_count.saturating_sub(1));
        let mut start_item = item_sel.saturating_sub(vis_items / 2);
        if vis_items > 0 && start_item + vis_items > item_count {
            start_item = item_count.saturating_sub(vis_items);
        }
        return (0, 1 + vis_items, true, title_rows + start_item);
    }
    let mut start = sel.saturating_sub(max_h / 2);
    if start + max_h > n {
        start = n - max_h;
    }
    (start, max_h, false, 0)
}

pub fn map_footer_menu_row(
    overlay_row: usize,
    start: usize,
    pin_title: bool,
    item_start: usize,
) -> usize {
    if pin_title {
        if overlay_row == 0 {
            return 0;
        }
        return item_start + overlay_row - 1;
    }
    start + overlay_row
}

/// Maps a screen y to the menu's underlying row index, if inside.
pub fn footer_menu_row_at_y(app: &App, y: usize, n: usize) -> Option<usize> {
    let (sel, title_rows) = footer_menu_sel(app);
    let region_h = footer_menu_height(app);
    let menu_h = region_h.saturating_sub(1);
    let (start, vis, pin_title, item_start) = footer_menu_window(n, menu_h, sel, title_rows);
    if vis < 1 {
        return None;
    }
    // y is relative to the viewport's top. The menu overlays the last
    // region_h rows of the viewport: divider on top, items below, so the
    // visible items end at the viewport's bottom edge.
    let top = app.viewport_height().saturating_sub(vis);
    if y < top || y >= top + vis {
        return None;
    }
    Some(map_footer_menu_row(y - top, start, pin_title, item_start))
}

pub fn menu_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.menu_visible {
        return None;
    }
    let n = match_commands(&app.menu_typed(), &app.slash_commands).len();
    if n == 0 {
        return None;
    }
    footer_menu_row_at_y(app, y, n)
}

pub fn manage_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.manage_visible {
        return None;
    }
    let n = app.manage_agents.len() + 1;
    let idx = footer_menu_row_at_y(app, y, n)?;
    if idx == 0 {
        return None; // title row is not selectable
    }
    let agent_idx = idx - 1;
    if agent_idx >= app.manage_agents.len() {
        return None;
    }
    Some(agent_idx)
}

pub fn picker_row_at_y(app: &App, y: usize) -> Option<usize> {
    if matches!(app.picker_kind, PickerKind::None) {
        return None;
    }
    let n = app.picker_items.len() + 1;
    let idx = footer_menu_row_at_y(app, y, n)?;
    if idx == 0 {
        return None;
    }
    let item_idx = idx - 1;
    if item_idx >= app.picker_items.len() {
        return None;
    }
    Some(item_idx)
}

pub fn context_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.context_visible {
        return None;
    }
    let n = app.context_rows.len() + 1;
    let idx = footer_menu_row_at_y(app, y, n)?;
    if idx == 0 {
        return None;
    }
    let row_idx = idx - 1;
    if row_idx >= app.context_rows.len() {
        return None;
    }
    Some(row_idx)
}

pub fn reasoning_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.reasoning_visible {
        return None;
    }
    let n = app.thinking_levels.len() + 1;
    let idx = footer_menu_row_at_y(app, y, n)?;
    if idx == 0 {
        return None;
    }
    let row_idx = idx - 1;
    if row_idx >= app.thinking_levels.len() {
        return None;
    }
    Some(row_idx)
}

pub fn at_menu_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.at_menu_visible {
        return None;
    }
    let n = app.at_menu_items.len();
    if n == 0 {
        return None;
    }
    let idx = footer_menu_row_at_y(app, y, n)?;
    if idx >= n {
        return None;
    }
    Some(idx)
}

// ---------------------------------------------------------------------------
// Full-screen overlay hit testing.
// ---------------------------------------------------------------------------

pub fn hover_overlay_row(app: &mut App, y: usize) {
    let Some(kind) = app.overlay else {
        return;
    };
    // All fullscreen overlays render through the shared template, so a
    // single spec-based hit-test covers hover as well as click.
    let (width, height, data) = overlay_hit_geometry(app);
    let spec = overlay_spec(app, kind, &data);
    // Raw screen Y → content-space Y (inside EDGE_PAD padding).
    let y = y.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    if let Some(idx) = crate::fullscreen_view::hit_test(&spec, y, width, height) {
        if matches!(data.rows[idx], crate::fullscreen_view::ViewRow::Item(_)) {
            app.overlay_sel = idx;
            sync_overlay_scroll(app);
        }
    }
}

pub fn click_overlay(app: &mut App, y: usize) -> Vec<crate::events::Effect> {
    let Some(kind) = app.overlay else {
        return Vec::new();
    };
    let (width, height, data) = overlay_hit_geometry(app);
    let spec = overlay_spec(app, kind, &data);
    // Raw screen Y → content-space Y (inside EDGE_PAD padding).
    let y = y.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    if let Some(idx) = crate::fullscreen_view::hit_test(&spec, y, width, height) {
        // Only selectable Item rows confirm; headers and raw display
        // rows (e.g. the /stats report body) are inert.
        if matches!(data.rows[idx], crate::fullscreen_view::ViewRow::Item(_)) {
            app.overlay_sel = idx;
            sync_overlay_scroll(app);
            return app.confirm_overlay();
        }
    }
    Vec::new()
}

/// Rows + footer for a fullscreen overlay view, shared by the renderer
/// and the click/hover hit-test paths so both always agree on geometry.
pub struct OverlayViewData {
    pub rows: Vec<crate::fullscreen_view::ViewRow>,
    pub footer: String,
}

impl OverlayViewData {
    fn new(rows: Vec<crate::fullscreen_view::ViewRow>, footer: String) -> Self {
        Self { rows, footer }
    }
}

/// Alias kept from the days when only /fork used the template.
pub type ForkViewData = OverlayViewData;

/// Rows + footer for any fullscreen overlay view (same builder backs
/// the renderer and the click/hover hit-test so the geometry can never
/// drift apart).
pub fn overlay_view_data(app: &App, kind: OverlayKind) -> OverlayViewData {
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    use crate::fullscreen_view::{ViewItem, ViewRow};
    match kind {
        OverlayKind::Fork => {
            let data = fork_view_rows(app);
            OverlayViewData::new(data.rows, data.footer)
        }
        OverlayKind::Model => {
            let filtered_count = filter_entries(&app.overlay_entries, &app.overlay_q).len();
            let rows = model_rows(app)
                .iter()
                .map(|row| match &row.entry {
                    Some(entry) => ViewRow::Item(ViewItem {
                        id: Some(format!("{}/{}", entry.provider.name, entry.model)),
                        label: entry.model.clone(),
                        trailing: entry.provider.name.clone(),
                        meta: String::new(),
                        marker: String::new(),
                        swatch: Vec::new(),
                        badges: Vec::new(),
                    }),
                    None => ViewRow::Header(row.label.clone()),
                })
                .collect();
            // Footer counts selectable models up to and including the
            // selection against the pre-grouping filtered count.
            let selected = model_rows(app)
                .iter()
                .take(app.overlay_sel.saturating_add(1))
                .filter(|row| row.entry.is_some())
                .count();
            let footer = if filtered_count > 0 {
                format!("{selected}/{filtered_count} models")
            } else {
                String::new()
            };
            OverlayViewData::new(rows, footer)
        }
        OverlayKind::Session => {
            let rows = session_rows(app)
                .iter()
                .map(|r| {
                    if r.date {
                        return ViewRow::Header(r.label.clone());
                    }
                    let mut item = ViewItem::new(r.label.clone());
                    item.id = r.sess.as_ref().map(|s| s.id.clone());
                    item.marker = if r.sess.as_ref().is_some_and(|s| s.id == app.session.id) {
                        "→ ".to_string()
                    } else {
                        String::new()
                    };
                    item.trailing = if r.sess.as_ref().is_some_and(|s| !s.parent_id.is_empty()) {
                        SUBAGENT_TAG.to_string()
                    } else {
                        String::new()
                    };
                    ViewRow::Item(item)
                })
                .collect();
            // Footer counts selectable sessions.
            let mut sel_count = 0usize;
            let mut total = 0usize;
            for (i, r) in session_rows(app).iter().enumerate() {
                if r.date {
                    continue;
                }
                total += 1;
                if i <= app.overlay_sel {
                    sel_count += 1;
                }
            }
            let footer = if total > 0 {
                format!("{sel_count}/{total} sessions")
            } else {
                String::new()
            };
            OverlayViewData::new(rows, footer)
        }
        OverlayKind::Stats => {
            let Some(report) = &app.overlay_stats else {
                return OverlayViewData::new(Vec::new(), String::new());
            };
            // Render at content width (the template already insets the
            // view by EDGE_PAD on every side).
            let lines = atom_core::session::stats::render_stats(report, width as i32, true);
            let rows = lines
                .into_iter()
                .map(|line| ViewRow::Raw(crate::ansi::ansi_to_line(&line).spans))
                .collect();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::Profile => {
            let Some(report) = &app.overlay_profile else {
                return OverlayViewData::new(Vec::new(), String::new());
            };
            // Profile is read-only — render at the current wall clock so the
            // "client uptime" and "server uptime" rows stay in sync
            // with the rest of the overlay (slight drift is fine).
            let lines = crate::profile::render_profile(report, std::time::SystemTime::now());
            let rows = lines
                .into_iter()
                .map(|line| ViewRow::Raw(vec![ratatui::text::Span::raw(line)]))
                .collect();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::Providers => {
            let filtered = atom_core::providers::providers::filter_provider_entries(
                &app.overlay_providers,
                &app.overlay_q,
            );
            let rows = filtered
                .iter()
                .map(|e| {
                    ViewRow::Item(ViewItem {
                        id: Some(e.id.clone()),
                        label: e.label.clone(),
                        trailing: e.status.clone(),
                        meta: String::new(),
                        marker: String::new(),
                        swatch: Vec::new(),
                        badges: e.caps.iter().map(|c| c.to_string()).collect(),
                    })
                })
                .collect();
            let footer = if filtered.is_empty() {
                String::new()
            } else {
                format!(
                    "{}/{} providers",
                    app.overlay_sel.min(filtered.len().saturating_sub(1)) + 1,
                    filtered.len()
                )
            };
            OverlayViewData::new(rows, footer)
        }
        OverlayKind::ProviderMethod => {
            let rows = ["API Key", "OAuth"]
                .iter()
                .map(|label| ViewRow::Item(ViewItem::new((*label).to_string())))
                .collect();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::ProviderKey => OverlayViewData::new(Vec::new(), String::new()),
        OverlayKind::Settings => {
            let rows = settings_labels(app)
                .into_iter()
                .map(|label| ViewRow::Item(ViewItem::new(label)))
                .collect::<Vec<_>>();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::WebSearch => {
            let rows = web_search_rows(app)
                .iter()
                .map(|(_, name, meta)| {
                    ViewRow::Item(ViewItem {
                        id: None,
                        label: name.clone(),
                        trailing: meta.clone(),
                        meta: String::new(),
                        marker: String::new(),
                        swatch: Vec::new(),
                        badges: Vec::new(),
                    })
                })
                .collect();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::WebFetch => {
            let rows = web_fetch_rows(app)
                .iter()
                .map(|(_, name, meta)| {
                    ViewRow::Item(ViewItem {
                        id: None,
                        label: name.clone(),
                        trailing: meta.clone(),
                        meta: String::new(),
                        marker: String::new(),
                        swatch: Vec::new(),
                        badges: Vec::new(),
                    })
                })
                .collect();
            OverlayViewData::new(rows, String::new())
        }
        OverlayKind::Theme => {
            let active = atom_core::render::colors::active_theme_name();
            let rows = theme_rows()
                .iter()
                .filter(|e| filter_theme_match(e, &app.overlay_q))
                .map(|entry| {
                    ViewRow::Item(ViewItem {
                        id: Some(entry.id.clone()),
                        label: entry.name.clone(),
                        trailing: if entry.builtin {
                            "built-in".to_string()
                        } else {
                            "custom".to_string()
                        },
                        meta: String::new(),
                        marker: if entry.id == active {
                            "● ".to_string()
                        } else {
                            String::new()
                        },
                        swatch: vec![
                            entry.theme.background.clone(),
                            entry.theme.primary.clone(),
                            entry.theme.secondary.clone(),
                            entry.theme.foreground.clone(),
                        ],
                        badges: Vec::new(),
                    })
                })
                .collect();
            OverlayViewData::new(rows, String::new())
        }
    }
}

pub fn fork_view_data(app: &App) -> ForkViewData {
    fork_view_rows(app)
}

fn fork_view_rows(app: &App) -> OverlayViewData {
    let rows = fork_rows(app);
    let view_rows: Vec<crate::fullscreen_view::ViewRow> = rows
        .iter()
        .map(|r| match r.kind {
            ForkRowKind::Header => crate::fullscreen_view::ViewRow::Header(r.label.clone()),
            ForkRowKind::SessionLatest | ForkRowKind::UserMessage => {
                crate::fullscreen_view::ViewRow::Item(crate::fullscreen_view::ViewItem {
                    id: r.position.map(|p| p.to_string()),
                    label: r.label.clone(),
                    trailing: r.timestamp.clone(),
                    meta: String::new(),
                    marker: String::new(),
                    swatch: Vec::new(),
                    badges: Vec::new(),
                })
            }
        })
        .collect();
    let footer = fork_footer(app, &view_rows);
    OverlayViewData::new(view_rows, footer)
}

/// Title / description / search placeholder chrome for an overlay. Render
/// and hit-test both go through this so chrome row counts (title,
/// description, search) can never drift apart. An empty placeholder
/// hides the search row entirely.
pub fn overlay_chrome(app: &App, kind: OverlayKind) -> (String, String, String) {
    let query_hint = "type to search, ↑↓ to navigate, Enter to select, ";
    let (title, description, placeholder) = match kind {
        OverlayKind::Model => (
            "Select model".to_string(),
            format!("{query_hint}Ctrl+P to pin"),
            "Search".to_string(),
        ),
        OverlayKind::Session => (
            "Sessions".to_string(),
            format!("{query_hint}Ctrl+P to pin, Ctrl+D to delete"),
            "Search".to_string(),
        ),
        OverlayKind::Stats => {
            let window = if app.stats_days > 0 {
                format!("last {} days", app.stats_days)
            } else {
                "all time".to_string()
            };
            (
                "Stats".to_string(),
                format!("token usage ({window}) — ↑↓ to scroll"),
                String::new(),
            )
        }
        OverlayKind::Providers => (
            "Providers".to_string(),
            "type to search, ↑↓ to navigate, Enter to add/update, Ctrl+D to disconnect".to_string(),
            "Search".to_string(),
        ),
        OverlayKind::ProviderMethod => (
            format!("Auth for {}", app.overlay_auth_id),
            "↑↓ to navigate, Enter to select".to_string(),
            String::new(),
        ),
        OverlayKind::ProviderKey => {
            let secret = if app.overlay_auth_type == "oauth" {
                "OAuth access token"
            } else {
                "API key"
            };
            (
                format!("Auth for {}", app.overlay_auth_id),
                format!("enter {secret} — Enter to save"),
                secret.to_string(),
            )
        }
        OverlayKind::Settings => (
            "Settings".to_string(),
            "↑↓ to navigate, Enter to change".to_string(),
            String::new(),
        ),
        OverlayKind::WebSearch => (
            "Web search provider".to_string(),
            "↑↓ to navigate, Enter to select".to_string(),
            String::new(),
        ),
        OverlayKind::WebFetch => (
            "Web fetch provider".to_string(),
            "↑↓ to navigate, Enter to select".to_string(),
            String::new(),
        ),
        OverlayKind::Theme => (
            "Theme".to_string(),
            format!("{query_hint}Enter to apply"),
            "Search".to_string(),
        ),
        OverlayKind::Profile => (
            "Profile".to_string(),
            "startup time + CPU/memory — ↑↓ to scroll".to_string(),
            String::new(),
        ),
        OverlayKind::Fork => (
            "Fork".to_string(),
            "type to filter, ↑↓ to navigate, Enter to fork".to_string(),
            "Search".to_string(),
        ),
    };
    (title, description, placeholder)
}

/// The ViewSpec for any fullscreen overlay. Render and hit-test both
/// go through this so chrome row counts (title, description, search)
/// can never drift apart.
pub fn overlay_spec<'a>(
    app: &'a App,
    kind: OverlayKind,
    data: &'a OverlayViewData,
) -> crate::fullscreen_view::ViewSpec<'a> {
    let (title, description, placeholder) = overlay_chrome(app, kind);
    let loading = if app.working_msg.is_empty() {
        None
    } else {
        Some(app.working_msg.as_str())
    };
    crate::fullscreen_view::ViewSpec {
        title,
        description,
        search_placeholder: placeholder,
        search_query: app.overlay_q.as_str(),
        search_selected: app.overlay_q_sel,
        search_style: crate::fullscreen_view::SearchStyle::Inline,
        rows: &data.rows,
        selected: app.overlay_sel.min(data.rows.len().saturating_sub(1)),
        scroll: app.overlay_scroll,
        footer: data.footer.as_str(),
        loading,
        spinner_frame: app.spinner_frame,
        // The provider-key prompt is an input-only view: its list is
        // empty by design, so the "no matches" placeholder is noise.
        hide_empty_state: kind == OverlayKind::ProviderKey,
    }
}

/// Hit-test geometry for any fullscreen overlay: (content width,
/// content height, data). Mouse Ys arrive in raw terminal rows; the
/// view is drawn inside `EDGE_PAD` padding, so hit-testers translate
/// the raw screen Y into content space by subtracting
/// [`crate::fullscreen_view::EDGE_PAD`].
fn overlay_hit_geometry(app: &App) -> (usize, usize, OverlayViewData) {
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let height = crate::fullscreen_view::content_height(app.height.max(1) as usize);
    let kind = app.overlay.unwrap_or(OverlayKind::Fork);
    (width, height, overlay_view_data(app, kind))
}

/// True when a raw screen coordinate lands on the overlay title row's
/// right-aligned `esc` dismiss hint, so clicking it closes the overlay.
pub fn overlay_esc_hint_hit(app: &App, x_raw: usize, y_raw: usize) -> bool {
    let Some(kind) = app.overlay else {
        return false;
    };
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let x = x_raw.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    let y = y_raw.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    let data = overlay_view_data(app, kind);
    let spec = overlay_spec(app, kind, &data);
    crate::fullscreen_view::esc_hint_hit(&spec, x, y, width)
}

/// Click-to-place the caret in a fullscreen overlay's search input,
/// like a normal text field. Returns true when the click landed on the
/// search row and was consumed.
pub fn overlay_click_search(app: &mut App, x_raw: usize, y_raw: usize) -> bool {
    let Some(kind) = app.overlay else {
        return false;
    };
    if !overlay_has_query(Some(kind)) {
        return false;
    }
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let x = x_raw.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    let y = y_raw.saturating_sub(crate::fullscreen_view::EDGE_PAD);
    let data = overlay_view_data(app, kind);
    let spec = overlay_spec(app, kind, &data);
    // Clicks on the search row (and only it) place the caret on that
    // cell.
    if y != crate::fullscreen_view::search_row_top(&spec, width) {
        return false;
    }
    // Place the caret at the char boundary at (or left of) the click
    // column; past the end of the query it lands at the end.
    let caret = crate::fullscreen_view::search_caret_char_at(&app.overlay_q, x);
    app.overlay_q_cursor = Some(caret);
    app.overlay_q_sel = false;
    true
}

/// Keep the current selection visible using the shared template's
/// geometry, so render and scroll math agree everywhere.
pub fn sync_overlay_scroll(app: &mut App) {
    let Some(kind) = app.overlay else {
        return;
    };
    if !matches!(
        kind,
        OverlayKind::Model | OverlayKind::Session | OverlayKind::Providers | OverlayKind::Fork
    ) {
        return;
    }
    let width = crate::fullscreen_view::content_width(app.width.max(1) as usize);
    let height = crate::fullscreen_view::content_height(app.height.max(1) as usize);
    let data = overlay_view_data(app, kind);
    let counts = crate::fullscreen_view::row_line_counts(&data.rows, width);
    let spec = overlay_spec(app, kind, &data);
    let visible = crate::fullscreen_view::list_visible_rows(&spec, width, height);
    app.overlay_scroll = overlay_keep_visible(
        app.overlay_scroll,
        app.overlay_sel,
        visible,
        counts.len(),
        |i| counts.get(i).copied().unwrap_or(1),
    );
}

/// Shared footer helper used by both the renderer and the click/hover
/// paths so they agree on the visible count.
fn fork_footer(app: &App, view_rows: &[crate::fullscreen_view::ViewRow]) -> String {
    if app.overlay_fork_user_messages.is_empty() {
        return String::new();
    }
    let visible = view_rows
        .iter()
        .filter(|r| matches!(r, crate::fullscreen_view::ViewRow::Item(item) if item.id.is_some()))
        .count();
    let total = app.overlay_fork_user_messages.len();
    if visible == total {
        format!("{visible}/{total} from message")
    } else {
        format!("{visible}/{total} from message match")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_parsing_table() {
        assert_eq!(parse_stats_days("/stats"), 0);
        assert_eq!(parse_stats_days("/stats 30"), 30);
        assert_eq!(parse_stats_days("/stats days30"), 30);
        assert_eq!(parse_stats_days("/stats days 7"), 7);
        assert_eq!(parse_stats_days("/stats -5"), 0);
        assert_eq!(parse_stats_days("hello"), 0);
        // Multi-byte runes must not panic on the fixed byte-offset check.
        assert_eq!(parse_stats_days("/stats\u{2014}\u{2014}days30"), 0);
        assert_eq!(parse_stats_days("/stats \u{e9}\u{e9}\u{e9}"), 0);
    }

    #[test]
    fn slash_matching_builtin_and_stats30() {
        // A bare "/" shows only the default shortlist.
        let base = match_commands("/", &[]);
        let names: Vec<&str> = base.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "/new",
                "/sessions",
                "/model",
                "/providers",
                "/subagents",
                "/compact",
                "/quit"
            ]
        );
        // One typed character opens up the rest of the catalog.
        let st = match_commands("/st", &[]);
        let names: Vec<&str> = st.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/stats", "/stats 30"]);
        let se = match_commands("/se", &[]);
        let names: Vec<&str> = se.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/sessions", "/settings"]);
    }

    #[test]
    fn slash_filtering_uses_discovery_snapshot_and_preserves_order() {
        let cwd = std::env::temp_dir().join(format!(
            "atom-tui-slash-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = cwd.join(".atom/skills/cache-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: cache-skill\ndescription: cached skill\n---\nbody\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join(".atom")).unwrap();
        std::fs::write(
            cwd.join(".atom/mcp.json"),
            r#"{"mcpServers":{"cache-mcp":{"command":"cached mcp"}}}"#,
        )
        .unwrap();

        let discovered = discover_commands(cwd.to_str().unwrap());
        std::fs::remove_dir_all(&cwd).unwrap();

        let matches = match_commands("/cache-", &discovered);
        let rows: Vec<(&str, &str, &str)> = matches
            .iter()
            .map(|command| {
                (
                    command.name.as_str(),
                    command.desc.as_str(),
                    command.kind.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("/cache-skill", "cached skill", "skill"),
                ("/cache-mcp", "cached mcp", "mcp"),
            ]
        );
    }

    #[test]
    fn slash_query_detection() {
        assert!(is_slash_query("/model"));
        assert!(!is_slash_query("/model extra"));
        assert!(!is_slash_query("hello"));
    }

    #[test]
    fn keep_visible_scrolls_to_selection() {
        let lc = |_i: usize| 1;
        assert_eq!(overlay_keep_visible(0, 0, 5, 20, lc), 0);
        assert_eq!(overlay_keep_visible(0, 10, 5, 20, lc), 6);
        assert_eq!(overlay_keep_visible(6, 6, 5, 20, lc), 6); // sticky when visible
    }

    #[test]
    fn footer_window_clips_and_pins_title() {
        // small menu fits whole
        let (start, vis, pin, item_start) = footer_menu_window(4, 10, 2, 1);
        assert_eq!((start, vis, pin, item_start), (0, 4, true, 1));
        // large menu clips around selection
        let (start, vis, pin, item_start) = footer_menu_window(50, 5, 40, 1);
        assert_eq!(vis, 5); // 1 title + 4 items
        assert!(pin);
        assert_eq!(item_start, 38); // title + window over items
        assert_eq!(start, 0);
    }

    #[test]
    fn map_rows_through_clipped_window() {
        assert_eq!(map_footer_menu_row(0, 10, true, 33), 0);
        assert_eq!(map_footer_menu_row(1, 10, true, 33), 33);
        assert_eq!(map_footer_menu_row(3, 10, false, 0), 13);
    }

    #[test]
    fn filter_sessions_multiword() {
        let mk = |id: &str, model: &str, title: &str| SessionInfo {
            id: id.into(),
            model: model.into(),
            title: title.into(),
            ..crate::app::empty_session_info()
        };
        let sessions = vec![mk("aaa", "gpt-5", "first"), mk("bbb", "ollama", "second")];
        let got = filter_sessions(&sessions, "");
        assert_eq!(got.len(), 2);
        let got = filter_sessions(&sessions, "OLLAMA second");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "bbb");
        let got = filter_sessions(&sessions, "gpt missing");
        assert!(got.is_empty());
    }

    #[test]
    fn model_rows_group_pinned_recent_and_remaining() {
        let mut app = App::new_test(80, 24);
        let entry = |model: &str| ModelEntry {
            provider: atom_core::providers::providers::Provider {
                name: "openai".into(),
                ..Default::default()
            },
            model: model.into(),
        };
        app.overlay_entries = vec![entry("a"), entry("b"), entry("c")];
        app.picker_settings.favorites =
            vec![crate::settings::PickerSettings::model_ref("openai", "b")];
        app.picker_settings.recents = vec![
            crate::settings::PickerSettings::model_ref("openai", "b"),
            crate::settings::PickerSettings::model_ref("openai", "c"),
        ];

        let rows = model_rows(&app);
        assert_eq!(rows[0].label, "Pinned");
        assert_eq!(rows[1].entry.as_ref().unwrap().model, "b");
        assert_eq!(rows[2].label, "Recent");
        assert_eq!(rows[3].entry.as_ref().unwrap().model, "c");
        assert_eq!(rows[4].label, "Models");
        assert_eq!(rows[5].entry.as_ref().unwrap().model, "a");
    }

    #[test]
    fn model_picker_hides_empty_groups() {
        let mut app = App::new_test(80, 24);
        app.overlay_entries = vec![ModelEntry {
            provider: atom_core::providers::providers::Provider {
                name: "openai".into(),
                ..Default::default()
            },
            model: "gpt-5".into(),
        }];
        let rows = model_rows(&app);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Models");
        assert_eq!(rows[1].entry.as_ref().unwrap().model, "gpt-5");

        app.picker_settings
            .favorites
            .push(crate::settings::PickerSettings::model_ref(
                "openai", "gpt-5",
            ));
        let rows = model_rows(&app);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Pinned");
        assert_eq!(rows[1].entry.as_ref().unwrap().model, "gpt-5");
        app.picker_settings
            .recents
            .push(crate::settings::PickerSettings::model_ref(
                "openai", "gpt-5",
            ));
        // Recent dedupes against pinned, so the section stays hidden.
        assert_eq!(model_rows(&app).len(), 2);
        app.overlay_entries.push(ModelEntry {
            provider: atom_core::providers::providers::Provider {
                name: "openai".into(),
                ..Default::default()
            },
            model: "gpt-4".into(),
        });
        app.picker_settings.recents.insert(
            0,
            crate::settings::PickerSettings::model_ref("openai", "gpt-4"),
        );
        let rows = model_rows(&app);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "Pinned");
        assert_eq!(rows[2].label, "Recent");
        assert_eq!(rows[3].entry.as_ref().unwrap().model, "gpt-4");
    }

    #[test]
    fn session_picker_keeps_pinned_none_placeholder() {
        let mut app = App::new_test(80, 24);
        app.overlay_sessions = vec![SessionInfo {
            id: "s1".into(),
            title: "chat".into(),
            ..crate::app::empty_session_info()
        }];
        assert_eq!(session_rows(&app)[0].label, "Pinned - None");
    }

    // -- /fork picker -----------------------------------------------------

    fn user_msg(position: i64, preview: &str) -> crate::app::ForkUserMessage {
        crate::app::ForkUserMessage {
            position,
            preview: preview.into(),
            timestamp: "—".into(),
        }
    }

    #[test]
    fn fork_overlay_filter_drops_user_messages_keeps_session_latest() {
        let mut app = App::new_test(80, 24);
        app.overlay_fork_user_messages = vec![
            user_msg(0, "convert loader"),
            user_msg(1, "extract auth"),
            user_msg(2, "add retry route"),
        ];
        let rows = fork_rows(&app);
        // Layout: Session header, SessionLatest, User messages header, 3 rows.
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].kind, ForkRowKind::Header);
        assert_eq!(rows[1].kind, ForkRowKind::SessionLatest);
        assert_eq!(rows[2].kind, ForkRowKind::Header);
        assert_eq!(rows[3].kind, ForkRowKind::UserMessage);
        assert_eq!(rows[4].kind, ForkRowKind::UserMessage);
        assert_eq!(rows[5].kind, ForkRowKind::UserMessage);

        // Typing "auth" should filter to the matching row, but the
        // SessionLatest sentinel stays at the top.
        app.overlay_q = "auth".into();
        let rows = fork_rows(&app);
        assert_eq!(rows[1].kind, ForkRowKind::SessionLatest);
        assert_eq!(rows[3].kind, ForkRowKind::UserMessage);
        assert!(rows[3].label.contains("auth"));
        // The other two rows are dropped by the filter.
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn fork_overlay_skips_header_rows_in_nav() {
        let rows = vec![
            ForkRow {
                kind: ForkRowKind::Header,
                label: "Session".into(),
                timestamp: String::new(),
                position: None,
            },
            ForkRow {
                kind: ForkRowKind::SessionLatest,
                label: "latest".into(),
                timestamp: String::new(),
                position: None,
            },
            ForkRow {
                kind: ForkRowKind::Header,
                label: "User messages".into(),
                timestamp: String::new(),
                position: None,
            },
            ForkRow {
                kind: ForkRowKind::UserMessage,
                label: "msg 1".into(),
                timestamp: String::new(),
                position: Some(0),
            },
        ];
        assert_eq!(move_fork_sel(&rows, 0, 1), 1);
        assert_eq!(move_fork_sel(&rows, 1, 1), 3);
        assert_eq!(move_fork_sel(&rows, 3, -1), 1);
        // Past the end clamps to the last item.
        assert_eq!(move_fork_sel(&rows, 3, 1), 3);
        // Before the start clamps to the first item.
        assert_eq!(move_fork_sel(&rows, 0, -1), 0);
    }

    #[test]
    fn fork_overlay_enter_emits_fork_session_with_selected_position() {
        // This is the lookup pattern used by confirm_overlay: read the
        // selected row, emit Effect::ForkSession with the chosen
        // position. SessionLatest → None; UserMessage → Some(pos).
        let mut rows = vec![
            ForkRow {
                kind: ForkRowKind::SessionLatest,
                label: "latest".into(),
                timestamp: String::new(),
                position: None,
            },
            ForkRow {
                kind: ForkRowKind::UserMessage,
                label: "msg 0".into(),
                timestamp: String::new(),
                position: Some(0),
            },
            ForkRow {
                kind: ForkRowKind::UserMessage,
                label: "msg 1".into(),
                timestamp: String::new(),
                position: Some(1),
            },
        ];
        // SessionLatest → position = None.
        let row = &rows[0];
        let position = match row.kind {
            ForkRowKind::SessionLatest => None,
            ForkRowKind::UserMessage => row.position,
            ForkRowKind::Header => None,
        };
        assert!(position.is_none());
        // UserMessage → position = Some(1).
        rows[1].kind = ForkRowKind::UserMessage;
        let row = &rows[1];
        let position = match row.kind {
            ForkRowKind::SessionLatest => None,
            ForkRowKind::UserMessage => row.position,
            ForkRowKind::Header => None,
        };
        assert_eq!(position, Some(0));
    }

    #[test]
    fn fork_click_on_search_row_places_caret() {
        // A click on the search row places the caret on the clicked
        // text cell; clicks above (title) and below (list) are passed
        // through.
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Fork);
        app.overlay_q = "abc".into();

        let width = crate::fullscreen_view::content_width(80);
        let data = overlay_view_data(&app, OverlayKind::Fork);
        let spec = overlay_spec(&app, OverlayKind::Fork, &data);
        let search_row =
            crate::fullscreen_view::search_row_top(&spec, width) + crate::fullscreen_view::EDGE_PAD;

        // Click on the row's first text cell → caret at char 0.
        assert!(overlay_click_search(&mut app, 1, search_row));
        assert_eq!(app.overlay_q_cursor, Some(0));

        // Click on the third text cell → caret at char 2.
        assert!(overlay_click_search(&mut app, 1 + 2, search_row));
        assert_eq!(app.overlay_q_cursor, Some(2));

        // Above the input (title row): not consumed (caret unchanged).
        assert!(!overlay_click_search(&mut app, 1, search_row - 1));
        assert_eq!(app.overlay_q_cursor, Some(2));

        // Below the input (list area): not consumed (caret unchanged).
        assert!(!overlay_click_search(&mut app, 1, search_row + 1));
        assert_eq!(app.overlay_q_cursor, Some(2));
    }
}
