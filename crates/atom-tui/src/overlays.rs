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
    Theme,
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
}

/// command is a slash command the user can type in the chat.
#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
    /// "" built-in, "skill", or "mcp"
    pub kind: &'static str,
}

pub const COMMANDS: [Command; 15] = [
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
                let meta = if !cfg.command.trim().is_empty() {
                    cfg.command.trim().to_string()
                } else {
                    cfg.url.trim().to_string()
                };
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
    let builtins = if typed == "/" {
        COMMANDS
            .iter()
            .filter(|c| DEFAULT_COMMANDS.contains(&c.name))
            .map(DynamicCommand::builtin)
            .collect::<Vec<_>>()
    } else {
        COMMANDS
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
        Some(OverlayKind::Settings) => 3,
        Some(OverlayKind::WebSearch) => web_search_rows(app).len(),
        Some(OverlayKind::Theme) => atom_core::render::colors::available_themes().len(),
        _ => 0,
    }
}

pub fn settings_labels(app: &App) -> Vec<String> {
    let compaction = app.atom_config.resolved_compaction();
    let web = app.atom_config.resolved_web_search();
    vec![
        format!(
            "Compaction model  {} / {}",
            compaction.provider, compaction.model
        ),
        format!("Web search provider  {}", web.server),
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

// ---------------------------------------------------------------------------
// Scroll math (direct ports).
// ---------------------------------------------------------------------------

pub fn overlay_list_max_items(height: u16) -> usize {
    let n = height as i32 - 6;
    n.max(3) as usize
}

pub fn overlay_list_max_lines(app: &App) -> usize {
    (app.height as usize)
        .saturating_sub(overlay_header_rows(app))
        .saturating_sub(2)
        .max(3)
}

pub fn overlay_list_scroll(sel: usize, max_items: usize, n: usize) -> usize {
    let sel = sel.min(n.saturating_sub(1));
    if max_items == 0 || n == 0 {
        return 0;
    }
    if sel >= max_items {
        sel - max_items + 1
    } else {
        0
    }
}

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

/// overlayForEachVisible paints list rows from scroll until the budget
/// is spent, always including sel even when taller than maxLines.
pub fn overlay_for_each_visible(
    scroll: usize,
    sel: usize,
    max_lines: usize,
    n: usize,
    line_count: impl Fn(usize) -> usize,
    mut f: impl FnMut(usize),
) {
    if n == 0 || scroll >= n {
        return;
    }
    let max_lines = max_lines.max(1);
    let mut used = 0usize;
    for i in scroll..n {
        let mut h = line_count(i);
        if h < 1 {
            h = 1;
        }
        if used > 0 && used + h > max_lines && i != sel {
            break;
        }
        f(i);
        used += h;
        if used >= max_lines {
            break;
        }
    }
}

pub fn overlay_hit_index(
    rel: usize,
    sel: usize,
    max_items: usize,
    n: usize,
    lines: impl Fn(usize) -> usize,
) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let scroll = overlay_list_scroll(sel, max_items, n);
    let end = (scroll + max_items).min(n);
    let mut row = 0usize;
    for i in scroll..end {
        let mut h = lines(i);
        if h < 1 {
            h = 1;
        }
        if rel >= row && rel < row + h {
            return Some(i);
        }
        row += h;
    }
    None
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
    rows.push(ModelRow {
        label: if pinned.is_empty() {
            "Pinned - None".to_string()
        } else {
            "Pinned".to_string()
        },
        entry: None,
    });
    rows.extend(pinned.into_iter().map(|entry| ModelRow {
        label: String::new(),
        entry: Some(entry),
    }));

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
    rows.push(ModelRow {
        label: if recent.is_empty() {
            "Recent - None".to_string()
        } else {
            "Recent".to_string()
        },
        entry: None,
    });
    rows.extend(recent.into_iter().map(|entry| ModelRow {
        label: String::new(),
        entry: Some(entry),
    }));

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

fn model_row_line_counts(app: &App) -> Vec<usize> {
    let width = app.width.max(1) as usize;
    model_rows(app)
        .iter()
        .enumerate()
        .map(|(i, row)| match &row.entry {
            Some(entry) => wrapped_label_line_count(
                &format!("{}  {}", entry.provider.name, entry.model),
                width,
                i == app.overlay_sel,
                "",
            ),
            None => wrap_plain(&row.label, width).len().max(1),
        })
        .collect()
}

pub fn sync_model_scroll(app: &mut App) {
    let counts = model_row_line_counts(app);
    app.overlay_scroll = overlay_keep_visible(
        app.overlay_scroll,
        app.overlay_sel,
        overlay_list_max_lines(app),
        counts.len(),
        |i| counts.get(i).copied().unwrap_or(1),
    );
}

// ---------------------------------------------------------------------------
// Session picker rows.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub date: bool,
    pub label: String,
    pub sess: Option<SessionInfo>,
}

/// Muted right-aligned tag marking subagent sessions in the picker.
pub const SUBAGENT_TAG: &str = "Subagent";
/// Columns the tag occupies: the tag plus the gap before it.
pub const SUBAGENT_TAG_WIDTH: usize = SUBAGENT_TAG.len() + 2;

/// Wrap width for a session row's label; subagent rows reserve space so
/// the tag can sit flush right on the first visual line.
fn session_label_width(width: usize, row: &SessionRow) -> usize {
    let is_sub = row.sess.as_ref().is_some_and(|s| !s.parent_id.is_empty());
    if is_sub {
        width.saturating_sub(SUBAGENT_TAG_WIDTH)
    } else {
        width
    }
}

/// dayLabel groups sessions under Today/Yesterday/date headers.
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
pub fn session_row_line_counts(app: &App) -> Vec<usize> {
    let rows = session_rows(app);
    let width = app.width.max(1) as usize;
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            if r.date {
                return 1;
            }
            let marker = if r
                .sess
                .as_ref()
                .map(|s| s.id == app.session.id)
                .unwrap_or(false)
            {
                "→ "
            } else {
                "  "
            };
            wrapped_label_line_count(
                &r.label,
                session_label_width(width, r),
                i == app.overlay_sel,
                marker,
            )
        })
        .collect()
}

pub fn sync_session_scroll(app: &mut App) {
    let counts = session_row_line_counts(app);
    let n = counts.len();
    let max_items = overlay_list_max_lines(app);
    app.overlay_scroll =
        overlay_keep_visible(app.overlay_scroll, app.overlay_sel, max_items, n, |i| {
            counts.get(i).copied().unwrap_or(1)
        });
}

pub fn stats_scroll_max(app: &App) -> usize {
    let Some(report) = &app.overlay_stats else {
        return 0;
    };
    let lines = atom_core::session::stats::render_stats(report, app.width as i32 - 4, true);
    let visible = (app.height as i32 - 6).max(3) as usize;
    lines.len().saturating_sub(visible)
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

pub fn overlay_title(app: &App, kind: OverlayKind) -> String {
    match kind {
        OverlayKind::Model => "Select a model — type to search, ↑↓ to navigate, Enter to select, Ctrl+P to pin, Esc to cancel".to_string(),
        OverlayKind::Session => "Switch session — type to search, ↑↓ to navigate, Enter to select, Ctrl+P to pin, Ctrl+D to delete, Esc to cancel".to_string(),
        OverlayKind::Stats => {
            let window = if app.stats_days > 0 {
                format!("last {} days", app.stats_days)
            } else {
                "all time".to_string()
            };
            format!("Token usage ({window}) — ↑↓ to scroll, Esc to close")
        }
        OverlayKind::Providers => "Providers — type to search, ↑↓ to navigate, Enter to add/update, Ctrl+D to disconnect, Esc to cancel".to_string(),
        OverlayKind::ProviderMethod => format!(
            "Auth for {} — ↑↓ to navigate, Enter to select, Esc to go back",
            app.overlay_auth_id
        ),
        OverlayKind::ProviderKey => {
            let secret = if app.overlay_auth_type == "oauth" {
                "OAuth access token"
            } else {
                "API key"
            };
            format!(
                "Enter {secret} for {} — Enter to save, Esc to go back",
                app.overlay_auth_id
            )
        }
        OverlayKind::Settings =>
            "Settings — ↑↓ to navigate, Enter to change, Esc to close".to_string(),
        OverlayKind::WebSearch =>
            "Web search provider — ↑↓ to navigate, Enter to select, Esc to settings".to_string(),
        OverlayKind::Theme =>
            "Theme — ↑↓ to navigate, Enter to apply, Esc to cancel".to_string(),
    }
}

pub fn overlay_header_rows(app: &App) -> usize {
    let Some(kind) = app.overlay else {
        return 0;
    };
    let width = app.width.max(1) as usize;
    let title_rows = wrap_plain(&overlay_title(app, kind), width).len().max(1);
    match kind {
        OverlayKind::Model | OverlayKind::Session | OverlayKind::Providers => {
            let query = if app.overlay_q.is_empty() && kind == OverlayKind::Session {
                "> search".to_string()
            } else {
                format!("> {}", app.overlay_q)
            };
            title_rows + wrap_plain(&query, width).len().max(1) + 2
        }
        OverlayKind::ProviderMethod
        | OverlayKind::Settings
        | OverlayKind::WebSearch
        | OverlayKind::Theme => title_rows + 1,
        _ => 0,
    }
}

pub fn overlay_row_at_y(app: &App, y: usize) -> Option<usize> {
    if !app.working_msg.is_empty() {
        return None;
    }
    let header = overlay_header_rows(app);
    if header == 0 || y < header {
        return None;
    }
    let rel = y - header;
    let width = app.width.max(1) as usize;
    let max_items = overlay_list_max_lines(app);
    match app.overlay {
        Some(OverlayKind::Model) => {
            let rows = model_rows(app);
            let counts = model_row_line_counts(app);
            let mut hit = None;
            let mut row = 0usize;
            overlay_for_each_visible(
                app.overlay_scroll,
                app.overlay_sel,
                max_items,
                rows.len(),
                |i| counts.get(i).copied().unwrap_or(1),
                |i| {
                    if hit.is_some() {
                        return;
                    }
                    let h = counts.get(i).copied().unwrap_or(1).max(1);
                    if rel >= row && rel < row + h {
                        hit = Some(i);
                    }
                    row += h;
                },
            );
            hit
        }
        Some(OverlayKind::Providers) => {
            let filtered = atom_core::providers::providers::filter_provider_entries(
                &app.overlay_providers,
                &app.overlay_q,
            );
            overlay_hit_index(rel, app.overlay_sel, max_items, filtered.len(), |i| {
                let e = &filtered[i];
                let label = if e.status.is_empty() {
                    e.label.clone()
                } else {
                    format!("{}  {}", e.label, e.status)
                };
                wrapped_label_line_count(&label, width, i == app.overlay_sel, "")
            })
        }
        Some(OverlayKind::Session) => {
            let rows = session_rows(app);
            let counts = session_row_line_counts(app);
            let n = rows.len();
            if n == 0 {
                return None;
            }
            let mut hit = None;
            let mut row = 0usize;
            overlay_for_each_visible(
                app.overlay_scroll,
                app.overlay_sel,
                max_items,
                n,
                |i| counts.get(i).copied().unwrap_or(1),
                |i| {
                    if hit.is_some() {
                        return;
                    }
                    let h = counts.get(i).copied().unwrap_or(1).max(1);
                    if rel >= row && rel < row + h {
                        hit = Some(i);
                    }
                    row += h;
                },
            );
            hit
        }
        Some(OverlayKind::ProviderMethod) => {
            if rel < 2 {
                Some(rel)
            } else {
                None
            }
        }
        Some(OverlayKind::Settings) => {
            let labels = settings_labels(app);
            overlay_hit_index(rel, app.overlay_sel, max_items, labels.len(), |index| {
                wrapped_label_line_count(&labels[index], width, index == app.overlay_sel, "")
            })
        }
        Some(OverlayKind::WebSearch) => {
            let rows = web_search_rows(app);
            overlay_hit_index(rel, app.overlay_sel, max_items, rows.len(), |index| {
                let (_, name, meta) = &rows[index];
                wrapped_label_line_count(
                    &format!("{name}  {meta}"),
                    width,
                    index == app.overlay_sel,
                    "",
                )
            })
        }
        _ => None,
    }
}

pub fn hover_overlay_row(app: &mut App, y: usize) {
    if let Some(idx) = overlay_row_at_y(app, y) {
        let is_session_header = app.overlay == Some(OverlayKind::Session) && {
            let rows = session_rows(app);
            rows.get(idx).map(|r| r.date).unwrap_or(false)
        };
        let is_model_header = app.overlay == Some(OverlayKind::Model)
            && model_rows(app)
                .get(idx)
                .is_none_or(|row| row.entry.is_none());
        if !is_session_header && !is_model_header {
            app.overlay_sel = idx;
            if app.overlay == Some(OverlayKind::Session) {
                sync_session_scroll(app);
            } else if app.overlay == Some(OverlayKind::Model) {
                sync_model_scroll(app);
            }
        }
    }
}

pub fn click_overlay(app: &mut App, y: usize) -> Vec<crate::events::Effect> {
    let Some(idx) = overlay_row_at_y(app, y) else {
        return Vec::new();
    };
    if app.overlay == Some(OverlayKind::Session) {
        let rows = session_rows(app);
        if rows.get(idx).map(|r| r.date).unwrap_or(true) {
            return Vec::new();
        }
    } else if app.overlay == Some(OverlayKind::Model)
        && model_rows(app)
            .get(idx)
            .is_none_or(|row| row.entry.is_none())
    {
        return Vec::new();
    }
    app.overlay_sel = idx;
    app.confirm_overlay()
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
    fn list_scroll_windows_forward_only() {
        assert_eq!(overlay_list_scroll(0, 5, 100), 0);
        assert_eq!(overlay_list_scroll(7, 5, 100), 3);
        assert_eq!(overlay_list_scroll(99, 5, 100), 95);
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
    fn empty_picker_groups_show_none() {
        let mut app = App::new_test(80, 24);
        app.overlay_entries = vec![ModelEntry {
            provider: atom_core::providers::providers::Provider {
                name: "openai".into(),
                ..Default::default()
            },
            model: "gpt-5".into(),
        }];
        assert_eq!(model_rows(&app)[0].label, "Pinned - None");
        assert_eq!(model_rows(&app)[1].label, "Recent - None");

        app.overlay_sessions = vec![SessionInfo {
            id: "s1".into(),
            title: "chat".into(),
            ..crate::app::empty_session_info()
        }];
        assert_eq!(session_rows(&app)[0].label, "Pinned - None");
    }
}
