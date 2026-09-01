//! app.rs is the TUI state machine, mirroring tui.go's tuiModel and
//! Update: session/model state, the conversation block list with its
//! render cache, overlays and footer menus, mouse selection, streaming
//! state, and the key/mouse/event handlers that return Effects for the
//! runtime loop to perform.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::text::Line;

use atom_core::providers::auth::{self, AuthEntry};
use atom_core::providers::modelsdev;
use atom_core::providers::providers::{self, Provider, ProviderListEntry};
use atom_core::session::context_breakdown::ContextRow;
use atom_core::session::stats::StatsReport;
use atom_core::session::store::SessionInfo;
use atom_core::types::ImageData;

use crate::blocks::{self, Block, BlockKind};
use crate::events::{AppMsg, Effect, SendRequest, StreamEvent};
use crate::overlays;
use crate::overlays::{
    filter_provider_entries, DynamicCommand, OverlayKind, PickerItem, PickerKind,
};
use crate::preview::{self, PendingImage};
use crate::prompt::Prompt;

/// bubbles spinner.MiniDot frames (12 fps).
pub const MINIDOT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// inputMaxHeight is the tallest the prompt field may grow, in rows.
pub const INPUT_MAX_HEIGHT: usize = 8;

pub const VIEWPORT_VPAD: usize = 1;
/// Blank rows left at the bottom of the message viewport (between the
/// last content row and the prompt) when no footer menu is open. The
/// scrollbar and viewport rect still span the full region; only content
/// is clipped short of this padding.
pub const VIEWPORT_BOTTOM_PAD: usize = 1;
pub const PROMPT_PAD: usize = 1;
/// Rows below the status bar reserved for the card-dark cwd footer
/// (the directory the agent was invoked from).
pub const STATUS_FOOTER_ROWS: usize = 1;

/// tuiHPad is the left/right inset in cells.
pub const TUI_HPAD: usize = 1;

/// Width of the scrollbar gutter in columns.
pub const SCROLLBAR_WIDTH: usize = 2;

/// splashTickInterval drives the empty-session atom animation.
pub const SPLASH_TICK_MS: u64 = 33;

/// MiniDot runs at ~24 fps for smooth animation.
pub const SPINNER_TICK_MS: u64 = 42;

/// outputTestSceneDuration backs the --output-test scene timer.
pub const TEST_SCENE_TICK_SECS: u64 = 3;

pub struct RunOptions {
    pub providers: Vec<Provider>,
    pub sel_provider: Provider,
    pub sel_model: String,
    pub session: SessionInfo,
    pub hot_state_path: Option<std::path::PathBuf>,
    /// Wall-clock instant captured at the very top of `main()`. Used by
    /// the dev-only `/profile` overlay to display when the client
    /// started (HH:MM:SS) and how long it has been running. None in
    /// tests, where `App::new_test` skips the field.
    pub started_at: Option<SystemTime>,
    /// Monotonic companion to `started_at`: same wall-clock instant
    /// but tracked via `Instant` so the uptime math can't be fooled
    /// by clock skew. None whenever `started_at` is None.
    pub started_instant: Option<Instant>,
    /// PID of the background `atoms` server the client connected to.
    /// Read from the pid file `ensure_server` writes; None when the
    /// server isn't reachable, so the profile overlay can show "no
    /// server pid known" instead of crashing.
    pub server_pid: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub rule_id: String,
    pub reason: String,
    /// Session that owns the pending approval (POST /approval target).
    /// Differs from the viewed session when answering for a subagent.
    pub session_id: String,
    /// Set when the request comes from a dispatched subagent.
    pub child_title: String,
    pub from_subagent: bool,
}

/// One user message in the /fork overlay: a one-line preview plus the
/// timestamp tag rendered on the right edge. The server uses
/// `position` to truncate the transcript (the position is the index
/// into the source session's `messages` array); the client uses
/// `preview` to filter the picker.
#[derive(Debug, Clone)]
pub struct ForkUserMessage {
    pub position: i64,
    /// Single-line preview of the user message, already trimmed.
    pub preview: String,
    /// Local-time HH:MM formatted timestamp; falls back to "—" when
    /// the server didn't record `created_at` (older sessions).
    pub timestamp: String,
}

pub struct App {
    // session and provider state
    pub providers: Vec<Provider>,
    pub sel_provider: Provider,
    pub sel_model: String,
    pub session: SessionInfo,

    // thinking level
    pub thinking_levels: Vec<String>,
    pub thinking_idx: usize,
    pub thinking_pref: String,

    /// whether reasoning blocks are rendered; toggled with /thinking.
    pub show_reasoning: bool,

    // conversation
    pub blocks: Vec<Block>,
    pub content_lines: Vec<Arc<Line<'static>>>,
    /// Per content line, clickable OSC 8 link regions (parallel to
    /// content_lines).
    pub link_lines: Vec<Vec<crate::ansi::LinkRegion>>,
    pub block_start: Vec<usize>,
    pub content_width: usize,
    /// pinned to the newest output until the user scrolls up
    pub following: bool,
    /// viewport YOffset analog
    pub scroll_y: usize,

    // scrollbar mouse interaction
    pub scrollbar_dragging: bool,

    // performance: skip redundant refresh_viewport work during pure scrolls
    pub viewport_dirty: bool,

    // mouse text selection over the viewport ((line,col) pairs)
    pub sel_anchor: Option<(usize, usize)>,
    pub sel_end: Option<(usize, usize)>,
    pub selecting: bool,
    pub sel_active: bool,
    pub prompt_selecting: bool,
    /// URI armed by pressing on a link; opened on release unless the
    /// press turned into a drag selection.
    pub link_pending: Option<String>,
    pub copied_msg: String,
    pub copied_at: Option<Instant>,

    // splash animation
    pub splash_t: f64,
    pub splash_gen: u64,

    // streaming state
    pub streaming: bool,
    pub remote_working: bool,
    pub pending_saved: bool,
    pub stream_gen: u64,
    pub turn_id: String,
    pub paused: bool,
    /// A mid-stream interruption (pause + resend) is in flight: the
    /// running turn was paused and its `saved` broadcast + SendClosed
    /// must not trigger a transcript reload, because the interruption
    /// message isn't persisted until the new turn finishes.
    pub interrupting: bool,
    pub working_msg: String,
    pub spinner_frame: usize,

    // overlay state
    pub overlay: Option<OverlayKind>,
    pub overlay_q: String,
    pub overlay_q_sel: bool,
    /// Caret position in the overlay search input, in chars from the
    /// start of `overlay_q`. `None` = no caret (view has no editable
    /// input); only the /fork fullscreen view drives it today. Editing
    /// treats `None` as end-of-query.
    pub overlay_q_cursor: Option<usize>,
    pub overlay_sel: usize,
    pub overlay_scroll: usize,
    pub overlay_entries: Vec<atom_core::providers::providers::ModelEntry>,
    pub overlay_sessions: Vec<SessionInfo>,
    pub overlay_stats: Option<StatsReport>,
    /// /profile: latest snapshot returned by Effect::FetchProfile.
    /// None until the first fetch completes — the overlay shows a
    /// spinner during that window, same as /stats.
    pub overlay_profile: Option<crate::profile::ProfileReport>,
    pub stats_days: i64,
    pub overlay_providers: Vec<ProviderListEntry>,
    pub overlay_auth_id: String,
    pub overlay_auth_type: String,
    pub picker_settings: crate::settings::PickerSettings,
    pub atom_config: atom_core::config::AtomConfig,
    pub model_picker_purpose: overlays::ModelPickerPurpose,
    pub settings_onboarding: bool,
    pub pending_model_provider: String,

    /// /fork overlay: user messages from the source session, used to
    /// build picker rows. The SessionLatest sentinel row is computed on
    /// the fly from `self.session`. Empty until the user opens `/fork`
    /// and the source loads via `Effect::LoadForkSource`.
    pub overlay_fork_user_messages: Vec<ForkUserMessage>,
    /// /fork overlay: id of the session being forked (always equal to
    /// `self.session.id` while the overlay is open, but cached so we
    /// don't have to remember which entry was loaded if the user
    /// navigates around before confirming).
    pub overlay_fork_source: String,

    /// Wall-clock instant from `RunOptions::started_at`. The /profile
    /// overlay uses this to render "client started: HH:MM:SS" — the
    /// actual launch time, not a duration. `started_instant` is the
    /// matching monotonic instant so uptime math is skew-proof.
    pub started_at: Option<SystemTime>,
    pub started_instant: Option<Instant>,
    /// Wall-clock when the TUI became ready to accept input (after
    /// setup_terminal, before the first frame). Set once by
    /// `event_loop`'s caller via `set_ready_at`. The /profile overlay
    /// reports `ready_at - started_at` as "client startup" — the
    /// hillclimb metric — and keeps using `started_at` for the live
    /// "client uptime" ticker.
    pub ready_at: Option<SystemTime>,
    /// Server PID from `RunOptions::server_pid`. /profile feeds it to
    /// `ps` for the server section; None means "server not reachable".
    pub server_pid: Option<i32>,

    // terminal dimensions
    pub width: u16,
    pub height: u16,

    // transient error message
    pub err_msg: String,

    // --hot dev loop state path (None when off)
    pub hot_state_path: Option<std::path::PathBuf>,

    // slash-command menu
    pub menu_visible: bool,
    pub menu_sel: usize,
    /// true when the menu was opened via Ctrl+P (prompt untouched); the
    /// menu then shows all commands regardless of the prompt text.
    pub menu_virtual: bool,
    pub slash_commands: Vec<DynamicCommand>,

    // /manage menu: child subagents
    pub manage_visible: bool,
    pub manage_agents: Vec<SessionInfo>,
    pub manage_sel: usize,
    pub manage_sticky: HashMap<String, bool>,
    pub manage_restore_from: String,

    // /mcps and /skills footer menus
    pub picker_kind: PickerKind,
    pub picker_items: Vec<PickerItem>,
    pub picker_sel: usize,
    pub last_picker_insert: String,

    // /context footer menu
    pub context_visible: bool,
    pub context_rows: Vec<ContextRow>,
    pub context_sel: usize,

    // /reasoning footer menu
    pub reasoning_visible: bool,
    pub reasoning_sel: usize,

    // @ file-mention menu
    pub at_menu_visible: bool,
    pub at_menu_items: Vec<String>,
    pub at_menu_sel: usize,
    pub at_menu_query: String,

    // pasted images attached to the pending prompt
    pub pending: Vec<PendingImage>,
    pub preview_dirty: bool,

    // sandbox approval gate
    pub approval: Option<ApprovalPrompt>,

    pub quitting: bool,

    // --output-test canned transcript mode
    pub test_mode: bool,
    pub test_scene: i32,

    pub input: Prompt,

    pub cwd: String,

    // shell mode (! prefix): typed input runs in the shell instead of
    // going to the model; `cd` moves the app (and the session) with it.
    pub shell_mode: bool,
    pub shell_running: bool,
    /// Kill switch for the running shell command, armed by the spawned
    /// task; Ctrl+C sends on it to abort the child process.
    pub shell_kill: Option<tokio::sync::oneshot::Sender<()>>,
}

impl App {
    pub fn new(opts: RunOptions) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let slash_commands = overlays::discover_commands(&cwd);
        let mut m = App {
            providers: opts.providers,
            sel_provider: opts.sel_provider,
            sel_model: opts.sel_model,
            session: opts.session,
            thinking_levels: Vec::new(),
            thinking_idx: 0,
            thinking_pref: String::new(),
            show_reasoning: true,
            blocks: Vec::new(),
            content_lines: Vec::new(),
            link_lines: Vec::new(),
            block_start: Vec::new(),
            content_width: 0,
            following: true,
            scroll_y: 0,
            scrollbar_dragging: false,
            viewport_dirty: true,
            sel_anchor: None,
            sel_end: None,
            selecting: false,
            sel_active: false,
            prompt_selecting: false,
            link_pending: None,
            copied_msg: String::new(),
            copied_at: None,
            splash_t: 0.0,
            splash_gen: 0,
            streaming: false,
            remote_working: false,
            pending_saved: false,
            stream_gen: 0,
            turn_id: String::new(),
            paused: false,
            interrupting: false,
            working_msg: String::new(),
            spinner_frame: 0,
            overlay: None,
            overlay_q: String::new(),
            overlay_q_sel: false,
            overlay_q_cursor: None,
            overlay_sel: 0,
            overlay_scroll: 0,
            overlay_entries: Vec::new(),
            overlay_sessions: Vec::new(),
            overlay_stats: None,
            overlay_profile: None,
            stats_days: 0,
            overlay_providers: Vec::new(),
            overlay_auth_id: String::new(),
            overlay_auth_type: String::new(),
            picker_settings: crate::settings::load(),
            atom_config: atom_core::config::load(),
            model_picker_purpose: overlays::ModelPickerPurpose::Chat,
            settings_onboarding: false,
            pending_model_provider: String::new(),
            overlay_fork_user_messages: Vec::new(),
            overlay_fork_source: String::new(),
            started_at: opts.started_at,
            started_instant: opts.started_instant,
            // Set by `set_ready_at` once the TUI is in raw mode and
            // about to draw its first frame. None until then.
            ready_at: None,
            server_pid: opts.server_pid,
            width: 80,
            height: 24,
            err_msg: String::new(),
            hot_state_path: opts.hot_state_path,
            menu_visible: false,
            menu_sel: 0,
            menu_virtual: false,
            slash_commands,
            manage_visible: false,
            manage_agents: Vec::new(),
            manage_sel: 0,
            manage_sticky: HashMap::new(),
            manage_restore_from: String::new(),
            picker_kind: PickerKind::None,
            picker_items: Vec::new(),
            picker_sel: 0,
            last_picker_insert: String::new(),
            context_visible: false,
            context_rows: Vec::new(),
            context_sel: 0,
            reasoning_visible: false,
            reasoning_sel: 0,
            at_menu_visible: false,
            at_menu_items: Vec::new(),
            at_menu_sel: 0,
            at_menu_query: String::new(),
            pending: Vec::new(),
            preview_dirty: false,
            approval: None,
            quitting: false,
            test_mode: false,
            test_scene: -1,
            input: Prompt::new(),
            cwd,
            shell_mode: false,
            shell_running: false,
            shell_kill: None,
        };
        m.refresh_thinking_levels();
        m.apply_thinking(&m.session.thinking.clone());
        // Apply the persisted theme after any dev hot-theme load so the
        // user's selection wins in normal runs (hot reload only runs in
        // --hot mode, which layers on top of this afterwards).
        if let Some(theme) = m.atom_config.theme.clone() {
            if let Err(error) = atom_core::render::colors::apply_theme(&theme) {
                m.err_msg = format!("theme: {error}");
                m.atom_config.theme = None;
            }
        }
        // If no model was found, auto-open the provider selector.
        if m.sel_model.is_empty() && m.session.id.is_empty() {
            m.open_overlay(OverlayKind::Providers);
            m.working_msg = "loading providers...".into();
        } else if !m.atom_config.setup_complete() {
            m.open_overlay(OverlayKind::Settings);
            m.settings_onboarding = true;
            m.overlay_sel = 0;
        }
        m
    }

    /// Test constructor: never persists defaults (test_mode guards).
    pub fn new_test(width: u16, height: u16) -> Self {
        let mut app = App::new(RunOptions {
            providers: Vec::new(),
            sel_provider: Provider::default(),
            sel_model: String::new(),
            session: empty_session_info(),
            hot_state_path: None,
            started_at: None,
            started_instant: None,
            server_pid: None,
        });
        app.width = width;
        app.height = height;
        app.test_mode = true; // never touch last-model.json from tests
        app.picker_settings = crate::settings::PickerSettings::default();
        app.atom_config = atom_core::config::AtomConfig::default();
        app.settings_onboarding = false;
        app.overlay = None;
        app.working_msg.clear();
        app
    }

    // -- layout helpers ----------------------------------------------------

    pub fn inner_width(&self) -> usize {
        // left pad (TUI_HPAD) + content + right pad (1) + scrollbar (SCROLLBAR_WIDTH)
        (self.width as usize)
            .saturating_sub(2 * TUI_HPAD + SCROLLBAR_WIDTH)
            .max(1)
    }

    pub fn input_width(&self) -> usize {
        self.inner_width().saturating_sub(2 * PROMPT_PAD).max(1)
    }

    /// Rows reserved inside the prompt box for image previews.
    pub fn preview_row_count(&self) -> usize {
        crate::preview::preview_row_count(self)
    }

    /// Read-only transcript view: subagent sessions accept no user input.
    pub fn read_only_view(&self) -> bool {
        !self.session.parent_id.is_empty()
    }

    /// Rows the prompt card occupies (padding + input + previews). Zero
    /// in read-only subagent views, where the prompt is hidden entirely.
    pub fn prompt_height(&self) -> usize {
        if self.read_only_view() {
            0
        } else {
            2 * PROMPT_PAD + self.input_height() + self.preview_row_count()
        }
    }

    /// inputHeight: wrapped prompt rows clamped to INPUT_MAX_HEIGHT or
    /// what fits under the terminal chrome.
    pub fn input_height(&self) -> usize {
        let lines = self.input.content_lines(self.input_width());
        let status_rows = crate::statusbar::status_bar_rows(self);
        let max = (self.height as usize)
            .saturating_sub(
                1 + status_rows + STATUS_FOOTER_ROWS + 2 * PROMPT_PAD + self.preview_row_count(),
            )
            .clamp(1, INPUT_MAX_HEIGHT);
        lines.min(max).max(1)
    }

    /// promptNavigable: up/down move the cursor when the field wraps.
    pub fn prompt_navigable(&self) -> bool {
        self.input.content_lines(self.input_width()) > 1
    }

    // -- thinking levels ---------------------------------------------------

    pub fn refresh_thinking_levels(&mut self) {
        self.thinking_levels =
            modelsdev::reasoning_levels_for(&self.sel_provider.name, &self.sel_model)
                .unwrap_or_default();
        if !self.thinking_levels.is_empty() && self.thinking_idx >= self.thinking_levels.len() {
            self.thinking_idx = modelsdev::default_thinking_index(&self.thinking_levels);
        }
    }

    pub fn thinking_level(&self) -> String {
        if self.thinking_levels.is_empty() {
            return String::new();
        }
        if self.thinking_idx >= self.thinking_levels.len() {
            return self.thinking_levels[modelsdev::default_thinking_index(&self.thinking_levels)]
                .clone();
        }
        self.thinking_levels[self.thinking_idx].clone()
    }

    pub fn cycle_thinking(&mut self) {
        if self.thinking_levels.is_empty() {
            return;
        }
        self.thinking_idx = (self.thinking_idx + 1) % self.thinking_levels.len();
        self.thinking_pref = self.thinking_levels[self.thinking_idx].clone();
    }

    fn thinking_index_of(levels: &[String], saved: &str) -> i32 {
        if saved.is_empty() {
            return -1;
        }
        levels
            .iter()
            .position(|l| l == saved)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    pub fn apply_thinking(&mut self, saved: &str) {
        if !saved.is_empty() {
            self.thinking_pref = saved.to_string();
        }
        if self.thinking_levels.is_empty() {
            self.thinking_idx = 0;
            return;
        }
        let idx = Self::thinking_index_of(&self.thinking_levels, saved);
        if idx >= 0 {
            self.thinking_idx = idx as usize;
            self.thinking_pref = saved.to_string();
            return;
        }
        if saved.is_empty() {
            if let Some(t) = load_last_model_thinking() {
                let idx = Self::thinking_index_of(&self.thinking_levels, &t);
                if idx >= 0 {
                    self.thinking_idx = idx as usize;
                    self.thinking_pref = t;
                    return;
                }
            }
        }
        self.thinking_idx = modelsdev::default_thinking_index(&self.thinking_levels);
        if self.thinking_pref.is_empty() {
            self.thinking_pref = self.thinking_levels[self.thinking_idx].clone();
        }
    }

    pub fn persist_defaults(&self) {
        if self.sel_model.is_empty() || self.test_mode {
            // --output-test uses a fake model; never save it as default.
            return;
        }
        let mut thinking = self.thinking_pref.clone();
        if thinking.is_empty() {
            thinking = self.thinking_level();
        }
        if thinking.is_empty() {
            thinking = self.session.thinking.clone();
        }
        save_last_model_state(&self.sel_provider.name, &self.sel_model, &thinking);
    }

    pub fn commit_thinking(&mut self) -> Vec<Effect> {
        let mut level = self.thinking_level();
        if level.is_empty() {
            level = self.thinking_pref.clone();
        }
        if !level.is_empty() {
            self.thinking_pref = level.clone();
            self.session.thinking = level;
        }
        self.persist_defaults();
        if !self.session.id.is_empty() && !self.session.thinking.is_empty() {
            vec![Effect::PatchSessionThinking]
        } else {
            Vec::new()
        }
    }

    // -- splash ------------------------------------------------------------

    pub fn splash_visible(&self) -> bool {
        self.blocks.is_empty() && self.overlay.is_none()
    }

    /// startSplash begins (or restarts) the animation tick chain.
    pub fn start_splash(&mut self) -> bool {
        if !self.splash_visible() {
            return false;
        }
        self.splash_gen += 1;
        true
    }

    // -- viewport cache ------------------------------------------------------

    /// Drops caches the spinner invalidates: collapsed reasoning labels,
    /// active compaction rows, running tool headers.
    pub fn invalidate_live_blocks(&mut self) {
        for b in self.blocks.iter_mut() {
            match b.kind {
                BlockKind::Reasoning => {
                    if b.active {
                        b.lines = None;
                        self.viewport_dirty = true;
                    }
                }
                BlockKind::Compaction => {
                    if b.active {
                        b.lines = None;
                        self.viewport_dirty = true;
                    }
                }
                BlockKind::Tool if !b.tool_done => {
                    b.lines = None;
                    self.viewport_dirty = true;
                }
                _ => {}
            }
        }
    }

    pub fn invalidate_all_blocks(&mut self) {
        for block in &mut self.blocks {
            block.lines = None;
        }
        self.viewport_dirty = true;
    }

    pub fn refresh_viewport(&mut self) {
        let mut width = self.inner_width();
        if width < 10 {
            width = 10;
        }
        if width != self.content_width {
            for b in self.blocks.iter_mut() {
                b.lines = None;
            }
            self.content_width = width;
            self.rebuild_content_from(0, width);
        } else if self.viewport_dirty || self.block_start.len() != self.blocks.len() {
            let first = self
                .blocks
                .iter()
                .position(|b| !b.lines_valid(width, self.show_reasoning));
            match first {
                Some(i) => self.rebuild_content_from(i, width),
                None => {
                    if self.block_start.len() != self.blocks.len() {
                        self.rebuild_content_from(0, width);
                    }
                }
            }
        }
        self.viewport_dirty = false;
        let max_scroll = self
            .content_lines
            .len()
            .saturating_sub(self.content_viewport_height());
        if self.following {
            self.scroll_y = max_scroll;
        } else {
            self.scroll_y = self.scroll_y.min(max_scroll);
        }
    }

    pub fn rebuild_content_from(&mut self, idx: usize, width: usize) {
        let frame = MINIDOT_FRAMES[self.spinner_frame % MINIDOT_FRAMES.len()].to_string();

        // Re-render any blocks whose cached lines are stale.
        for i in idx..self.blocks.len() {
            let show_r = self.show_reasoning;
            let b = &mut self.blocks[i];
            if !b.lines_valid(width, show_r) {
                let rendered = blocks::render_block_linked(b, width, show_r, &frame, &self.cwd);
                let lines = rendered.lines.into_iter().map(Arc::new).collect();
                b.lines = Some(lines);
                b.line_links = rendered.links;
                b.line_width = width;
                b.line_show_r = show_r;
                b.line_expanded = b.expanded;
            }
        }

        // Incremental content_lines assembly: truncate back to `idx` and
        // rebuild only the tail.  For the common streaming case (only the
        // last block changed) this avoids re-cloning thousands of Arc lines.
        if idx > 0 && idx <= self.block_start.len() {
            // block_start[idx] is where block idx's content begins.
            // The separator line between block idx-1 and block idx (if any)
            // sits one position before that, so we truncate to the end of
            // block idx-1's content (i.e. where block idx's separator would
            // start).  We detect the separator by checking the slot just
            // before block_start[idx].
            let raw = if idx < self.block_start.len() {
                self.block_start[idx]
            } else {
                self.content_lines.len()
            };
            // Remove the separator line that was inserted before block idx.
            let keep_lines = if raw > 0
                && idx < self.block_start.len()
                && self
                    .content_lines
                    .get(raw.wrapping_sub(1))
                    .is_some_and(|l| l.spans.is_empty())
            {
                raw - 1
            } else {
                raw
            };
            self.content_lines.truncate(keep_lines);
            self.link_lines.truncate(keep_lines);
            self.block_start.truncate(idx);
        } else {
            self.content_lines.clear();
            self.link_lines.clear();
            self.block_start.clear();
        }

        let start_block = if self.block_start.is_empty() { 0 } else { idx };
        let mut has_visible_block = !self.content_lines.is_empty();
        for i in start_block..self.blocks.len() {
            let block = &self.blocks[i];
            let lines = block.lines.as_deref().unwrap_or_default();
            let visible = lines
                .iter()
                .any(|line| line.spans.iter().any(|span| !span.content.is_empty()));
            if !visible {
                self.block_start.push(self.content_lines.len());
                self.link_lines.push(Vec::new());
                continue;
            }
            if has_visible_block {
                self.content_lines.push(Arc::new(Line::from("")));
                self.link_lines.push(Vec::new());
            }
            self.block_start.push(self.content_lines.len());
            self.content_lines.extend(lines.iter().cloned());
            self.link_lines.extend(
                (0..lines.len()).map(|i| block.line_links.get(i).cloned().unwrap_or_default()),
            );
            has_visible_block = true;
        }
    }

    /// Conversation rows available before a footer menu is accounted for.
    pub fn base_viewport_height(&self) -> usize {
        let vp = (self.height as usize).saturating_sub(
            crate::statusbar::status_bar_rows(self) + STATUS_FOOTER_ROWS + self.prompt_height(),
        );
        // Reserve only the top viewport padding: the prompt card sits
        // directly below the scrolling region with no empty row in
        // between. Footer menus float over the viewport's bottom rows.
        vp.saturating_sub(VIEWPORT_VPAD).max(1)
    }

    pub fn viewport_height(&self) -> usize {
        // Footer menus overlay the viewport's last rows instead of
        // taking dedicated rows, so the viewport never shrinks.
        self.base_viewport_height()
    }

    /// Number of content rows actually drawn in the message viewport.
    /// The scrollbar and viewport rect keep their full height
    /// (`viewport_height()`); this reserves a small bottom padding row
    /// of blank space between the last content row and the prompt.
    /// An open footer menu floats over the viewport's bottom rows and
    /// does not change the scrollable content height.
    pub fn content_viewport_height(&self) -> usize {
        self.viewport_height()
            .saturating_sub(VIEWPORT_BOTTOM_PAD)
            .max(1)
    }

    pub fn block_index_at_content_line(&self, line: usize) -> i32 {
        if self
            .content_lines
            .get(line)
            .is_none_or(|line| line.spans.is_empty())
        {
            return -1;
        }
        let mut idx: i32 = -1;
        for (i, start) in self.block_start.iter().enumerate() {
            if *start <= line {
                idx = i as i32;
            } else {
                break;
            }
        }
        if idx < 0 || idx as usize >= self.blocks.len() {
            return -1;
        }
        idx
    }

    pub fn block_index_at_viewport_y(&self, y: usize) -> i32 {
        let Some(y) = y.checked_sub(VIEWPORT_VPAD) else {
            return -1;
        };
        if y >= self.content_viewport_height() {
            return -1;
        }
        self.block_index_at_content_line(self.scroll_y + y)
    }

    // -- stream events -----------------------------------------------------

    /// handleStreamEvent processes one NDJSON event from the server.
    pub fn handle_stream_event(&mut self, ev: &StreamEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        match ev.event_type.as_str() {
            "round_start" => {
                // The server is starting a model round (before any token:
                // history rebuild, compaction, context upload). Show the
                // live Thinking indicator immediately instead of leaving
                // the transcript looking finished.
                let extends = matches!(
                    self.blocks.last(),
                    Some(b) if b.kind == BlockKind::Reasoning && b.active
                );
                if !extends {
                    self.blocks.push(Block {
                        kind: BlockKind::Reasoning,
                        expanded: self.show_reasoning,
                        active: true,
                        started_at: Some(Instant::now()),
                        ..Default::default()
                    });
                }
            }
            "tool_pending" => {
                let active = matches!(
                    self.blocks.last(),
                    Some(b) if b.kind == BlockKind::Reasoning && b.active
                );
                if !active {
                    self.blocks.push(Block {
                        kind: BlockKind::Reasoning,
                        expanded: self.show_reasoning,
                        active: true,
                        started_at: Some(Instant::now()),
                        ..Default::default()
                    });
                }
            }
            "content" => {
                if ev.text.is_empty() {
                    return effects;
                }
                // Drop a placeholder that never received reasoning text so
                // pure-content replies don't leave a stray Thinking header.
                self.finalize_reasoning(None);
                self.finalize_compaction();
                if matches!(
                    self.blocks.last().map(|b| b.kind),
                    Some(BlockKind::Assistant)
                ) {
                    let last = self.blocks.last_mut().unwrap();
                    last.text.push_str(&ev.text);
                    last.lines = None;
                    self.viewport_dirty = true;
                } else {
                    self.blocks.push(Block {
                        kind: BlockKind::Assistant,
                        text: ev.text.clone(),
                        ..Default::default()
                    });
                }
            }
            "reasoning" => {
                let extends = matches!(
                    self.blocks.last(),
                    Some(b) if b.kind == BlockKind::Reasoning && b.active
                );
                if extends {
                    let last = self.blocks.last_mut().unwrap();
                    last.text.push_str(&ev.text);
                    last.lines = None;
                    self.viewport_dirty = true;
                } else {
                    self.blocks.push(Block {
                        kind: BlockKind::Reasoning,
                        text: ev.text.clone(),
                        expanded: self.show_reasoning,
                        active: true,
                        started_at: Some(Instant::now()),
                        ..Default::default()
                    });
                }
            }
            "reasoning_end" => {
                self.finalize_reasoning(ev.duration);
            }
            "tool" => {
                self.finalize_reasoning(None);
                let title = blocks::tool_display_name(&ev.name);
                let text = blocks::tool_action(&ev.name, &ev.arguments);
                if let Some(last) = self.blocks.last() {
                    if last.kind == BlockKind::Tool
                        && last.title == title
                        && last.text == text
                        && !last.tool_done
                    {
                        return effects;
                    }
                }
                self.blocks.push(Block {
                    kind: BlockKind::Tool,
                    title,
                    tool_name: ev.name.clone(),
                    text,
                    ..Default::default()
                });
            }
            "tool_result" => {
                blocks::attach_tool_result(&mut self.blocks, &ev.text, "");
                self.viewport_dirty = true;
                if blocks::assign_block_diagram_ids(&mut self.blocks) {
                    self.preview_dirty = true;
                    // Tool results arrive after the visualize card was
                    // rendered. Assigning an id only marks the preview as
                    // dirty; explicitly schedule the kitty transmission so
                    // the reserved placeholder grid is not left blank.
                    effects.push(Effect::PaintPreviews);
                }
                if !self.session.id.is_empty()
                    && !atom_tools::parse_dispatch_session_id(&ev.text).is_empty()
                {
                    effects.push(Effect::ListChildren {
                        id: self.session.id.clone(),
                    });
                }
            }
            "tool_diff" => {
                for b in self.blocks.iter_mut().rev() {
                    if b.kind == BlockKind::Tool && b.diff.is_empty() {
                        b.diff = ev.diff.clone();
                        b.lines = None;
                        self.viewport_dirty = true;
                        break;
                    }
                }
            }
            "compaction" => {
                self.finalize_reasoning(None);
                let active_last = matches!(
                    self.blocks.last(),
                    Some(b) if b.kind == BlockKind::Compaction && b.active
                );
                if active_last {
                    return effects;
                }
                self.blocks.push(Block {
                    kind: BlockKind::Compaction,
                    model: ev.model.clone(),
                    active: true,
                    started_at: Some(Instant::now()),
                    ..Default::default()
                });
            }
            "compaction_end" => {
                if let Some(b) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| b.kind == BlockKind::Compaction && b.active)
                {
                    if !ev.text.is_empty() {
                        b.text = ev.text.clone();
                    }
                    // Older servers omit the model from the start event;
                    // the end event carries it too.
                    if !ev.model.is_empty() {
                        b.model = ev.model.clone();
                    }
                }
                self.finalize_compaction();
            }
            // Truncated thinking; the server is continuing the same turn.
            "nudge" => {
                self.finalize_reasoning(None);
            }
            "error" => {
                self.finalize_reasoning(None);
                self.finalize_compaction();
                self.finalize_tools();
                self.blocks.push(Block {
                    kind: BlockKind::Error,
                    text: ev.message.clone(),
                    ..Default::default()
                });
            }
            "usage" => {
                if let Some(u) = ev.usage.clone() {
                    self.session.usage = Some(u);
                }
            }
            "approval_request" => {
                // `emit` fans the event out on both the /send stream and
                // the session subscription, and sub_event forwards it even
                // while this client's own stream is live (so a subagent's
                // prompt is never dropped). The same id therefore arrives
                // twice; handle it only once or two identical approval
                // cards would be stacked in the transcript.
                if self.approval.as_ref().is_some_and(|p| p.id == ev.id)
                    || self
                        .blocks
                        .iter()
                        .any(|b| b.approval.as_ref().is_some_and(|a| a.id == ev.id))
                {
                    return effects;
                }
                // Render the sandbox approval inline as a tool block with
                // clickable buttons. The block stays active (tool_done=false)
                // until the user responds.
                let sid = if ev.session_id.is_empty() {
                    self.session.id.clone()
                } else {
                    ev.session_id.clone()
                };
                self.approval = Some(ApprovalPrompt {
                    id: ev.id.clone(),
                    command: ev.command.clone(),
                    cwd: ev.cwd.clone(),
                    rule_id: ev.rule_id.clone(),
                    reason: ev.reason.clone(),
                    session_id: sid.clone(),
                    child_title: ev.child_title.clone(),
                    from_subagent: ev.from_subagent,
                });
                let mut inline = Some(blocks::InlineApproval {
                    id: ev.id.clone(),
                    session_id: sid,
                    command: ev.command.clone(),
                    cwd: ev.cwd.clone(),
                    rule_id: ev.rule_id.clone(),
                    reason: ev.reason.clone(),
                    from_subagent: ev.from_subagent,
                    child_title: ev.child_title.clone(),
                    // v2: forward the origin tag (defaults to "self"
                    // when the server didn't set one) and the
                    // accept-all prefix preview for `[a]`.
                    origin: ev.origin.clone(),
                    accept_all_preview: ev.accept_all_preview.clone(),
                });
                // Convert the bash tool block this approval is for in
                // place, when one exists. The server emits the `tool`
                // event just before requesting approval, so the most
                // recent pending bash block with matching command text
                // is the unambiguous target. Reusing it keeps the
                // transcript to one block per tool call: header →
                // sandbox approval card → sandbox result. Without the
                // conversion we'd stack a fresh "Sandbox" block on top
                // of the original "Bash" block, and the upcoming
                // `tool_result` would attach to the new one (leaving
                // the original Bash block as an empty dangling header
                // and any later tool result overwriting the card).
                let mut converted = false;
                for b in self.blocks.iter_mut().rev() {
                    if b.kind == BlockKind::Tool
                        && b.tool_name == "bash"
                        && b.text == ev.command
                        && !b.tool_done
                    {
                        b.title = "Sandbox".to_string();
                        b.approval = inline.take();
                        b.expanded = true;
                        b.lines = None;
                        converted = true;
                        break;
                    }
                }
                if !converted {
                    // No matching bash block — happens only on
                    // reconnect/replay where the `tool` event landed
                    // before the local client subscribed. Fall back to
                    // creating a fresh Sandbox block so the prompt is
                    // never lost.
                    self.blocks.push(Block {
                        kind: BlockKind::Tool,
                        title: "Sandbox".to_string(),
                        tool_name: "sandbox".to_string(),
                        text: ev.command.clone(),
                        approval: inline.take(),
                        expanded: true,
                        ..Default::default()
                    });
                }
                self.viewport_dirty = true;
                self.following = true;
                self.refresh_viewport();
            }
            "paused" => {
                self.finalize_reasoning(None);
                self.finalize_compaction();
                self.finalize_tools();
                self.paused = true;
            }
            "done" => {
                self.finalize_reasoning(None);
                self.finalize_tools();
                if !ev.model.is_empty() {
                    if let Some(block) = self
                        .blocks
                        .iter_mut()
                        .rev()
                        .find(|block| block.kind == BlockKind::Assistant)
                    {
                        block.model = ev.model.clone();
                        block.turn_duration = ev.duration;
                        block.lines = None;
                        self.viewport_dirty = true;
                    }
                }
            }
            _ => {}
        }
        effects
    }

    pub fn finalize_reasoning(&mut self, dur: Option<Duration>) {
        let Some(idx) = self
            .blocks
            .iter()
            .rposition(|b| b.kind == BlockKind::Reasoning && b.active)
        else {
            return;
        };
        // A placeholder from round_start that never received reasoning
        // text is removed outright; keeping it would leave a stray
        // "Thinking" header above pure-content replies.
        if self.blocks[idx].text.is_empty() {
            self.blocks.remove(idx);
            return;
        }
        let b = &mut self.blocks[idx];
        b.active = false;
        b.dur = Some(
            dur.unwrap_or_else(|| b.started_at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO)),
        );
        b.lines = None;
        self.viewport_dirty = true;
    }

    pub fn finalize_compaction(&mut self) {
        if let Some(b) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| b.kind == BlockKind::Compaction && b.active)
        {
            b.active = false;
            b.dur = Some(b.started_at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO));
            b.lines = None;
            self.viewport_dirty = true;
        }
    }

    fn finalize_tools(&mut self) {
        for b in &mut self.blocks {
            if b.kind == BlockKind::Tool && !b.tool_done {
                b.tool_done = true;
                b.lines = None;
                self.viewport_dirty = true;
            }
        }
    }

    // -- menus -------------------------------------------------------------

    pub fn set_menu_visible(&mut self, v: bool) {
        self.menu_visible = v;
        if !v {
            self.menu_virtual = false;
        } else {
            self.close_context_menu();
            self.close_reasoning_menu();
        }
    }

    /// Record the wall-clock instant the TUI became ready to accept
    /// input (after `setup_terminal`, before the first frame).
    /// /profile uses this as the end of its startup-time window —
    /// `ready_at - started_at` is the static "ms to load" value that
    /// gets hillclimbed. Called once by `event_loop`'s caller.
    pub fn set_ready_at(&mut self, ready_at: SystemTime) {
        self.ready_at = Some(ready_at);
    }

    /// Effective typed prefix for the slash menu. A Ctrl+P-opened menu
    /// (menu_virtual) matches against "" unless the prompt itself starts
    /// with "/", so the palette shows every command without touching the
    /// prompt text.
    pub fn menu_typed(&self) -> String {
        if self.menu_virtual && !self.input.value.starts_with('/') {
            String::new()
        } else {
            self.input.value.clone()
        }
    }

    pub fn close_picker(&mut self) {
        self.picker_kind = PickerKind::None;
        self.picker_items.clear();
        self.picker_sel = 0;
    }

    pub fn close_context_menu(&mut self) {
        self.context_visible = false;
        self.context_rows.clear();
        self.context_sel = 0;
    }

    pub fn close_reasoning_menu(&mut self) {
        self.reasoning_visible = false;
        self.reasoning_sel = 0;
    }

    pub fn close_at_menu(&mut self) {
        self.at_menu_visible = false;
        self.at_menu_items.clear();
        self.at_menu_sel = 0;
        self.at_menu_query.clear();
    }

    /// Extracts the @-query token at the cursor (the word immediately
    /// after the last `@` before the cursor that has no spaces).
    fn at_query_at_cursor(&self) -> Option<String> {
        let value = &self.input.value;
        let cursor = self.input.cursor.min(value.len());
        let before = &value[..cursor];
        // Find the last `@` before the cursor.
        let at_pos = before.rfind('@')?;
        let after_at = &before[at_pos + 1..];
        // The @-query must not contain spaces (it's a single token).
        if after_at.contains([' ', '\t', '\n']) {
            return None;
        }
        Some(after_at.to_string())
    }

    /// Syncs the @-mention file menu based on the current input.
    fn sync_at_menu(&mut self) {
        let Some(query) = self.at_query_at_cursor() else {
            self.close_at_menu();
            return;
        };
        // Build file list from cwd
        let items = list_project_files(&self.cwd, &query);
        if items.is_empty() {
            self.close_at_menu();
            return;
        }
        self.at_menu_query = query;
        self.at_menu_items = items;
        if self.at_menu_sel >= self.at_menu_items.len() {
            self.at_menu_sel = 0;
        }
        self.at_menu_visible = true;
    }

    /// Selects the highlighted @-menu item: replaces the @query with the
    /// full path.
    pub fn select_at_menu_item(&mut self) -> Vec<Effect> {
        if self.at_menu_sel >= self.at_menu_items.len() {
            self.close_at_menu();
            return Vec::new();
        }
        let selected = self.at_menu_items[self.at_menu_sel].clone();
        // Replace the @query in the input with @selected
        let value = &self.input.value;
        let cursor = self.input.cursor.min(value.len());
        let before = &value[..cursor];
        if let Some(at_pos) = before.rfind('@') {
            let prefix = value[..at_pos].to_string();
            let suffix = value[cursor..].to_string();
            let new_value = format!("{}@{}{}", prefix, selected, suffix);
            let new_cursor = at_pos + 1 + selected.len();
            self.input.set_value(&new_value);
            self.input.cursor = new_cursor;
        }
        self.close_at_menu();
        Vec::new()
    }

    pub fn hide_manage_menu(&mut self) {
        self.manage_visible = false;
        self.close_context_menu();
        self.close_reasoning_menu();
    }

    pub fn dismiss_manage_menu(&mut self) {
        self.manage_visible = false;
        if !self.session.id.is_empty() {
            self.manage_sticky.remove(&self.session.id);
        }
        self.close_context_menu();
        self.close_reasoning_menu();
    }

    pub fn open_manage_menu(&mut self) -> Vec<Effect> {
        if self.session.id.is_empty() {
            return Vec::new();
        }
        self.manage_visible = true;
        self.manage_sel = 0;
        self.manage_sticky.insert(self.session.id.clone(), true);
        self.set_menu_visible(false);
        self.close_picker();
        self.close_context_menu();
        self.close_reasoning_menu();
        vec![Effect::ListChildren {
            id: self.session.id.clone(),
        }]
    }

    // -- input handling ----------------------------------------------------

    /// handleInput processes submitted text: slash commands run locally,
    /// regular text sends to the server.
    pub fn handle_input(&mut self, text: &str) -> Vec<Effect> {
        // Block submit while any pasted image is still being normalized
        // on the blocking pool — sending the prompt with an empty
        // payload would either drop the image or ship a half-built
        // block. The marker is in the prompt already, so the user can
        // see what's waiting; the gate is a transient 100-300 ms.
        if preview::pending_has_unprepared(self) {
            self.err_msg = "preparing image…".to_string();
            return Vec::new();
        }
        if text == "/subagents" {
            if self.input.value.trim() == text {
                self.input.clear();
            }
            self.err_msg.clear();
            return self.open_manage_menu();
        }
        if text == "/context" {
            if self.input.value.trim() == text {
                self.input.clear();
            }
            self.dismiss_manage_menu();
            self.close_picker();
            self.set_menu_visible(false);
            self.err_msg.clear();
            return vec![Effect::FetchContext {
                id: self.session.id.clone(),
            }];
        }
        if text == "/mcps" || text == "/skills" {
            if self.input.value.trim() == text {
                self.input.clear();
            }
            self.dismiss_manage_menu();
            self.close_context_menu();
            self.err_msg.clear();
            let kind = if text == "/mcps" {
                PickerKind::Mcp
            } else {
                PickerKind::Skills
            };
            return self.open_picker(kind);
        }

        let passthrough = !text.starts_with('/')
            || overlays::is_catalog_prompt(text, &self.slash_commands)
            || overlays::looks_like_file_path(text);
        if passthrough && (self.streaming || self.remote_working) {
            // Mid-stream submit: the interruption rides in the effect.
            // Pause the running turn via the server first, then dial the
            // new /send stream; nothing is stored in the App. The prompt
            // is cleared right away so the next draft can be typed while
            // the pause/send happens in the background.
            let imgs: Vec<ImageData> = self.pending.iter().map(|p| p.img.clone()).collect();
            let imgs_meta = std::mem::take(&mut self.pending);
            self.preview_dirty = true;
            self.input.clear();
            self.last_picker_insert.clear();
            self.dismiss_manage_menu();
            self.close_picker();
            self.close_context_menu();
            self.close_at_menu();
            self.err_msg.clear();
            self.paused = true;
            self.interrupting = true;
            self.blocks.push(Block {
                kind: BlockKind::User,
                text: text.to_string(),
                images: imgs_meta,
                ..Default::default()
            });
            self.streaming = true;
            let pause_turn_id = std::mem::take(&mut self.turn_id);
            self.turn_id = new_turn_id();
            self.following = true;
            let req = SendRequest {
                session_id: self.session.id.clone(),
                turn_id: self.turn_id.clone(),
                message: text.to_string(),
                thinking: self.thinking_level(),
                images: imgs,
                key: self.sel_provider.key.clone(),
                base_url: self.sel_provider.base_url.clone(),
                reasoning_field: self.sel_provider.reasoning_field.clone(),
                compact: false,
                compact_instructions: String::new(),
            };
            return vec![
                Effect::InterruptTurn {
                    pause_turn_id,
                    req: Box::new(req),
                },
                Effect::PaintPreviews,
            ];
        }

        self.input.clear();
        self.last_picker_insert.clear();
        self.interrupting = false;
        self.dismiss_manage_menu();
        self.close_picker();
        self.close_context_menu();
        self.close_at_menu();
        self.err_msg.clear();

        if passthrough {
            let imgs: Vec<ImageData> = self.pending.iter().map(|p| p.img.clone()).collect();
            let pending_images = std::mem::take(&mut self.pending);
            self.preview_dirty = true;
            self.blocks.push(Block {
                kind: BlockKind::User,
                text: text.to_string(),
                images: pending_images,
                ..Default::default()
            });
            self.streaming = true;
            self.paused = false;
            self.turn_id = new_turn_id();
            self.following = true;
            let req = SendRequest {
                session_id: self.session.id.clone(),
                turn_id: self.turn_id.clone(),
                message: text.to_string(),
                thinking: self.thinking_level(),
                images: imgs,
                key: self.sel_provider.key.clone(),
                base_url: self.sel_provider.base_url.clone(),
                reasoning_field: self.sel_provider.reasoning_field.clone(),
                compact: false,
                compact_instructions: String::new(),
            };
            return vec![Effect::SendTurn(Box::new(req)), Effect::PaintPreviews];
        }

        if text == "/compact" || text.starts_with("/compact ") {
            if self.session.id.is_empty() {
                self.err_msg = "no session to compact".into();
                return Vec::new();
            }
            if self.read_only_view() {
                self.err_msg = "subagent sessions are managed by their parent".into();
                return Vec::new();
            }
            let extra = text
                .strip_prefix("/compact")
                .unwrap_or("")
                .trim()
                .to_string();
            self.paused = false;
            self.following = true;
            if self.streaming {
                return vec![Effect::Compact {
                    instructions: extra,
                }];
            }
            self.streaming = true;
            self.turn_id = new_turn_id();
            let req = SendRequest {
                session_id: self.session.id.clone(),
                turn_id: self.turn_id.clone(),
                message: String::new(),
                thinking: self.thinking_level(),
                images: Vec::new(),
                key: self.sel_provider.key.clone(),
                base_url: self.sel_provider.base_url.clone(),
                reasoning_field: self.sel_provider.reasoning_field.clone(),
                compact: true,
                compact_instructions: extra,
            };
            return vec![Effect::SendTurn(Box::new(req))];
        }

        if text == "/stats" || text.starts_with("/stats ") {
            let days = overlays::parse_stats_days(text);
            self.open_overlay(OverlayKind::Stats);
            self.overlay_q.clear();
            self.overlay_sel = 0;
            self.overlay_stats = None;
            self.stats_days = days;
            self.working_msg = "loading stats...".into();
            return vec![Effect::FetchStats { days }];
        }

        match text {
            "/quit" | "/exit" => {
                let mut fx = self.commit_thinking();
                self.quitting = true;
                fx.push(Effect::Quit);
                fx
            }
            "/settings" => {
                self.open_overlay(OverlayKind::Settings);
                self.overlay_sel = 0;
                self.overlay_q.clear();
                self.settings_onboarding = false;
                Vec::new()
            }
            "/theme" => {
                let rows = overlays::theme_rows();
                self.open_overlay(OverlayKind::Theme);
                self.overlay_sel = rows
                    .iter()
                    .position(|entry| entry.id == atom_core::render::colors::active_theme_name())
                    .unwrap_or(0);
                self.overlay_q.clear();
                Vec::new()
            }
            "/model" => {
                self.model_picker_purpose = overlays::ModelPickerPurpose::Chat;
                self.open_overlay(OverlayKind::Model);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.pending_model_provider.clear();
                self.working_msg = "loading models...".into();
                vec![Effect::FetchModels]
            }
            "/reasoning" => {
                self.reasoning_visible = true;
                self.reasoning_sel = self
                    .thinking_idx
                    .min(self.thinking_levels.len().saturating_sub(1));
                Vec::new()
            }
            "/providers" => {
                self.open_overlay(OverlayKind::Providers);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_auth_id.clear();
                self.overlay_auth_type.clear();
                self.overlay_providers = providers::list_addable_providers();
                self.working_msg.clear();
                Vec::new()
            }
            "/new" => {
                let provider = self.sel_provider.name.clone();
                let model = self.sel_model.clone();
                let cwd = self.cwd.clone();
                let thinking = self.thinking_level();
                vec![Effect::CreateSession {
                    provider,
                    model,
                    cwd,
                    thinking,
                }]
            }
            "/sessions" | "/resume" => {
                self.open_overlay(OverlayKind::Session);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.working_msg = "loading sessions...".into();
                vec![Effect::FetchSessions]
            }
            "/fork" => {
                if self.streaming || self.remote_working {
                    self.err_msg = "wait for the current turn to finish before forking".into();
                    return Vec::new();
                }
                if self.session.id.is_empty() {
                    self.err_msg = "no session to fork".into();
                    return Vec::new();
                }
                if self.read_only_view() {
                    self.err_msg = "cannot fork a subagent session".into();
                    return Vec::new();
                }
                self.open_overlay(OverlayKind::Fork);
                self.overlay_q.clear();
                self.overlay_q_sel = false;
                self.overlay_q_cursor = Some(0);
                self.overlay_sel = overlays::first_fork_row();
                self.overlay_scroll = 0;
                self.overlay_fork_user_messages.clear();
                self.overlay_fork_source = self.session.id.clone();
                self.working_msg = "loading session...".into();
                vec![Effect::LoadForkSource {
                    id: self.session.id.clone(),
                }]
            }
            "/thinking" => {
                self.show_reasoning = !self.show_reasoning;
                for block in &mut self.blocks {
                    if block.kind == BlockKind::Reasoning {
                        block.expanded = self.show_reasoning;
                        block.lines = None;
                        self.viewport_dirty = true;
                    }
                }
                self.refresh_viewport();
                Vec::new()
            }
            "/profile" if atom_core::build::is_dev() => {
                // Dev-only diagnostic overlay: startup time + CPU/RSS
                // for the client and the running `atoms` server. Hidden
                // in release builds — the catalog also omits it, so a
                // user can't reach this arm without typing the literal
                // command, but we double-check here in case a custom
                // hook or scripted client tries to.
                self.open_overlay(OverlayKind::Profile);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.overlay_profile = None;
                self.working_msg = "sampling processes...".into();
                vec![Effect::FetchProfile {
                    client_pid: std::process::id() as i32,
                    server_pid: self.server_pid,
                }]
            }
            "/profile" => {
                self.err_msg = "/profile is dev-only".into();
                Vec::new()
            }
            other => {
                self.err_msg = format!("unknown command: {other}");
                Vec::new()
            }
        }
    }

    /// Open a fullscreen overlay. Query overlays get their native
    /// search caret initialized (hidden by default elsewhere), so the
    /// terminal cursor only shows on searchable pickers.
    pub fn open_overlay(&mut self, kind: OverlayKind) {
        self.overlay = Some(kind);
        self.overlay_q_sel = false;
        self.overlay_scroll = 0;
        self.overlay_q_cursor = if overlays::overlay_has_query(Some(kind)) {
            Some(0)
        } else {
            None
        };
    }

    pub fn open_picker(&mut self, kind: PickerKind) -> Vec<Effect> {
        self.dismiss_manage_menu();
        self.set_menu_visible(false);
        self.close_context_menu();
        self.close_reasoning_menu();
        self.picker_kind = kind;
        self.picker_sel = 0;
        self.picker_items = match kind {
            PickerKind::Mcp => picker_items(&self.slash_commands, "mcp"),
            PickerKind::Skills => picker_items(&self.slash_commands, "skill"),
            PickerKind::None => {
                self.close_picker();
                return Vec::new();
            }
        };
        Vec::new()
    }

    pub fn select_picker_item(&mut self, close: bool) -> Vec<Effect> {
        if self.picker_sel >= self.picker_items.len() {
            self.close_picker();
            return Vec::new();
        }
        // Snapshot the row fields we care about so the immutable borrow
        // of `self.picker_items` ends before we touch `self` mutably
        // (e.g. when starting the OAuth flow).
        let item = &self.picker_items[self.picker_sel];
        let title = item.title.clone();
        let meta = item.meta.clone();
        let mcp_server = item.mcp_server.clone();
        // For MCP rows that need OAuth ("auth required" / "auth expired"
        // — see atom_tools::mcp_oauth::mcp_auth_display), Enter on the
        // server should kick off the browser sign-in instead of just
        // inserting "/name" into the prompt. Authenticated MCPs fall
        // through and behave like skills (insert the slash text).
        if self.picker_kind == PickerKind::Mcp {
            if let Some(server) = mcp_server.as_deref() {
                let needs_auth = matches!(meta.as_str(), "auth required" | "auth expired");
                if needs_auth {
                    if let Some(fx) = self.start_mcp_oauth(server) {
                        if close {
                            self.close_picker();
                        }
                        return fx;
                    }
                }
            }
        }
        let name = title.trim().to_string();
        if !name.is_empty() {
            self.apply_picker_insert(&format!("/{name}"));
        }
        if close {
            self.close_picker();
        }
        Vec::new()
    }

    /// Build a StartMcpOAuth effect for `server`, looking up its URL
    /// and static client id from the cwd's MCP configs. Returns None
    /// when the server isn't configured or doesn't opt into OAuth so
    /// callers can fall back to inserting the slash text.
    fn start_mcp_oauth(&mut self, server: &str) -> Option<Vec<Effect>> {
        let cfgs = atom_tools::mcp::load_mcp_configs(&self.cwd);
        let cfg = cfgs.get(server)?;
        if !cfg.auth.eq_ignore_ascii_case("oauth") {
            return None;
        }
        self.working_msg = format!("waiting for {server} sign-in in the browser...");
        Some(vec![Effect::StartMcpOAuth {
            server: server.to_string(),
            url: cfg.url.clone(),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            token_endpoint_auth_method: cfg.token_endpoint_auth_method.clone(),
        }])
    }

    pub fn apply_picker_insert(&mut self, text: &str) {
        let cur = self.input.value.clone();
        let last = self.last_picker_insert.clone();
        if !last.is_empty() && cur == last {
            self.input.set_value(text);
        } else if !last.is_empty() && cur.ends_with(&last) {
            let prefix = cur[..cur.len() - last.len()]
                .trim_end_matches([' ', '\t'])
                .to_string();
            self.input.set_value(&join_prompt(&prefix, text));
        } else if overlays::is_slash_query(&cur) {
            self.input.set_value(text);
        } else {
            self.input.set_value(&join_prompt(&cur, text));
        }
        self.last_picker_insert = text.to_string();
    }

    /// selectSlashMatch completes the highlighted row; built-ins run now.
    pub fn select_slash_match(&mut self) -> Vec<Effect> {
        let typed = self.menu_typed();
        let matches = overlays::match_commands(&typed, &self.slash_commands);
        if matches.is_empty() || self.menu_sel >= matches.len() {
            self.set_menu_visible(false);
            return Vec::new();
        }
        let sel = matches[self.menu_sel].clone();
        self.set_menu_visible(false);
        if sel.catalog_insert() {
            self.apply_picker_insert(&sel.name);
            return Vec::new();
        }
        self.input.set_value(&sel.name);
        let text = sel.name.trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        self.handle_input(&text)
    }

    pub fn select_manage_agent(&mut self) -> Vec<Effect> {
        if self.manage_sel >= self.manage_agents.len() {
            return Vec::new();
        }
        let id = self.manage_agents[self.manage_sel].id.clone();
        self.hide_manage_menu();
        vec![Effect::LoadSession { id }]
    }

    // -- selection ---------------------------------------------------------

    pub fn clear_selection(&mut self) {
        self.selecting = false;
        self.sel_active = false;
        self.sel_anchor = None;
        self.sel_end = None;
    }

    pub fn selection_range(&self) -> ((usize, usize), (usize, usize)) {
        let a = self.sel_anchor.unwrap_or((0, 0));
        let b = self.sel_end.unwrap_or(a);
        if a.0 > b.0 || (a.0 == b.0 && a.1 > b.1) {
            (b, a)
        } else {
            (a, b)
        }
    }

    pub fn selected_text(&self) -> String {
        if !self.sel_active || self.content_lines.is_empty() {
            return String::new();
        }
        let (a, b) = self.selection_range();
        let mut parts = Vec::new();
        for i in a.0..=b.0 {
            let Some(line) = self.content_lines.get(i) else {
                break;
            };
            let w = ansi_line_width(line);
            let c0 = if i == a.0 { a.1 } else { 0 }.min(w);
            let c1 = (if i == b.0 { b.1 + 1 } else { w }).min(w);
            if c1 > c0 {
                parts.push(crate::ansi::cut_line_range(line, c0, c1));
            }
        }
        parts.join("\n")
    }

    pub fn copy_selection(&mut self) -> Vec<Effect> {
        let text = self.selected_text();
        if text.is_empty() {
            return Vec::new();
        }
        self.copied_msg = format!("Copied {} chars", text.chars().count());
        self.copied_at = Some(Instant::now());
        self.clear_selection();
        vec![Effect::CopyToClipboard { text }]
    }

    // -- AppMsg entry ------------------------------------------------------

    /// handle_msg is Update(): feed one runtime message, get effects.
    pub fn handle_msg(&mut self, msg: AppMsg) -> Vec<Effect> {
        match msg {
            AppMsg::Key(k) => self.key(k),
            AppMsg::Mouse(m) => self.mouse(m),
            AppMsg::Resize(w, h) => self.resize(w, h),
            AppMsg::Paste(content) => self.paste(content),
            AppMsg::ModelsLoaded(entries) => {
                if self.overlay != Some(OverlayKind::Model) {
                    return Vec::new();
                }
                self.overlay_entries = entries;
                self.overlay_sel = overlays::first_model_row(self);
                self.overlay_scroll = 0;
                overlays::sync_model_scroll(self);
                self.pending_model_provider.clear();
                self.working_msg.clear();
                Vec::new()
            }
            AppMsg::SessionsLoaded(sessions) => {
                if self.overlay != Some(OverlayKind::Session) {
                    return Vec::new();
                }
                self.overlay_sessions = sessions;
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.overlay_sel = overlays::first_session_row(self);
                overlays::sync_session_scroll(self);
                self.working_msg.clear();
                Vec::new()
            }
            AppMsg::ChildrenLoaded { id, agents } => {
                // Child-list requests race session switches. Never install a
                // response belonging to the session we just left.
                if id != self.session.id {
                    return Vec::new();
                }
                self.manage_agents = agents;
                if self.manage_agents.is_empty() {
                    if !self
                        .manage_sticky
                        .get(&self.session.id)
                        .copied()
                        .unwrap_or(false)
                    {
                        self.manage_visible = false;
                    }
                } else if self
                    .manage_sticky
                    .get(&self.session.id)
                    .copied()
                    .unwrap_or(false)
                {
                    self.manage_visible = true;
                    let from = self.manage_restore_from.clone();
                    if !from.is_empty() {
                        if let Some(i) = self.manage_agents.iter().position(|a| a.id == from) {
                            self.manage_sel = i;
                        }
                    }
                }
                self.manage_restore_from.clear();
                if self.manage_sel >= self.manage_agents.len() {
                    self.manage_sel = 0;
                }
                Vec::new()
            }
            AppMsg::ContextLoaded(rows) => {
                self.dismiss_manage_menu();
                self.close_picker();
                self.set_menu_visible(false);
                self.close_reasoning_menu();
                self.context_rows = rows;
                self.context_visible = true;
                self.context_sel = 0;
                self.err_msg.clear();
                Vec::new()
            }
            AppMsg::StatsLoaded(report) => {
                match report {
                    Ok(r) => {
                        self.overlay_stats = Some(*r);
                        self.overlay_sel = 0;
                        self.working_msg.clear();
                    }
                    Err(e) => {
                        self.err_msg = e;
                        self.working_msg.clear();
                        self.overlay = None;
                    }
                }
                Vec::new()
            }
            AppMsg::ProfileLoaded(report) => {
                match report {
                    Ok(r) => {
                        self.overlay_profile = Some(*r);
                        self.overlay_sel = 0;
                        self.working_msg.clear();
                    }
                    Err(e) => {
                        self.err_msg = e;
                        self.working_msg.clear();
                        self.overlay = None;
                    }
                }
                Vec::new()
            }
            AppMsg::Errored(e) => {
                self.err_msg = e;
                self.working_msg.clear();
                self.overlay = None;
                self.dismiss_manage_menu();
                self.close_picker();
                self.close_context_menu();
                Vec::new()
            }
            AppMsg::CompactDone(result) => {
                self.working_msg.clear();
                if let Err(e) = result {
                    self.err_msg = e;
                }
                Vec::new()
            }
            AppMsg::ModelsDevReady => self.after_models_dev_ready(),
            AppMsg::ProvidersRebuilt(providers) => self.providers_rebuilt(providers),
            AppMsg::CreatedSession(info) => self.created_session(*info),
            AppMsg::SessionLoaded(sess) => self.session_loaded(*sess),
            AppMsg::ForkSourceLoaded { id, sess } => self.fork_source_loaded(id, *sess),
            AppMsg::ForkedSession { info, draft } => self.forked_session(*info, draft),
            AppMsg::ClipboardText(text) => {
                if self.overlay.is_some() {
                    if overlays::overlay_has_query(Some(self.overlay.unwrap())) && !text.is_empty()
                    {
                        self.replace_or_append_overlay_query(&text);
                    }
                    return Vec::new();
                }
                if !text.is_empty() && !self.read_only_view() {
                    self.input.insert_str(&text);
                    return self.after_input_change();
                }
                Vec::new()
            }
            AppMsg::ClipboardImage { name, data } => {
                // Ctrl/Cmd+V with an image on the clipboard. Overlays
                // swallow it (matching Go's applyClipboardPaste, which
                // only routes text into a provider-key query and drops
                // images while any overlay is open).
                if self.overlay.is_some() {
                    return Vec::new();
                }
                match preview::paste_image(self, &name, &data) {
                    Ok(effects) => effects,
                    Err(e) => {
                        self.err_msg = e.to_string();
                        Vec::new()
                    }
                }
            }
            AppMsg::PendingImageReady { num, result } => {
                // Background image normalization finished. The marker
                // has been in the prompt since paste time; this just
                // fills the slot (or drops it on failure) and asks the
                // kitty layer to repaint.
                match result {
                    Ok(prepared) => {
                        preview::finalize_pending_image(self, num, prepared);
                    }
                    Err(e) => {
                        preview::drop_pending_image(self, num);
                        self.err_msg = format!("image prep failed: {e}");
                    }
                }
                if preview::kitty_terminal() {
                    vec![Effect::PaintPreviews]
                } else {
                    Vec::new()
                }
            }
            AppMsg::SendStarted { sid } => {
                // The runtime owns the receiver; bump our wait chain. A
                // live stream means a turn is in flight: clear any pause
                // left by an interrupted turn and restore streaming (the
                // old stream's SendClosed may have landed before this),
                // and settle any reasoning/tool blocks the old stream
                // never got to finalize.
                let _ = sid;
                self.stream_gen += 1;
                self.finalize_reasoning(None);
                self.finalize_compaction();
                self.finalize_tools();
                self.paused = false;
                self.streaming = true;
                self.interrupting = false;
                Vec::new()
            }
            AppMsg::SendEvent(v) => {
                let ev = crate::events::parse_stream_event(&v);
                let effects = self.handle_stream_event(&ev);
                self.refresh_viewport();
                effects
            }
            AppMsg::SendClosed => {
                self.finalize_reasoning(None);
                self.finalize_compaction();
                self.finalize_tools();
                self.streaming = false;
                if self.pending_saved {
                    self.pending_saved = false;
                    // While an interruption is in flight the old turn's
                    // stream just closed; its `saved` broadcast predates
                    // the interruption message, so a reload would drop
                    // the user block from the view. The interruption's
                    // own SendClosed (after SendStarted) reloads.
                    if !self.session.id.is_empty() && !self.interrupting {
                        return vec![Effect::LoadSession {
                            id: self.session.id.clone(),
                        }];
                    }
                }
                Vec::new()
            }
            AppMsg::SubStarted { sid } => {
                let _ = sid;
                Vec::new()
            }
            AppMsg::SubEvent(v) => self.sub_event(v),
            AppMsg::SubEnded { sid } => {
                if sid == self.session.id && !self.session.id.is_empty() {
                    let mut effects = vec![Effect::SubscribeAfter {
                        id: sid.clone(),
                        delay_ms: 1000,
                    }];
                    if self.streaming {
                        self.pending_saved = true;
                    } else {
                        effects.push(Effect::LoadSession { id: sid });
                    }
                    return effects;
                }
                Vec::new()
            }
            AppMsg::TickSpinner => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                if self.streaming || self.remote_working || self.test_mode || self.shell_running {
                    self.invalidate_live_blocks();
                }
                Vec::new()
            }
            AppMsg::TickSplash(t) => {
                if !self.splash_visible() {
                    return Vec::new();
                }
                self.splash_t = t;
                Vec::new()
            }
            AppMsg::TestSceneTick => {
                if !self.test_mode {
                    return Vec::new();
                }
                crate::outputtest::advance_output_test_scene(self)
            }
            AppMsg::OAuthDone(result) => self.oauth_done(result),
            AppMsg::McpOAuthDone { server, result } => self.mcp_oauth_done(&server, result),
            AppMsg::ShellKillArmed(tx) => {
                // Stale senders from an already-finished command are
                // dropped by the next ShellDone.
                self.shell_kill = Some(tx);
                Vec::new()
            }
            AppMsg::ShellDone {
                cmd,
                output,
                code,
                new_cwd,
                ..
            } => self.shell_done(cmd, output, code, new_cwd),
            // Handled by the loop before reaching the state machine.
            AppMsg::SubscribeNow(_)
            | AppMsg::HotRebuilt(_)
            | AppMsg::ThemeReloaded(_)
            | AppMsg::Redraw
            | AppMsg::MathWake
            | AppMsg::Heartbeat
            | AppMsg::SendReady { .. }
            | AppMsg::SubReady { .. } => Vec::new(),
        }
    }

    fn after_models_dev_ready(&mut self) -> Vec<Effect> {
        match self.overlay {
            Some(OverlayKind::Providers) => {
                self.overlay_providers = providers::list_addable_providers();
                self.working_msg.clear();
                self.overlay_sel = 0;
                Vec::new()
            }
            Some(OverlayKind::Model) => {
                self.working_msg = "loading models...".into();
                vec![Effect::FetchModels]
            }
            _ => Vec::new(),
        }
    }

    fn providers_rebuilt(&mut self, providers: Vec<Provider>) -> Vec<Effect> {
        self.providers = providers;
        if !self.sel_provider.name.is_empty() {
            if let Some(p) = atom_core::providers::providers::provider_by_name(
                &self.providers,
                &self.sel_provider.name,
            ) {
                self.sel_provider = p;
            } else if !self.sel_provider.id.is_empty() {
                if let Some(p) = self
                    .providers
                    .iter()
                    .find(|p| p.id == self.sel_provider.id)
                    .cloned()
                {
                    self.sel_provider = p;
                }
            }
        }
        let pref = if self.session.thinking.is_empty() {
            self.thinking_pref.clone()
        } else {
            self.session.thinking.clone()
        };
        self.refresh_thinking_levels();
        self.apply_thinking(&pref);
        match self.overlay {
            Some(OverlayKind::Model) => {
                if !self.pending_model_provider.is_empty() {
                    if let Some(provider) = self
                        .providers
                        .iter()
                        .find(|provider| provider.id == self.pending_model_provider)
                    {
                        self.overlay_q = provider.name.clone();
                    } else {
                        self.overlay_q = self.pending_model_provider.clone();
                    }
                }
                self.working_msg = "loading models...".into();
                vec![Effect::FetchModels]
            }
            Some(OverlayKind::Providers) => {
                self.overlay_providers = providers::list_addable_providers();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn created_session(&mut self, info: SessionInfo) -> Vec<Effect> {
        self.manage_restore_from.clear();
        self.hide_manage_menu();
        self.manage_agents.clear();
        self.manage_sel = 0;
        self.close_picker();
        self.close_context_menu();
        self.pending.clear();
        self.session = info;
        self.apply_thinking(&self.session.thinking.clone());
        self.persist_defaults();
        self.blocks.clear();
        self.paused = false;
        self.following = true;
        self.pending.clear();
        self.interrupting = false;
        self.preview_dirty = true;
        self.refresh_viewport();
        if !self.atom_config.setup_complete() {
            self.open_overlay(OverlayKind::Settings);
            self.settings_onboarding = true;
            self.overlay_sel = 0;
        }
        let mut fx = vec![Effect::Subscribe {
            id: self.session.id.clone(),
        }];
        if self.preview_dirty {
            fx.push(Effect::PaintPreviews);
        }
        fx
    }

    /// fork_source_loaded: the /fork overlay's source-session fetch
    /// resolved. Filter user messages into picker rows, install them on
    /// the App, and clear the loading spinner.
    ///
    /// Race: the user may have navigated to a different session while
    /// the request was in flight; if so, drop the response.
    fn fork_source_loaded(
        &mut self,
        id: String,
        sess: atom_core::session::store::Session,
    ) -> Vec<Effect> {
        if self.overlay != Some(OverlayKind::Fork) || id != self.overlay_fork_source {
            return Vec::new();
        }
        let mut user_messages: Vec<ForkUserMessage> = sess
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == "user")
            .map(|(idx, message)| {
                let preview = first_line(&message.content);
                ForkUserMessage {
                    position: idx as i64,
                    preview,
                    timestamp: fork_format_timestamp(message.created_at),
                }
            })
            .collect();
        // Drop empty user messages (a forked session can have a placeholder
        // user turn); they're not useful fork points.
        user_messages.retain(|msg| !msg.preview.is_empty());
        self.overlay_fork_user_messages = user_messages;
        self.overlay_sel = overlays::first_fork_row();
        self.overlay_scroll = 0;
        self.working_msg.clear();
        Vec::new()
    }

    /// forked_session: the server confirmed the fork. Switch into the
    /// child session (same as a /new, but with a pre-filled prompt and
    /// a `parent_id` lineage marker already set server-side) and write
    /// the chosen message's text into the prompt.
    fn forked_session(&mut self, info: SessionInfo, draft: String) -> Vec<Effect> {
        // Adopt the new session id locally so any in-flight state keyed
        // by the source id doesn't bleed across. session_loaded below
        // will see same_session=true and skip its reset block, so we
        // run the session-switch reset here too.
        self.remote_working = false;
        self.paused = false;
        self.following = true;
        self.streaming = false;
        self.interrupting = false;
        self.turn_id = String::new();
        self.pending.clear();
        self.preview_dirty = true;
        // Drop subagent picker / manage menu state — a forked root
        // session owns no agents.
        self.manage_restore_from.clear();
        self.hide_manage_menu();
        self.manage_agents.clear();
        self.manage_sel = 0;
        self.close_picker();
        self.close_context_menu();
        // Fork-specific: pre-fill the prompt with the picked message's
        // content, then dismiss the /fork overlay.
        self.input.set_value(&draft);
        self.last_picker_insert.clear();
        self.overlay = None;
        self.overlay_q.clear();
        self.overlay_q_cursor = None;
        self.overlay_fork_user_messages.clear();
        self.overlay_fork_source.clear();
        self.working_msg.clear();
        self.session = info;
        self.apply_thinking(&self.session.thinking.clone());
        self.persist_defaults();
        // Clear stale blocks from the source session so the view doesn't
        // briefly show the wrong transcript while the LoadSession below
        // round-trips the fork child's persisted messages.
        self.blocks.clear();
        self.refresh_viewport();
        // Fetch the full transcript (truncated messages up to the picked
        // message) and let session_loaded render it. Without this the
        // view starts empty and only populates after the user submits
        // the draft and the server pushes a `saved` event.
        vec![Effect::LoadSession {
            id: self.session.id.clone(),
        }]
    }

    /// sessionLoaded: switching sessions resets paused/following; the
    /// "saved" reload path keeps both.
    fn session_loaded(&mut self, sess: atom_core::session::store::Session) -> Vec<Effect> {
        let same_session = sess.id == self.session.id;
        self.remote_working = false;
        if !same_session {
            self.paused = false;
            self.following = true;
            self.streaming = false;
            self.interrupting = false;
            // The turn id belongs to the session it was generated for: a
            // stale id from the previous view would make a pause target a
            // turn that never exists (e.g. a subagent's "dispatch-<id>"
            // turn) instead of the session's live turns.
            self.turn_id = String::new();
            self.manage_restore_from = self.session.id.clone();
            self.hide_manage_menu();
            self.manage_agents.clear();
            self.manage_sel = 0;
            self.close_picker();
            self.close_context_menu();
            self.pending.clear();
            self.preview_dirty = true;
        }
        let session_provider = if sess.provider.is_empty() {
            sess.messages
                .iter()
                .rev()
                .find(|message| message.role == "assistant" && !message.provider.is_empty())
                .map(|message| message.provider.as_str())
                .unwrap_or("")
        } else {
            &sess.provider
        };
        if !session_provider.is_empty() {
            if let Some(provider) = self
                .providers
                .iter()
                .find(|provider| {
                    provider.name == session_provider || provider.id == session_provider
                })
                .cloned()
            {
                self.sel_provider = provider;
            }
        }
        self.session = sess.info();
        self.sel_model = sess.model.clone();
        if !same_session || !sess.thinking.is_empty() {
            self.refresh_thinking_levels();
            self.apply_thinking(&sess.thinking.clone());
        } else {
            self.session.thinking = self.thinking_level();
        }
        let prev = std::mem::take(&mut self.blocks);
        self.blocks = blocks::session_to_blocks(&sess);
        for block in &mut self.blocks {
            if block.kind == BlockKind::Reasoning {
                block.expanded = self.show_reasoning;
            }
        }
        let reserved: Vec<usize> = self.pending.iter().map(|p| p.num).collect();
        blocks::assign_block_image_nums(&mut self.blocks, &reserved);
        if blocks::assign_block_diagram_ids(&mut self.blocks) {
            self.preview_dirty = true;
        }
        if self
            .blocks
            .iter()
            .any(|b| b.kind == crate::blocks::BlockKind::User && !b.images.is_empty())
        {
            self.preview_dirty = true;
        }
        if same_session {
            let prev2 = prev.clone();
            blocks::restore_reasoning_durations(&mut self.blocks, &prev2);
        }
        self.viewport_dirty = true;
        self.refresh_viewport();

        let mut fx = Vec::new();
        if !same_session {
            // A stale subscription keeps draining; the runtime swaps to
            // the new session's channel on SubStarted.
        }
        fx.push(Effect::Subscribe {
            id: self.session.id.clone(),
        });
        // Don't push PaintPreviews here; preview_dirty is already set and
        // the post-draw check in the event loop will fire it AFTER the
        // first frame renders placeholder cells at the correct terminal
        // width. Pushing it here causes a race: the blocking paint task
        // transmits kitty data before the draw, potentially with stale
        // geometry (default 80-col width before the real size is known).
        if !self.session.id.is_empty() {
            fx.push(Effect::ListChildren {
                id: self.session.id.clone(),
            });
        }
        fx
    }

    /// SSE event from another instance's activity on this session.
    fn sub_event(&mut self, v: serde_json::Value) -> Vec<Effect> {
        let ev = crate::events::parse_stream_event(&v);
        if ev.event_type == "title" && !ev.text.is_empty() {
            self.session.title = ev.text.clone();
            return Vec::new();
        }
        if ev.event_type == "children" {
            if self.session.id.is_empty() {
                return Vec::new();
            }
            return vec![Effect::ListChildren {
                id: self.session.id.clone(),
            }];
        }
        let live = self.streaming;
        if ev.event_type == "approval_request" {
            // A subagent's request arrives on the session subscription
            // while the parent's own /send stream may be painting; it
            // must never be dropped, or the prompt (and the blocked
            // child) would silently hang.
            let effects = self.handle_stream_event(&ev);
            self.refresh_viewport();
            return effects;
        }
        if ev.event_type == "saved" {
            // A `saved` broadcast from the paused turn predates the
            // interruption message in the transcript; ignore it until
            // the interruption's own stream is live (SendStarted), or
            // the reload would drop the user block from the view.
            if self.interrupting {
                return Vec::new();
            }
            if live {
                self.pending_saved = true;
                return Vec::new();
            }
            self.remote_working = false;
            if self.session.id.is_empty() {
                return Vec::new();
            }
            return vec![Effect::LoadSession {
                id: self.session.id.clone(),
            }];
        }
        // Skip incremental SSE while our own /send stream paints, to
        // avoid duplicate tool blocks and overlapping text.
        if live {
            return Vec::new();
        }
        if ev.event_type == "subscribed" {
            return Vec::new();
        }
        if ev.event_type == "user_message" {
            // Another client sent a message on this session. Append it
            // directly instead of reloading — a reload mid-turn would
            // wipe the live streaming view. The sender is live and was
            // skipped above, so this only reaches viewing clients.
            self.blocks.push(Block {
                kind: BlockKind::User,
                text: ev.text.clone(),
                ..Default::default()
            });
            self.refresh_viewport();
            return Vec::new();
        }
        match ev.event_type.as_str() {
            "round_start" | "tool_pending" | "content" | "reasoning" | "reasoning_end" | "tool"
            | "tool_result" | "tool_diff" | "compaction" | "compaction_end" | "usage" => {
                self.remote_working = true;
            }
            "done" | "paused" | "error" => {
                self.remote_working = false;
            }
            _ => {}
        }
        let effects = self.handle_stream_event(&ev);
        self.refresh_viewport();
        effects
    }

    fn oauth_done(&mut self, result: Result<AuthEntry, String>) -> Vec<Effect> {
        self.working_msg.clear();
        match result {
            Err(e) => {
                if !e.contains("canceled") && !e.contains("cancel") {
                    self.err_msg = e;
                }
            }
            Ok(entry) => {
                if let Err(e) = auth::set_auth("openai", entry) {
                    self.err_msg = e.to_string();
                    self.open_overlay(OverlayKind::Providers);
                    self.overlay_q.clear();
                    self.overlay_sel = 0;
                    self.overlay_providers = providers::list_addable_providers();
                    return Vec::new();
                }
                self.open_models_for_provider("openai");
                return vec![Effect::ReloadProviders];
            }
        }
        self.open_overlay(OverlayKind::Providers);
        self.overlay_q.clear();
        self.overlay_sel = 0;
        self.overlay_auth_type.clear();
        self.overlay_providers = providers::list_addable_providers();
        vec![Effect::ReloadProviders]
    }

    fn open_models_for_provider(&mut self, id: &str) {
        self.open_overlay(OverlayKind::Model);
        self.overlay_q.clear();
        self.overlay_sel = 0;
        self.overlay_scroll = 0;
        self.overlay_auth_type.clear();
        self.pending_model_provider = id.to_string();
        self.working_msg = "loading models...".into();
    }

    /// Handle the result of Effect::StartMcpOAuth. On success the
    /// auth store already holds fresh tokens for `server`, so the
    /// slash catalog is rebuilt and the picker (if still open) is
    /// refreshed in place. On failure surface the error and bail
    /// back to the picker so the user can retry.
    fn mcp_oauth_done(&mut self, server: &str, result: Result<(), String>) -> Vec<Effect> {
        self.working_msg.clear();
        match result {
            Err(e) => {
                if !e.contains("canceled") && !e.contains("cancel") {
                    self.err_msg = format!("MCP sign-in failed for {server}: {e}");
                }
                if !matches!(self.picker_kind, PickerKind::None) {
                    self.refresh_picker_items();
                }
                Vec::new()
            }
            Ok(()) => {
                // Slash catalog items are computed once on init and
                // cached on the App; rebuild so the meta column flips
                // from "auth required" to "authenticated" for this
                // server and any picker still open reflects that.
                self.slash_commands = overlays::discover_commands(&self.cwd);
                if !matches!(self.picker_kind, PickerKind::None) {
                    self.refresh_picker_items();
                }
                Vec::new()
            }
        }
    }

    /// Re-populate picker items for the current picker_kind without
    /// resetting the selection so the highlighted row stays where it
    /// was after a state change (e.g. an MCP just finished OAuth).
    fn refresh_picker_items(&mut self) {
        let kind = match self.picker_kind {
            PickerKind::Mcp => "mcp",
            PickerKind::Skills => "skill",
            PickerKind::None => return,
        };
        self.picker_items = picker_items(&self.slash_commands, kind);
        if self.picker_sel >= self.picker_items.len() {
            self.picker_sel = self.picker_items.len().saturating_sub(1);
        }
    }

    /// Shared resize path for AppMsg::Resize and view::draw's
    /// area-size sync: update dims, re-wrap content when width moved.
    pub(crate) fn apply_resize(&mut self, w: u16, h: u16) {
        let prev_w = self.width;
        self.width = w;
        self.height = h;
        if w != prev_w {
            self.refresh_viewport();
            if self.blocks.iter().any(|block| block.diagram.is_some()) {
                self.preview_dirty = true;
            }
        }
    }

    fn resize(&mut self, w: u16, h: u16) -> Vec<Effect> {
        let width_changed = w != self.width;
        self.apply_resize(w, h);
        if width_changed && self.blocks.iter().any(|block| block.diagram.is_some()) {
            // refresh_viewport recomputes each diagram's placeholder grid;
            // transmit a matching virtual placement for the new geometry.
            self.preview_dirty = true;
            vec![Effect::PaintPreviews]
        } else {
            Vec::new()
        }
    }

    fn paste(&mut self, content: String) -> Vec<Effect> {
        // A paste into an open overlay (e.g. the /provider key prompt) goes
        // to the overlay query, not the main prompt input — matching
        // ClipboardText (Ctrl/Cmd+V) routing below.
        if let Some(kind) = self.overlay {
            if overlays::overlay_has_query(Some(kind)) && !content.is_empty() {
                self.replace_or_append_overlay_query(&content);
            }
            return Vec::new();
        }
        // A paste mixing text and OSC 1337 images inserts both in order.
        if content.contains("\x1b]1337;") {
            return preview::paste_mixed_content(self, &content);
        }
        // Finder / kitty file drops arrive as quoted paths.
        let files = preview::local_images_from_paste(&content);
        if !files.is_empty() {
            return preview::paste_local_images(self, files);
        }
        if self.read_only_view() {
            return Vec::new();
        }
        self.input.insert_str(&content);
        self.after_input_change()
    }

    /// afterInputChange syncs menus/previews after prompt edits.
    pub fn after_input_change(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.shell_mode {
            // Shell mode has no slash/@ menus; keep image chips in sync.
            if preview::sync_pending_from_input(self) {
                effects.push(Effect::PaintPreviews);
            }
            return effects;
        }
        if (!matches!(self.picker_kind, PickerKind::None)
            || self.context_visible
            || self.reasoning_visible)
            && !self.input.value.trim().is_empty()
        {
            self.close_picker();
            self.close_context_menu();
            self.close_reasoning_menu();
        }
        if preview::sync_pending_from_input(self) {
            effects.push(Effect::PaintPreviews);
        }
        // Slash menu visibility follows the typed prefix; a Ctrl+P-opened
        // menu (menu_virtual) stays open against the prompt as-is.
        let typed = self.menu_typed();
        if (typed.starts_with('/') && !overlays::looks_like_file_path(&typed)) || self.menu_virtual
        {
            let n = overlays::match_commands(&typed, &self.slash_commands).len();
            if n > 0 {
                self.set_menu_visible(true);
                if self.menu_sel >= n {
                    self.menu_sel = 0;
                }
            } else {
                self.set_menu_visible(false);
            }
        } else {
            self.set_menu_visible(false);
        }

        // @ file-mention menu: trigger when the cursor is inside an @word.
        self.sync_at_menu();

        effects
    }

    // -- shell mode ----------------------------------------------------------

    /// Submits a shell-mode command: append a running tool block and
    /// spawn the shell. The result arrives as AppMsg::ShellDone.
    fn run_shell_command(&mut self, cmd: &str) -> Vec<Effect> {
        self.shell_running = true;
        self.err_msg.clear();
        let mut block = Block::new(BlockKind::Tool);
        block.title = format!("! {cmd}");
        block.tool_name = "shell".into();
        block.tool_done = false;
        self.blocks.push(block);
        self.following = true;
        self.refresh_viewport();
        vec![Effect::RunShell {
            cmd: cmd.to_string(),
            cwd: self.cwd.clone(),
        }]
    }

    /// Leaves shell mode, aborting any running command (Ctrl+C, or
    /// backspacing out of an empty prompt).
    pub fn exit_shell_mode(&mut self) {
        self.shell_mode = false;
        self.shell_running = false;
        self.input.clear();
        if let Some(tx) = self.shell_kill.take() {
            let _ = tx.send(());
        }
    }

    /// ShellDone: fill in the running block, then follow a `cd` by moving
    /// the app's cwd (footer, @-completion, skill discovery) and patching
    /// the session on the server so the agent's tools move too.
    fn shell_done(
        &mut self,
        cmd: String,
        output: String,
        code: Option<i32>,
        new_cwd: String,
    ) -> Vec<Effect> {
        self.shell_running = false;
        self.shell_kill = None;

        let mut result = output;
        match code {
            Some(0) | None => {}
            Some(exit) => result.push_str(&format!("\nexit code {exit}")),
        }
        // Killed commands report code None; say so unless output already
        // explains itself (spawn errors carry their own message).
        if code.is_none() && !result.starts_with("error:") {
            result.push_str("\nkilled");
        }

        let title = format!("! {cmd}");
        if let Some(block) = self.blocks.iter_mut().rev().find(|b| {
            b.kind == BlockKind::Tool && b.tool_name == "shell" && !b.tool_done && b.title == title
        }) {
            block.tool_done = true;
            block.result = result;
            block.lines = None;
            self.viewport_dirty = true;
        }

        let mut effects = Vec::new();
        if !new_cwd.is_empty() && new_cwd != self.cwd {
            self.cwd = new_cwd.clone();
            self.slash_commands = overlays::discover_commands(&self.cwd);
            if !self.session.id.is_empty() {
                effects.push(Effect::PatchSessionCwd {
                    id: self.session.id.clone(),
                    cwd: new_cwd,
                });
            }
        }
        effects
    }

    // -- keys ----------------------------------------------------------------

    pub fn key(&mut self, k: KeyEvent) -> Vec<Effect> {
        // The sandbox approval overlay pauses normal input entirely.
        if let Some(req) = self.approval.clone() {
            return self.approval_key(k, req);
        }

        if k.kind != crossterm::event::KeyEventKind::Press
            && k.kind != crossterm::event::KeyEventKind::Repeat
        {
            return Vec::new();
        }

        if self.overlay.is_some() {
            return self.update_overlay_key(k);
        }

        // Read-only subagent views accept no user input: swallow the
        // editing/sending keys and let everything else (Esc, arrows,
        // Ctrl+C, Ctrl+P, Shift+Up) fall through to the global bindings.
        if self.read_only_view() {
            match k.code {
                KeyCode::Char(_) => {
                    let global = k.modifiers.contains(KeyModifiers::CONTROL)
                        || k.modifiers.contains(KeyModifiers::ALT)
                        || k.modifiers.contains(KeyModifiers::SUPER);
                    if !global {
                        return Vec::new();
                    }
                }
                KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete => return Vec::new(),
                _ => {}
            }
        }

        let mods = k.modifiers;
        let shift = mods.contains(KeyModifiers::SHIFT);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);

        // Shell mode: Ctrl+C leaves (aborting a running command) instead
        // of clearing the prompt / quitting the app.
        if self.shell_mode && ctrl && matches!(k.code, KeyCode::Char('c')) {
            self.exit_shell_mode();
            return Vec::new();
        }

        // Alt/Shift+Enter insert a newline instead of sending; Ctrl+J too.
        if let KeyCode::Enter = k.code {
            if alt || shift {
                self.input.newline();
                return self.after_input_change();
            }
        }
        if let KeyCode::Char('j') = k.code {
            if ctrl {
                self.input.newline();
                return self.after_input_change();
            }
        }
        if let KeyCode::Char('a') = k.code {
            if mods.contains(KeyModifiers::SUPER) {
                self.input.select_all();
                return Vec::new();
            }
        }
        if let KeyCode::Down = k.code {
            if shift {
                return self.open_manage_menu();
            }
        }
        if let KeyCode::Up = k.code {
            if shift {
                if !self.session.parent_id.is_empty() {
                    return vec![Effect::LoadSession {
                        id: self.session.parent_id.clone(),
                    }];
                }
                // On the parent, Shift+Up closes the subagent menu
                // (the mirror of Shift+Down opening it).
                if self.manage_visible {
                    self.dismiss_manage_menu();
                }
                return Vec::new();
            }
        }

        // Footer menus capture only their navigation keys; everything
        // else falls through to the global bindings.
        if self.context_visible {
            if let Some(fx) = self.context_key(k.code) {
                return fx;
            }
        } else if self.reasoning_visible {
            if let Some(fx) = self.reasoning_key(k.code) {
                return fx;
            }
        } else if !matches!(self.picker_kind, PickerKind::None) {
            if let Some(fx) = self.picker_key(k.code) {
                return fx;
            }
        } else if self.manage_visible {
            if let Some(fx) = self.manage_key(k.code) {
                return fx;
            }
        } else if self.menu_visible {
            if let Some(fx) = self.menu_key(k.code) {
                return fx;
            }
        } else if self.at_menu_visible {
            if let Some(fx) = self.at_menu_key(k.code) {
                return fx;
            }
        }

        match k.code {
            KeyCode::Char('v') if ctrl || mods.contains(KeyModifiers::SUPER) => {
                return vec![Effect::ReadClipboard];
            }
            KeyCode::Char('c') if mods.contains(KeyModifiers::SUPER) && !ctrl => {
                // Cmd+C copies the prompt's text selection first (Cmd+A
                // selects all input text), then falls back to the
                // conversation scrollback selection.
                if self.input.has_selection() {
                    let text = self.input.selected_text();
                    if !text.is_empty() {
                        self.copied_msg = format!("Copied {} chars", text.chars().count());
                        self.copied_at = Some(std::time::Instant::now());
                        self.input.clear_selection();
                        return vec![Effect::CopyToClipboard { text }];
                    }
                }
                if self.sel_active {
                    return self.copy_selection();
                }
                return Vec::new();
            }
            KeyCode::Char('c') if ctrl => {
                if !self.input.value.is_empty() {
                    self.input.clear();
                    return self.after_input_change();
                }
                let mut out = self.commit_thinking();
                self.quitting = true;
                out.push(Effect::Quit);
                return out;
            }
            KeyCode::Char('d') if ctrl => {
                let mut out = self.commit_thinking();
                self.quitting = true;
                out.push(Effect::Quit);
                return out;
            }
            KeyCode::Char('p') if ctrl => {
                // Ctrl+P toggles the "/" command menu without touching
                // the prompt: open it as-is, close it when already open.
                if self.menu_visible {
                    self.set_menu_visible(false);
                } else {
                    self.menu_virtual = !self.input.value.starts_with('/');
                    let typed = self.menu_typed();
                    let n = overlays::match_commands(&typed, &self.slash_commands).len();
                    if n > 0 {
                        self.set_menu_visible(true);
                        if self.menu_sel >= n {
                            self.menu_sel = 0;
                        }
                    }
                }
                return Vec::new();
            }
            KeyCode::Esc => {
                if self.sel_active || self.selecting {
                    self.clear_selection();
                }
                self.input.clear_selection();
                if self.streaming {
                    self.paused = true;
                    return vec![Effect::PauseTurn];
                }
                // A detached subagent turn (dispatch) runs without a TUI
                // /send stream, so `streaming` stays false while the child
                // works. Esc in a subagent view must still stop it, with an
                // empty turn_id: the server registers the child turn as
                // "dispatch-<child id>", so a stale turn id from a previous
                // send would only record a pending pause that never fires.
                if !self.session.parent_id.is_empty() && self.remote_working {
                    self.paused = true;
                    self.turn_id = String::new();
                    return vec![Effect::PauseTurn];
                }
            }
            KeyCode::Enter => {
                let text = self.input.value.trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                self.set_menu_visible(false);
                if self.shell_running {
                    // One command at a time; the running block's spinner
                    // shows why the submit is a no-op.
                    return Vec::new();
                }
                if self.shell_mode {
                    self.input.clear();
                    return self.run_shell_command(&text);
                }
                // `!` prefix enters shell mode; `!cmd` runs right away.
                if let Some(rest) = text.strip_prefix('!') {
                    self.shell_mode = true;
                    let cmd = rest.trim();
                    if cmd.is_empty() {
                        return Vec::new();
                    }
                    self.input.clear();
                    return self.run_shell_command(cmd);
                }
                return self.handle_input(&text);
            }
            KeyCode::Tab => {
                self.cycle_thinking();
                return self.commit_thinking();
            }
            KeyCode::Char('t') if ctrl => {
                self.cycle_thinking();
                return self.commit_thinking();
            }
            KeyCode::Up => {
                if self.prompt_navigable() {
                    self.input.up();
                    return Vec::new();
                }
                let half = (self.content_viewport_height() / 2).max(1) as i64;
                self.scroll_viewport(-half);
            }
            KeyCode::Down => {
                if self.prompt_navigable() {
                    self.input.down();
                    return Vec::new();
                }
                let half = (self.content_viewport_height() / 2).max(1) as i64;
                self.scroll_viewport(half);
            }
            KeyCode::PageUp => {
                self.scroll_viewport(-(self.content_viewport_height() as i64));
            }
            KeyCode::PageDown => {
                self.scroll_viewport(self.content_viewport_height() as i64);
            }
            KeyCode::Home => {
                self.following = false;
                self.scroll_y = 0;
            }
            KeyCode::End => {
                self.following = true;
                self.refresh_viewport();
            }
            KeyCode::Backspace => {
                // Deleting backwards past the start of an empty shell-mode
                // prompt leaves shell mode.
                if self.shell_mode && self.input.value.is_empty() {
                    self.exit_shell_mode();
                    return Vec::new();
                }
                self.input.backspace();
                return self.after_input_change();
            }
            KeyCode::Delete => {
                self.input.delete_fwd();
                return self.after_input_change();
            }
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Char(ch) => {
                if ctrl || alt {
                    return Vec::new();
                }
                // `!` on an empty prompt enters shell mode; the bang
                // itself is not inserted.
                if ch == '!' && self.input.value.is_empty() {
                    self.shell_mode = true;
                    return Vec::new();
                }
                self.input.insert_str(&ch.to_string());
                return self.after_input_change();
            }
            _ => {}
        }
        Vec::new()
    }

    pub fn scroll_viewport(&mut self, delta: i64) {
        let total = self.content_lines.len() as i64;
        let vp = self.content_viewport_height() as i64;
        let max = (total - vp).max(0);
        self.scroll_y = (self.scroll_y as i64 + delta).clamp(0, max.max(0)) as usize;
        self.following = self.scroll_y as i64 >= max.max(0);
    }

    /// Jumps scroll position so the scrollbar thumb tracks the given screen Y.
    /// Used for click/drag on the scrollbar track.
    fn scroll_to_scrollbar_y(&mut self, y: usize) {
        let track = self.viewport_height();
        if track == 0 {
            return;
        }
        let content_visible = self.content_viewport_height();
        let total = self.content_lines.len();
        let max_scroll = total.saturating_sub(content_visible);
        if max_scroll == 0 {
            return;
        }
        // Map y (screen row) to a position within the track [0, track-1].
        let row_in_track = y.saturating_sub(VIEWPORT_VPAD).min(track.saturating_sub(1));
        // Proportional scroll: row_in_track / (track - 1) ≈ scroll_y / max_scroll.
        let new_scroll = if track <= 1 {
            0
        } else {
            row_in_track * max_scroll / (track - 1)
        };
        self.scroll_y = new_scroll.min(max_scroll);
        self.following = self.scroll_y >= max_scroll;
    }

    /// Returns None for keys the menu does not own (they fall through).
    fn context_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match code {
            KeyCode::Esc => self.close_context_menu(),
            KeyCode::Up => {
                if self.context_sel > 0 {
                    self.context_sel -= 1;
                }
            }
            KeyCode::Down => {
                if self.context_sel + 1 < self.context_rows.len() {
                    self.context_sel += 1;
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Returns None for keys the menu does not own (they fall through).
    fn reasoning_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match code {
            KeyCode::Esc => self.close_reasoning_menu(),
            KeyCode::Enter => return Some(self.select_reasoning_level()),
            KeyCode::Up => {
                if self.reasoning_sel > 0 {
                    self.reasoning_sel -= 1;
                }
            }
            KeyCode::Down => {
                if self.reasoning_sel + 1 < self.thinking_levels.len() {
                    self.reasoning_sel += 1;
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// selectReasoningLevel applies the highlighted level (Enter/click).
    pub fn select_reasoning_level(&mut self) -> Vec<Effect> {
        if self.reasoning_sel < self.thinking_levels.len() {
            self.thinking_idx = self.reasoning_sel;
            self.thinking_pref = self.thinking_levels[self.reasoning_sel].clone();
        }
        self.close_reasoning_menu();
        self.refresh_viewport();
        self.commit_thinking()
    }

    fn picker_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match code {
            KeyCode::Esc => self.close_picker(),
            KeyCode::Enter => return Some(self.select_picker_item(true)),
            KeyCode::Up => {
                if self.picker_sel > 0 {
                    self.picker_sel -= 1;
                }
            }
            KeyCode::Down => {
                if self.picker_sel + 1 < self.picker_items.len() {
                    self.picker_sel += 1;
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn manage_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match code {
            KeyCode::Esc => self.dismiss_manage_menu(),
            KeyCode::Up => {
                if self.manage_sel > 0 {
                    self.manage_sel -= 1;
                }
            }
            KeyCode::Down => {
                if self.manage_sel + 1 < self.manage_agents.len() {
                    self.manage_sel += 1;
                }
            }
            KeyCode::Enter => return Some(self.select_manage_agent()),
            _ => return None,
        }
        Some(Vec::new())
    }

    fn at_menu_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match code {
            KeyCode::Esc => {
                self.close_at_menu();
            }
            KeyCode::Up => {
                if self.at_menu_sel > 0 {
                    self.at_menu_sel -= 1;
                }
            }
            KeyCode::Down => {
                if self.at_menu_sel + 1 < self.at_menu_items.len() {
                    self.at_menu_sel += 1;
                }
            }
            KeyCode::Enter | KeyCode::Tab => {
                return Some(self.select_at_menu_item());
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    fn menu_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        let typed = self.menu_typed();
        match code {
            KeyCode::Esc => self.set_menu_visible(false),
            KeyCode::Up => {
                if self.menu_sel > 0 {
                    self.menu_sel -= 1;
                }
            }
            KeyCode::Down => {
                let n = overlays::match_commands(&typed, &self.slash_commands).len();
                if self.menu_sel + 1 < n {
                    self.menu_sel += 1;
                }
            }
            KeyCode::Enter => return Some(self.select_slash_match()),
            KeyCode::Tab => {
                let matches = overlays::match_commands(&typed, &self.slash_commands);
                if !matches.is_empty() && self.menu_sel < matches.len() {
                    self.input.set_value(&matches[self.menu_sel].name);
                    self.set_menu_visible(false);
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    // -- overlay keys --------------------------------------------------------

    fn update_overlay_key(&mut self, k: KeyEvent) -> Vec<Effect> {
        let Some(kind) = self.overlay else {
            return Vec::new();
        };

        // Clipboard bindings (Cmd/Ctrl+V paste, Cmd+A select-all, Cmd+C copy)
        // normally live in key(); the overlay short-circuit there swallows
        // them, so handle them here for query overlays (e.g. /provider key).
        if overlays::overlay_has_query(Some(kind)) {
            let mods = k.modifiers;
            let super_ = mods.contains(KeyModifiers::SUPER);
            if let KeyCode::Char('v') = k.code {
                if mods.contains(KeyModifiers::CONTROL) || super_ {
                    // ReadClipboard -> ClipboardText already routes into the
                    // overlay query (replace_or_append_overlay_query).
                    return vec![Effect::ReadClipboard];
                }
            }
            if let KeyCode::Char('a') = k.code {
                if super_ {
                    self.overlay_q_sel = true;
                    return Vec::new();
                }
            }
            if let KeyCode::Char('c') = k.code {
                if super_ && !mods.contains(KeyModifiers::CONTROL) && self.overlay_q_sel {
                    return vec![Effect::CopyToClipboard {
                        text: self.overlay_q.clone(),
                    }];
                }
            }
        }

        if k.code == KeyCode::Char('d') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return match kind {
                OverlayKind::Providers => self.disconnect_selected_provider(),
                OverlayKind::Session => self.delete_selected_session(),
                _ => Vec::new(),
            };
        }

        if k.code == KeyCode::Char('p') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return match kind {
                OverlayKind::Model => self.toggle_selected_model_pin(),
                OverlayKind::Session => self.toggle_selected_session_pin(),
                _ => Vec::new(),
            };
        }

        // Tab re-samples the live pids in /profile so the overlay can
        // be refreshed without leaving the view. Other overlays ignore
        // Tab here — the menu/footer menu handlers consume it.
        if matches!(
            k,
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            }
        ) && matches!(kind, OverlayKind::Profile)
        {
            return vec![Effect::FetchProfile {
                client_pid: std::process::id() as i32,
                server_pid: self.server_pid,
            }];
        }

        match k.code {
            KeyCode::Esc => {
                match kind {
                    OverlayKind::ProviderMethod | OverlayKind::ProviderKey => {
                        self.open_overlay(OverlayKind::Providers);
                        self.overlay_q.clear();
                        self.overlay_sel = 0;
                        self.overlay_providers = providers::list_addable_providers();
                    }
                    OverlayKind::WebSearch => {
                        self.open_overlay(OverlayKind::Settings);
                        self.overlay_sel = 2;
                    }
                    OverlayKind::WebFetch => {
                        self.open_overlay(OverlayKind::Settings);
                        self.overlay_sel = 3;
                    }
                    OverlayKind::Model
                        if self.model_picker_purpose
                            == overlays::ModelPickerPurpose::Compaction =>
                    {
                        self.open_overlay(OverlayKind::Settings);
                        self.overlay_sel = 0;
                        self.overlay_q.clear();
                        self.working_msg.clear();
                    }
                    OverlayKind::Settings if self.settings_onboarding => {
                        self.accept_settings_defaults();
                        self.overlay = None;
                    }
                    OverlayKind::Fork => {
                        // Drop the picker state so reopening /fork reloads
                        // the source session from scratch — the user
                        // might have added a message in the meantime.
                        self.overlay_fork_user_messages.clear();
                        self.overlay_fork_source.clear();
                        self.overlay = None;
                        self.overlay_q.clear();
                        self.overlay_q_cursor = None;
                        self.overlay_q_cursor = None;
                        self.working_msg.clear();
                    }
                    _ => {
                        self.overlay = None;
                        self.overlay_q.clear();
                        self.overlay_q_cursor = None;
                        self.working_msg.clear();
                    }
                }
                Vec::new()
            }
            KeyCode::Enter => self.confirm_overlay(),
            KeyCode::Up => {
                match kind {
                    OverlayKind::Stats => {
                        if self.overlay_sel > 0 {
                            self.overlay_sel -= 1;
                        }
                        // Stats has no selection highlight: overlay_sel
                        // is the scroll window's first row, so the
                        // renderer's scroll must track it.
                        self.overlay_scroll = self.overlay_sel;
                    }
                    OverlayKind::Profile => {
                        // Profile reuses the same scroll-as-sel trick
                        // as /stats: overlay_sel is the first visible
                        // row, so the renderer can just copy it.
                        if self.overlay_sel > 0 {
                            self.overlay_sel -= 1;
                        }
                        self.overlay_scroll = self.overlay_sel;
                    }
                    OverlayKind::Session => overlays::move_session_sel(self, -1),
                    OverlayKind::Model => overlays::move_model_sel(self, -1),
                    OverlayKind::Fork => {
                        let rows = overlays::fork_rows(self);
                        let new_sel = overlays::move_fork_sel(&rows, self.overlay_sel, -1);
                        self.overlay_sel = new_sel;
                        overlays::sync_overlay_scroll(self);
                    }
                    OverlayKind::ProviderKey => {}
                    _ => {
                        if self.overlay_sel > 0 {
                            self.overlay_sel -= 1;
                        }
                        overlays::sync_overlay_scroll(self);
                    }
                }
                Vec::new()
            }
            KeyCode::Down => {
                match kind {
                    OverlayKind::Stats => {
                        let max = overlays::stats_scroll_max(self);
                        if self.overlay_sel < max {
                            self.overlay_sel += 1;
                        }
                        // See Up: scroll tracks the selection row.
                        self.overlay_scroll = self.overlay_sel;
                    }
                    OverlayKind::Profile => {
                        let max = overlays::profile_scroll_max(self);
                        if self.overlay_sel < max {
                            self.overlay_sel += 1;
                        }
                        self.overlay_scroll = self.overlay_sel;
                    }
                    OverlayKind::Session => overlays::move_session_sel(self, 1),
                    OverlayKind::Model => overlays::move_model_sel(self, 1),
                    OverlayKind::Fork => {
                        let rows = overlays::fork_rows(self);
                        let new_sel = overlays::move_fork_sel(&rows, self.overlay_sel, 1);
                        self.overlay_sel = new_sel;
                        overlays::sync_overlay_scroll(self);
                    }
                    OverlayKind::ProviderKey => {}
                    _ => {
                        let cnt = overlays::overlay_count(self);
                        if cnt > 0 && self.overlay_sel < cnt - 1 {
                            self.overlay_sel += 1;
                        }
                        overlays::sync_overlay_scroll(self);
                    }
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                if matches!(
                    kind,
                    OverlayKind::Stats | OverlayKind::ProviderMethod | OverlayKind::Profile
                ) {
                    return Vec::new();
                }
                if self.overlay_q_sel {
                    self.overlay_q.clear();
                    self.overlay_q_sel = false;
                    self.overlay_q_cursor = Some(0);
                    self.reset_overlay_sel_after_query();
                    return Vec::new();
                }
                if overlays::overlay_has_query(Some(kind)) {
                    // Caret-aware delete: remove the char before the
                    // caret instead of popping the tail.
                    self.overlay_backspace_char();
                    return Vec::new();
                }
                Vec::new()
            }
            KeyCode::Delete => {
                if overlays::overlay_has_query(Some(kind)) {
                    self.overlay_delete_char();
                }
                Vec::new()
            }
            KeyCode::Left => {
                if overlays::overlay_has_query(Some(kind)) {
                    self.overlay_move_caret(-1);
                }
                Vec::new()
            }
            KeyCode::Right => {
                if overlays::overlay_has_query(Some(kind)) {
                    self.overlay_move_caret(1);
                }
                Vec::new()
            }
            KeyCode::Home => {
                if overlays::overlay_has_query(Some(kind)) {
                    self.overlay_q_cursor = Some(0);
                }
                Vec::new()
            }
            KeyCode::End => {
                if overlays::overlay_has_query(Some(kind)) {
                    let len = self.overlay_q.chars().count();
                    self.overlay_q_cursor = Some(len);
                }
                Vec::new()
            }
            KeyCode::Char(c) => {
                if k.modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    return Vec::new();
                }
                if overlays::overlay_has_query(Some(kind)) {
                    let text = if c == ' ' {
                        " ".to_string()
                    } else {
                        c.to_string()
                    };
                    self.replace_or_append_overlay_query(&text);
                    self.reset_overlay_sel_after_query();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn reset_overlay_sel_after_query(&mut self) {
        if self.overlay == Some(OverlayKind::Session) {
            self.overlay_sel = overlays::first_session_row(self);
            self.overlay_scroll = 0;
            overlays::sync_session_scroll(self);
        } else if self.overlay == Some(OverlayKind::Model) {
            self.overlay_sel = overlays::first_model_row(self);
            self.overlay_scroll = 0;
            overlays::sync_model_scroll(self);
        } else if self.overlay == Some(OverlayKind::Fork) {
            // Typing in the search box always lands the selection on
            // the SessionLatest sentinel when no filter matches, or
            // the first matching user message otherwise. This mirrors
            // the behavior of /sessions: the picker resets to the top.
            let rows = overlays::fork_rows(self);
            // Find the first non-Header row.
            let first_pick = rows
                .iter()
                .position(|r| r.kind != overlays::ForkRowKind::Header)
                .unwrap_or(0);
            self.overlay_sel = first_pick;
            self.overlay_scroll = 0;
        } else if self.overlay != Some(OverlayKind::ProviderKey) {
            self.overlay_sel = 0;
        }
    }

    pub fn replace_or_append_overlay_query(&mut self, text: &str) {
        if self.overlay_q_sel {
            self.overlay_q = text.to_string();
            self.overlay_q_sel = false;
            self.overlay_q_cursor = Some(text.chars().count());
            return;
        }
        if self.overlay_q_cursor.is_some() && overlays::overlay_has_query(self.overlay) {
            // Caret-aware insert (also covers paste): typed/pasted text
            // lands at the caret, which then sits at its end.
            let caret = self.overlay_caret();
            let inserted = text.chars().count();
            let byte_at = self
                .overlay_q
                .char_indices()
                .nth(caret)
                .map(|(b, _)| b)
                .unwrap_or(self.overlay_q.len());
            self.overlay_q.insert_str(byte_at, text);
            self.overlay_q_cursor = Some(caret + inserted);
            return;
        }
        self.overlay_q.push_str(text);
    }

    /// Caret char offset in the overlay search input, clamped to the
    /// current query; `None` cursor state behaves as end-of-query.
    fn overlay_caret(&self) -> usize {
        let len = self.overlay_q.chars().count();
        self.overlay_q_cursor.map_or(len, |c| c.min(len))
    }

    /// Delete the char before the caret in the overlay search input.
    fn overlay_backspace_char(&mut self) {
        let caret = self.overlay_caret();
        if caret == 0 {
            return;
        }
        let mut chars: Vec<char> = self.overlay_q.chars().collect();
        chars.remove(caret - 1);
        self.overlay_q = chars.into_iter().collect();
        self.overlay_q_cursor = Some(caret - 1);
        self.reset_overlay_sel_after_query();
    }

    /// Delete the char at the caret in the overlay search input.
    fn overlay_delete_char(&mut self) {
        let caret = self.overlay_caret();
        let mut chars: Vec<char> = self.overlay_q.chars().collect();
        if caret >= chars.len() {
            return;
        }
        chars.remove(caret);
        self.overlay_q = chars.into_iter().collect();
        self.overlay_q_cursor = Some(caret);
        self.reset_overlay_sel_after_query();
    }

    /// Move the caret ±1 chars, clamped to the query bounds.
    fn overlay_move_caret(&mut self, dir: i32) {
        let len = self.overlay_q.chars().count() as i32;
        let caret = self.overlay_caret() as i32 + dir;
        self.overlay_q_cursor = Some(caret.clamp(0, len) as usize);
    }

    fn disconnect_selected_provider(&mut self) -> Vec<Effect> {
        let filtered = filter_provider_entries(&self.overlay_providers, &self.overlay_q);
        if filtered.is_empty() {
            return Vec::new();
        }
        let sel = self.overlay_sel.min(filtered.len() - 1);
        let e = filtered[sel].clone();
        if e.id == "ollama-local" || !e.connected || !e.stored {
            return Vec::new();
        }
        let _ = auth::remove_auth(&e.id);
        auth::remove_legacy_provider_key(&e.id);
        vec![Effect::ReloadProviders]
    }

    fn delete_selected_session(&mut self) -> Vec<Effect> {
        let rows = overlays::session_rows(self);
        if self.overlay_sel >= rows.len() || rows[self.overlay_sel].date {
            return Vec::new();
        }
        let id = rows[self.overlay_sel].sess.as_ref().unwrap().id.clone();
        self.remove_overlay_session(&id);
        vec![Effect::DeleteSession { id }]
    }

    fn save_atom_config(&mut self) {
        if self.test_mode {
            return;
        }
        if let Err(error) = atom_core::config::save(&self.atom_config) {
            self.err_msg = format!("save settings: {error}");
        } else {
            atom_tools::close_all_mcp();
            self.slash_commands = overlays::discover_commands(&self.cwd);
        }
    }

    fn accept_settings_defaults(&mut self) {
        if self.atom_config.compaction.is_none() {
            self.atom_config.compaction = Some(self.atom_config.resolved_compaction());
        }
        if self.atom_config.web_search.is_none() {
            self.atom_config.web_search = Some(self.atom_config.resolved_web_search());
        }
        if self.atom_config.web_fetch.is_none() {
            self.atom_config.web_fetch = Some(self.atom_config.resolved_web_fetch());
        }
        self.settings_onboarding = false;
        self.save_atom_config();
    }

    fn save_picker_settings(&mut self) {
        if !self.test_mode {
            if let Err(err) = crate::settings::save(&self.picker_settings) {
                self.err_msg = format!("save picker settings: {err}");
            }
        }
    }

    fn toggle_selected_model_pin(&mut self) -> Vec<Effect> {
        let Some(entry) = overlays::selected_model(self) else {
            return Vec::new();
        };
        let selected =
            crate::settings::PickerSettings::model_ref(&entry.provider.name, &entry.model);
        self.picker_settings.toggle_model(selected);
        self.save_picker_settings();
        let rows = overlays::model_rows(self);
        if let Some(i) = rows.iter().position(|row| {
            row.entry.as_ref().is_some_and(|candidate| {
                candidate.provider.name == entry.provider.name && candidate.model == entry.model
            })
        }) {
            self.overlay_sel = i;
        }
        overlays::sync_model_scroll(self);
        Vec::new()
    }

    fn toggle_selected_session_pin(&mut self) -> Vec<Effect> {
        let rows = overlays::session_rows(self);
        let Some(session) = rows
            .get(self.overlay_sel)
            .and_then(|row| row.sess.as_ref())
            .cloned()
        else {
            return Vec::new();
        };
        self.picker_settings.toggle_session(&session.id);
        self.save_picker_settings();
        let rows = overlays::session_rows(self);
        if let Some(i) = rows.iter().position(|row| {
            row.sess
                .as_ref()
                .is_some_and(|candidate| candidate.id == session.id)
        }) {
            self.overlay_sel = i;
        }
        overlays::sync_session_scroll(self);
        Vec::new()
    }

    pub fn remove_overlay_session(&mut self, id: &str) {
        if self.session.id == id {
            self.session = empty_session_info();
            self.blocks.clear();
            self.streaming = false;
            self.paused = false;
            self.dismiss_manage_menu();
            self.manage_agents.clear();
            self.manage_sticky.remove(id);
            self.close_context_menu();
            self.close_reasoning_menu();
            self.refresh_viewport();
        }
        self.overlay_sessions.retain(|s| s.id != id);
        let pins_before = self.picker_settings.pinned_sessions.len();
        self.picker_settings
            .pinned_sessions
            .retain(|pinned| pinned != id);
        if self.picker_settings.pinned_sessions.len() != pins_before {
            self.save_picker_settings();
        }

        let rows = overlays::session_rows(self);
        if rows.is_empty() {
            self.overlay_sel = 0;
            self.overlay_scroll = 0;
            return;
        }
        let mut idx = self.overlay_sel.min(rows.len() - 1);
        if !rows[idx].date {
            self.overlay_sel = idx;
            overlays::sync_session_scroll(self);
            return;
        }
        let mut found: Option<usize> = (0..idx).rev().find(|i| !rows[*i].date);
        if found.is_none() {
            found = (idx + 1..rows.len()).find(|i| !rows[*i].date);
        }
        idx = match found {
            Some(i) => i,
            None => overlays::first_session_row(self),
        };
        self.overlay_sel = idx;
        overlays::sync_session_scroll(self);
    }

    /// confirmOverlay handles Enter in an overlay.
    pub fn confirm_overlay(&mut self) -> Vec<Effect> {
        let Some(kind) = self.overlay else {
            return Vec::new();
        };
        match kind {
            OverlayKind::Model => {
                let Some(e) = overlays::selected_model(self) else {
                    return Vec::new();
                };
                if self.model_picker_purpose == overlays::ModelPickerPurpose::Compaction {
                    self.atom_config.compaction = Some(atom_core::config::CompactionConfig {
                        provider: if e.provider.id.is_empty() {
                            e.provider.name.clone()
                        } else {
                            e.provider.id.clone()
                        },
                        model: e.model.clone(),
                        ..self.atom_config.compaction.clone().unwrap_or_default()
                    });
                    self.save_atom_config();
                    self.model_picker_purpose = overlays::ModelPickerPurpose::Chat;
                    self.open_overlay(OverlayKind::Settings);
                    self.overlay_sel = 0;
                    self.overlay_q.clear();
                    self.working_msg.clear();
                    return Vec::new();
                }
                let prev_thinking = self.thinking_level();
                self.sel_provider = e.provider.clone();
                self.sel_model = e.model.clone();
                self.refresh_thinking_levels();
                if !self.session.thinking.is_empty() {
                    self.apply_thinking(&self.session.thinking.clone());
                } else {
                    self.apply_thinking(&prev_thinking);
                }
                self.session.thinking = self.thinking_level();
                self.picker_settings
                    .push_recent(crate::settings::PickerSettings::model_ref(
                        &e.provider.name,
                        &e.model,
                    ));
                self.save_picker_settings();
                self.persist_defaults();

                if !self.session.id.is_empty() {
                    // Mid-session switch: update the model in place.
                    let provider = e.provider.name.clone();
                    let model = e.model.clone();
                    let thinking = self.session.thinking.clone();
                    self.session.provider = provider.clone();
                    self.session.model = model.clone();
                    self.overlay = None;
                    self.overlay_q.clear();
                    self.overlay_q_cursor = None;
                    self.working_msg.clear();
                    return vec![Effect::PatchSessionModel {
                        provider,
                        model,
                        thinking,
                    }];
                }
                // No session yet: create one with the selected model.
                let provider = e.provider.name.clone();
                let model = e.model.clone();
                let cwd = self.cwd.clone();
                let thinking = self.thinking_level();
                self.overlay = None;
                self.overlay_q.clear();
                self.overlay_q_cursor = None;
                self.working_msg.clear();
                vec![Effect::CreateSession {
                    provider,
                    model,
                    cwd,
                    thinking,
                }]
            }
            OverlayKind::Session => {
                let rows = overlays::session_rows(self);
                if self.overlay_sel >= rows.len() || rows[self.overlay_sel].date {
                    self.overlay_sel = overlays::first_session_row(self);
                }
                if self.overlay_sel >= rows.len() {
                    return Vec::new();
                }
                let picked = rows[self.overlay_sel].sess.as_ref().unwrap().id.clone();
                self.overlay = None;
                self.overlay_q.clear();
                self.overlay_q_cursor = None;
                self.working_msg.clear();
                vec![Effect::LoadSession { id: picked }]
            }
            OverlayKind::Stats => {
                self.overlay = None;
                self.overlay_q.clear();
                self.overlay_q_cursor = None;
                self.working_msg.clear();
                Vec::new()
            }
            OverlayKind::Profile => {
                // /profile is read-only: Enter just closes (same shape
                // as /stats). Tab is handled separately in the key
                // dispatch so this path only sees Enter.
                self.overlay = None;
                self.overlay_q.clear();
                self.overlay_q_cursor = None;
                self.working_msg.clear();
                Vec::new()
            }
            OverlayKind::Providers => {
                let filtered = filter_provider_entries(&self.overlay_providers, &self.overlay_q);
                if filtered.is_empty() {
                    return Vec::new();
                }
                if self.overlay_sel >= filtered.len() {
                    self.overlay_sel = 0;
                }
                let e = filtered[self.overlay_sel].clone();
                if e.id == "ollama-local" {
                    return Vec::new();
                }
                self.overlay_auth_id = e.id.clone();
                // Web-tool providers (tinyfish, parallel, exa) only take
                // API keys — skip the API Key/OAuth method choice.
                if atom_core::providers::providers::provider_caps(&e.id).contains(&"Models") {
                    self.open_overlay(OverlayKind::ProviderMethod);
                } else {
                    self.overlay_auth_type = "api".into();
                    self.open_overlay(OverlayKind::ProviderKey);
                }
                self.overlay_q.clear();
                self.overlay_sel = 0;
                Vec::new()
            }
            OverlayKind::ProviderMethod => {
                self.overlay_auth_type = if self.overlay_sel == 0 {
                    "api".into()
                } else {
                    "oauth".into()
                };
                if self.overlay_auth_type == "oauth" && self.overlay_auth_id == "openai" {
                    self.overlay = None;
                    self.working_msg = "waiting for ChatGPT sign-in in the browser...".into();
                    return vec![Effect::StartOpenAIOAuth];
                }
                self.open_overlay(OverlayKind::ProviderKey);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                Vec::new()
            }
            OverlayKind::ProviderKey => {
                let secret = self.overlay_q.trim().to_string();
                if secret.is_empty() || self.overlay_auth_id.is_empty() {
                    return Vec::new();
                }
                let entry = if self.overlay_auth_type == "oauth" {
                    AuthEntry {
                        r#type: "oauth".into(),
                        access: secret,
                        expires: 0,
                        ..Default::default()
                    }
                } else {
                    AuthEntry {
                        r#type: "api".into(),
                        key: secret,
                        ..Default::default()
                    }
                };
                let id = self.overlay_auth_id.clone();
                if let Err(err) = auth::set_auth(&id, entry) {
                    self.err_msg = err.to_string();
                    self.overlay = None;
                    self.overlay_q.clear();
                    self.overlay_q_cursor = None;
                    self.working_msg.clear();
                    return Vec::new();
                }
                if atom_core::providers::providers::provider_caps(&id).contains(&"Models") {
                    self.open_models_for_provider(&id);
                } else {
                    // Web-tool providers (search/fetch only) have no
                    // models: return to the providers list so the row
                    // now reads connected.
                    self.overlay_providers = providers::list_addable_providers();
                    self.open_overlay(OverlayKind::Providers);
                    self.overlay_q.clear();
                    self.overlay_sel = 0;
                }
                vec![Effect::ReloadProviders]
            }
            OverlayKind::Settings => match self.overlay_sel {
                0 => {
                    self.model_picker_purpose = overlays::ModelPickerPurpose::Compaction;
                    self.open_overlay(OverlayKind::Model);
                    self.overlay_q.clear();
                    self.overlay_sel = 0;
                    self.overlay_scroll = 0;
                    self.working_msg = "loading models...".into();
                    vec![Effect::FetchModels]
                }
                1 => {
                    let mut compaction = self.atom_config.compaction.clone().unwrap_or_default();
                    compaction.enabled = Some(!compaction.resolved_enabled());
                    self.atom_config.compaction = Some(compaction);
                    self.save_atom_config();
                    Vec::new()
                }
                2 => {
                    self.open_overlay(OverlayKind::WebSearch);
                    let selected = self.atom_config.resolved_web_search().server;
                    let rows = overlays::web_search_rows(self);
                    self.overlay_sel = rows.iter().position(|row| row.0 == selected).unwrap_or(0);
                    Vec::new()
                }
                3 => {
                    self.open_overlay(OverlayKind::WebFetch);
                    let selected = self.atom_config.resolved_web_fetch().server;
                    let rows = overlays::web_fetch_rows(self);
                    self.overlay_sel = rows.iter().position(|row| row.0 == selected).unwrap_or(0);
                    Vec::new()
                }
                _ => {
                    self.accept_settings_defaults();
                    self.overlay = None;
                    Vec::new()
                }
            },
            OverlayKind::WebSearch => {
                let rows = overlays::web_search_rows(self);
                let Some((id, _, _)) = rows.get(self.overlay_sel).cloned() else {
                    return Vec::new();
                };
                let tool = atom_core::config::bundled_web_search_profile(&id)
                    .map(|profile| profile.tool)
                    .unwrap_or_else(|| "web_search".into());
                self.atom_config.web_search =
                    Some(atom_core::config::WebSearchConfig { server: id, tool });
                self.save_atom_config();
                self.open_overlay(OverlayKind::Settings);
                self.overlay_sel = 2;
                Vec::new()
            }
            OverlayKind::WebFetch => {
                let rows = overlays::web_fetch_rows(self);
                let Some((id, _, _)) = rows.get(self.overlay_sel).cloned() else {
                    return Vec::new();
                };
                let tool = atom_core::config::bundled_web_fetch_profile(&id)
                    .map(|profile| profile.tool)
                    .unwrap_or_else(|| "web_fetch".into());
                self.atom_config.web_fetch =
                    Some(atom_core::config::WebFetchConfig { server: id, tool });
                self.save_atom_config();
                self.open_overlay(OverlayKind::Settings);
                self.overlay_sel = 3;
                Vec::new()
            }
            OverlayKind::Theme => {
                let rows = overlays::theme_rows();
                let Some(entry) = rows.get(self.overlay_sel) else {
                    return Vec::new();
                };
                let id = entry.id.clone();
                let name = entry.name.clone();
                if let Err(error) = atom_core::render::colors::apply_theme(&id) {
                    self.err_msg = format!("theme: {error}");
                    return Vec::new();
                }
                self.atom_config.theme = Some(id);
                self.save_atom_config();
                self.overlay = None;
                self.copied_msg = format!("theme: {name}");
                self.copied_at = Some(Instant::now());
                // The palette changed behind every cached render: drop the
                // block caches and repaint previews, mirroring what the
                // hot theme reload path does. The next frame's
                // refresh_viewport rebuilds against the new palette.
                self.invalidate_all_blocks();
                self.preview_dirty = true;
                vec![Effect::PaintPreviews]
            }
            OverlayKind::Fork => {
                let rows = overlays::fork_rows(self);
                if self.overlay_sel >= rows.len() {
                    return Vec::new();
                }
                let row = &rows[self.overlay_sel];
                if row.kind == overlays::ForkRowKind::Header {
                    return Vec::new();
                }
                let source_id = self.overlay_fork_source.clone();
                let position = match row.kind {
                    overlays::ForkRowKind::SessionLatest => None,
                    overlays::ForkRowKind::UserMessage => row.position,
                    overlays::ForkRowKind::Header => return Vec::new(),
                };
                self.working_msg = "forking session...".into();
                vec![Effect::ForkSession {
                    source_id,
                    position,
                }]
            }
        }
    }

    // -- approval ----------------------------------------------------------

    fn approval_key(&mut self, k: KeyEvent, req: ApprovalPrompt) -> Vec<Effect> {
        // v2 spec: four buttons, no session-scoped grant.
        //   Y → allow_once, A → allow_always, N → deny_once,
        //   D → deny_always (lowercase and the capital letters shown
        //   on the buttons). Esc is intentionally not bound — the four
        //   visible buttons are the only choices.
        let decision = match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(("allow_once", "allowed once")),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(("allow_always", "always allowed")),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(("deny_once", "denied")),
            KeyCode::Char('d') | KeyCode::Char('D') => Some(("deny_always", "denied, rule saved")),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quitting = true;
                return vec![Effect::Quit];
            }
            _ => None,
        };
        match decision {
            Some((wire, note)) => {
                self.resolve_approval_block(&req.id, note);
                self.approval = None;
                self.copied_msg = format!("sandbox: {note}");
                self.copied_at = Some(Instant::now());
                vec![Effect::RespondApproval {
                    sid: req.session_id.clone(),
                    id: req.id,
                    decision: wire.to_string(),
                }]
            }
            None => Vec::new(),
        }
    }

    /// Clear the inline approval card (button row, child header) once
    /// the user has answered the prompt. The block stays
    /// `tool_done = false`: the tool hasn't actually returned yet — the
    /// server is still running the command and will send a `tool_result`
    /// event with the real output, which `attach_tool_result` needs a
    /// non-done tool block to land on. Marking it done here would either
    /// orphan the real result into its own block or attach it to a
    /// previous still-open tool block, both of which break the transcript
    /// and confuse the model loop.
    fn resolve_approval_block(&mut self, approval_id: &str, note: &str) {
        for b in self.blocks.iter_mut().rev() {
            if b.kind == BlockKind::Tool {
                if let Some(ref appr) = b.approval {
                    if appr.id == approval_id {
                        b.result = format!("sandbox: {note}");
                        b.approval = None;
                        b.lines = None;
                        self.viewport_dirty = true;
                        break;
                    }
                }
            }
        }
    }

    /// Check if a click at viewport (x, y) lands anywhere on a visualize
    /// block. The entire card is the click target — header, summary rows,
    /// and the inline image alike — and opens the browser pan/zoom viewer.
    /// The block spans `block_start` (content row of its top pad row)
    /// through `block_start + lines.len() - 1`.
    fn diagram_open_hit(&self, bi: usize, x: usize, y: usize) -> Option<String> {
        let d = self.blocks.get(bi)?.diagram.as_ref()?;
        if d.html.is_empty() {
            return None;
        }
        let block_start = *self.block_start.get(bi)?;
        let content_row = y.checked_sub(VIEWPORT_VPAD)? + self.scroll_y;
        let lines = self.blocks[bi].lines.as_ref()?;
        let last_row = block_start + lines.len().saturating_sub(1);
        if content_row < block_start || content_row > last_row {
            return None;
        }
        // Column: anywhere across the card (the boxed text starts one pad
        // column in and spans the full inner width).
        let inner = self.inner_width().saturating_sub(2).max(1);
        let col = x.checked_sub(TUI_HPAD)?;
        if col <= inner + 1 {
            Some(d.html.clone())
        } else {
            None
        }
    }

    /// Check if a click at (content_row, col) falls on an approval button in
    /// block `bi`. Returns the decision string ("allow_once" etc.) or None.
    fn approval_button_hit(&self, bi: usize, content_row: usize, col: usize) -> Option<String> {
        // The buttons are on the rendered line just above the bottom
        // pad row. Block layout is [pad] [header] ...body... [pad],
        // and the button line is the last body row, so it sits two
        // lines from the end (penultimate). It was briefly `len - 3`
        // while a help row sat between buttons and pad; the help row
        // is gone and that offset silently broke button clicks.
        let block_start = *self.block_start.get(bi)?;
        let block_lines = self.blocks[bi].lines.as_ref()?;
        let button_row = block_start + block_lines.len().saturating_sub(2);
        if content_row != button_row {
            return None;
        }
        // Button layout: "  Y Once   A Always   N Deny   D Never  "
        // Body rows are wrapped with a one-cell leading pad column,
        // so a line-relative col c is button-relative c - 1.
        let buttons = blocks::approval_buttons();
        for btn in &buttons {
            if col > btn.col_start && col < btn.col_end + 1 {
                return Some(btn.decision.to_string());
            }
        }
        None
    }

    /// Handle clicking an approval button: resolve the block and emit the
    /// approval effect.
    fn resolve_approval_click(&mut self, bi: usize, decision: &str) -> Vec<Effect> {
        let appr = match self.blocks[bi].approval.take() {
            Some(a) => a,
            None => return Vec::new(),
        };
        let note = match decision {
            "allow_once" => "allow once",
            "allow_always" => "always allowed",
            "deny_once" => "denied",
            "deny_always" => "denied",
            _ => "denied",
        };
        self.blocks[bi].tool_done = true;
        self.blocks[bi].result = format!("sandbox: {note}");
        self.blocks[bi].lines = None;
        self.viewport_dirty = true;
        self.approval = None;
        self.copied_msg = format!("sandbox: {note}");
        self.copied_at = Some(Instant::now());
        self.refresh_viewport();
        vec![Effect::RespondApproval {
            sid: appr.session_id,
            id: appr.id,
            decision: decision.to_string(),
        }]
    }

    // -- mouse ---------------------------------------------------------------

    pub fn mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        let (x, y) = (m.column as usize, m.row as usize);
        if self.overlay.is_some() {
            return match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // The `esc` hint on the overlay title row dismisses
                    // the overlay like the key.
                    if overlays::overlay_esc_hint_hit(self, x, y) {
                        return self.update_overlay_key(KeyEvent::new(
                            KeyCode::Esc,
                            KeyModifiers::empty(),
                        ));
                    }
                    // A click on the search row places the caret
                    // at the clicked column instead of selecting a row.
                    if overlays::overlay_click_search(self, x, y) {
                        Vec::new()
                    } else {
                        overlays::click_overlay(self, y)
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    overlays::hover_overlay_row(self, y);
                    Vec::new()
                }
                // Wheel scrolls modal overlays by moving the selection the
                // same way the arrow keys do (scroll follows via the
                // sticky keep-visible logic).
                MouseEventKind::ScrollUp => {
                    self.update_overlay_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()))
                }
                MouseEventKind::ScrollDown => {
                    self.update_overlay_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()))
                }
                _ => Vec::new(),
            };
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(x, y),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(x, y),
            MouseEventKind::Up(MouseButton::Left) => self.release(x, y),
            MouseEventKind::ScrollUp => self.wheel(false, y),
            MouseEventKind::ScrollDown => self.wheel(true, y),
            _ => Vec::new(),
        }
    }

    fn click(&mut self, x: usize, y: usize) -> Vec<Effect> {
        self.link_pending = None;
        // Status-bar hints are clickable: the subagent indicator opens the
        // subagent menu (Shift+↓), "Shift ↑ to return" goes to the parent
        // session.
        if let Some(action) = self.status_nav_hit(x, y) {
            return self.run_status_nav_action(action);
        }
        // Scrollbar click: the rightmost SCROLLBAR_WIDTH columns within the
        // viewport region. The wider target also helps in terminal
        // multiplexer splits where the absolute last column's mouse events
        // are intercepted for pane-resize handling.
        if x >= (self.width as usize).saturating_sub(SCROLLBAR_WIDTH)
            && y >= VIEWPORT_VPAD
            && y < VIEWPORT_VPAD + self.viewport_height()
            && !self.content_lines.is_empty()
        {
            self.scrollbar_dragging = true;
            self.scroll_to_scrollbar_y(y);
            return Vec::new();
        }
        let viewport_y = y.checked_sub(VIEWPORT_VPAD);
        if self.context_visible {
            if let Some(row) = viewport_y.and_then(|y| overlays::context_row_at_y(self, y)) {
                self.context_sel = row;
            }
            return Vec::new();
        }
        if self.reasoning_visible {
            if let Some(row) = viewport_y.and_then(|y| overlays::reasoning_row_at_y(self, y)) {
                self.reasoning_sel = row;
                return self.select_reasoning_level();
            }
            return Vec::new();
        }
        if !matches!(self.picker_kind, PickerKind::None) {
            if let Some(row) = viewport_y.and_then(|y| overlays::picker_row_at_y(self, y)) {
                self.picker_sel = row;
                return self.select_picker_item(false);
            }
            return Vec::new();
        }
        if self.manage_visible {
            if let Some(row) = viewport_y.and_then(|y| overlays::manage_row_at_y(self, y)) {
                self.manage_sel = row;
                return self.select_manage_agent();
            }
            return Vec::new();
        }
        if self.menu_visible {
            if let Some(row) = viewport_y.and_then(|y| overlays::menu_row_at_y(self, y)) {
                self.menu_sel = row;
                return self.select_slash_match();
            }
            return Vec::new();
        }
        if self.at_menu_visible {
            if let Some(row) = viewport_y.and_then(|y| overlays::at_menu_row_at_y(self, y)) {
                self.at_menu_sel = row;
                return self.select_at_menu_item();
            }
            return Vec::new();
        }
        if self.mouse_in_prompt(y) {
            if self.sel_active {
                self.clear_selection();
            }
            self.prompt_selecting = true;
            // A click on an actual editable text row repositions the cursor
            // at the clicked (wrapped, scrolled) display position. Padding /
            // border rows and the image-preview rows are left untouched.
            let geo = crate::view::Layout::compute(self);
            let text_top = geo.prompt_top_y + PROMPT_PAD;
            let text_bottom = text_top + self.input_height();
            if y >= text_top && y < text_bottom {
                self.input.clear_selection();
                let col = x.saturating_sub(TUI_HPAD + PROMPT_PAD);
                let wrapped_row = (y - text_top) + self.input.scroll_y;
                let offset = self
                    .input
                    .offset_at_display(self.input_width(), wrapped_row, col);
                self.input.cursor = offset;
            }
            return Vec::new();
        }
        self.input.clear_selection();
        if self.sel_active {
            self.clear_selection();
        }
        // A press on an OSC 8 link arms it: opened on release unless the
        // press turns into a drag selection. Takes priority over the
        // collapse/expand toggles below so clicking a link inside a
        // collapsed card opens it instead of toggling.
        if let Some(uri) = self.link_hit(x, y) {
            self.link_pending = Some(uri);
            if let Some(pos) = self.content_pos_at(x, y) {
                self.selecting = true;
                self.sel_anchor = Some(pos);
                self.sel_end = Some(pos);
            }
            return Vec::new();
        }
        let mut effects = Vec::new();
        let idx = self.block_index_at_viewport_y(y);
        if idx >= 0 {
            let bi = idx as usize;
            let content_row = y.checked_sub(VIEWPORT_VPAD).map(|row| self.scroll_y + row);
            let label = blocks::reasoning_label(
                &self.blocks[bi],
                MINIDOT_FRAMES[self.spinner_frame % MINIDOT_FRAMES.len()],
            );
            let on_reasoning_header = self.blocks[bi].kind == BlockKind::Reasoning
                && content_row == self.block_start.get(bi).copied()
                && x.checked_sub(TUI_HPAD)
                    .is_some_and(|x| x < unicode_width::UnicodeWidthStr::width(label.as_str()));
            if on_reasoning_header {
                self.blocks[bi].expanded = !self.blocks[bi].expanded;
                self.blocks[bi].lines = None;
                self.viewport_dirty = true;
                self.refresh_viewport();
                return Vec::new();
            }
        }
        if let Some(pos) = self.content_pos_at(x, y) {
            self.selecting = true;
            self.sel_anchor = Some(pos);
            self.sel_end = Some(pos);
        }
        if idx >= 0 {
            let bi = idx as usize;
            let inner = self.inner_width().saturating_sub(2).max(1);
            if self.blocks[bi].kind == BlockKind::User
                && self.blocks[bi].user_collapsible(inner, &self.cwd)
            {
                self.blocks[bi].expanded = !self.blocks[bi].expanded;
                self.blocks[bi].lines = None;
                self.viewport_dirty = true;
                self.refresh_viewport();
                return Vec::new();
            }
            if self.blocks[bi].kind == BlockKind::Tool {
                // Approval block: check if click lands on a button.
                if self.blocks[bi].approval.is_some() {
                    let col = x.saturating_sub(TUI_HPAD);
                    if let Some(content_row) =
                        y.checked_sub(VIEWPORT_VPAD).map(|vy| self.scroll_y + vy)
                    {
                        if let Some(decision) = self.approval_button_hit(bi, content_row, col) {
                            return self.resolve_approval_click(bi, &decision);
                        }
                    }
                    return Vec::new();
                }
                if let Some(uri) = self.diagram_open_hit(bi, x, y) {
                    self.copied_msg = "opening diagram viewer".into();
                    self.copied_at = Some(Instant::now());
                    return vec![Effect::OpenLink { uri }];
                }
                if self.blocks[bi].tool_collapsible(inner, inner, &self.cwd) {
                    self.blocks[bi].expanded = !self.blocks[bi].expanded;
                    self.blocks[bi].lines = None;
                    self.viewport_dirty = true;
                    self.refresh_viewport();
                    return Vec::new();
                }
                let id = self.blocks[bi].session_id.clone();
                if !id.is_empty() {
                    effects.push(Effect::LoadSession { id });
                }
            }
        }
        effects
    }

    fn drag(&mut self, x: usize, y: usize) -> Vec<Effect> {
        if self.scrollbar_dragging {
            self.scroll_to_scrollbar_y(y);
            return Vec::new();
        }
        if self.prompt_selecting {
            return Vec::new();
        }
        if self.selecting {
            // Auto-scroll when the pointer reaches an edge.
            if y <= VIEWPORT_VPAD {
                if self.scroll_y > 0 {
                    self.scroll_y -= 1;
                }
            } else if self.content_viewport_height() > 0
                && y >= VIEWPORT_VPAD + self.content_viewport_height() - 1
            {
                self.scroll_y += 1;
                let max = self
                    .content_lines
                    .len()
                    .saturating_sub(self.content_viewport_height());
                self.following = self.scroll_y >= max;
            }
            if let Some(pos) = self.content_pos_at(x, y) {
                if self.sel_end != Some(pos) {
                    self.sel_end = Some(pos);
                    self.sel_active = true;
                    // The press became a selection, not a link open.
                    self.link_pending = None;
                }
            }
        }
        Vec::new()
    }

    fn release(&mut self, _x: usize, _y: usize) -> Vec<Effect> {
        if self.scrollbar_dragging {
            self.scrollbar_dragging = false;
            return Vec::new();
        }
        if let Some(uri) = self.link_pending.take() {
            if !self.sel_active {
                // Plain click on a link: open it.
                self.clear_selection();
                self.copied_msg =
                    format!("opening {}", atom_core::util::first_line_trunc(&uri, 48));
                self.copied_at = Some(Instant::now());
                return vec![Effect::OpenLink { uri }];
            }
            // The press turned into a drag selection; copy path below.
        }
        if self.selecting {
            self.selecting = false;
            if self.sel_active {
                return self.copy_selection();
            }
        }
        if self.prompt_selecting {
            self.prompt_selecting = false;
            if self.input.has_selection() {
                let text = self.input.selected_text();
                let n = text.chars().count();
                if n > 0 {
                    self.copied_msg = format!("Copied {n} chars");
                    self.copied_at = Some(Instant::now());
                    self.input.clear_selection();
                    return vec![Effect::CopyToClipboard { text }];
                }
            }
            self.input.clear_selection();
        }
        Vec::new()
    }

    fn wheel(&mut self, down: bool, y: usize) -> Vec<Effect> {
        // An open footer menu owns the wheel: scroll its selection the
        // same way the arrow keys do (the menu window follows).
        if self.footer_menu_wheel(down) {
            return Vec::new();
        }
        if self.prompt_navigable() && self.mouse_in_prompt(y) {
            let h = self.input_height();
            let total = self.input.content_lines(self.input_width());
            let max_off = total.saturating_sub(h);
            if down {
                self.input.scroll_y = (self.input.scroll_y + 3).min(max_off);
            } else {
                self.input.scroll_y = self.input.scroll_y.saturating_sub(3);
            }
            return Vec::new();
        }
        if down {
            self.scroll_viewport(3);
        } else {
            self.scroll_viewport(-3);
        }
        Vec::new()
    }

    pub fn mouse_in_prompt(&self, y: usize) -> bool {
        if self.read_only_view() {
            return false;
        }
        let geo = crate::view::Layout::compute(self);
        y >= geo.prompt_top_y
            && y < geo.prompt_top_y
                + 2 * PROMPT_PAD
                + self.input_height()
                + self.preview_row_count()
    }

    /// The clickable status-bar hint under a screen position, if any.
    fn status_nav_hit(&self, x: usize, y: usize) -> Option<crate::statusbar::NavAction> {
        let geo = crate::view::Layout::compute(self);
        let row = y.checked_sub(geo.status_y)?;
        let col = x.saturating_sub(TUI_HPAD);
        crate::statusbar::nav_hit_regions(self)
            .into_iter()
            .find(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
            .map(|(_, _, _, action)| action)
    }

    /// Routes a wheel event to the open footer menu's arrow-key handler.
    /// Returns false when no menu is open (the wheel then scrolls the
    /// viewport or prompt as usual).
    fn footer_menu_wheel(&mut self, down: bool) -> bool {
        let code = if down { KeyCode::Down } else { KeyCode::Up };
        if self.context_visible {
            self.context_key(code).is_some()
        } else if self.reasoning_visible {
            self.reasoning_key(code).is_some()
        } else if !matches!(self.picker_kind, PickerKind::None) {
            self.picker_key(code).is_some()
        } else if self.manage_visible {
            self.manage_key(code).is_some()
        } else if self.menu_visible {
            self.menu_key(code).is_some()
        } else if self.at_menu_visible {
            self.at_menu_key(code).is_some()
        } else {
            false
        }
    }

    /// Runs the action bound to a clicked status-bar hint.
    fn run_status_nav_action(&mut self, action: crate::statusbar::NavAction) -> Vec<Effect> {
        use crate::statusbar::NavAction;
        match action {
            NavAction::OpenSubagents => self.open_manage_menu(),
            NavAction::ReturnToParent => {
                if !self.session.parent_id.is_empty() {
                    vec![Effect::LoadSession {
                        id: self.session.parent_id.clone(),
                    }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn content_pos_at(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        let vp_h = self.content_viewport_height();
        let y = y.checked_sub(VIEWPORT_VPAD)?;
        if y >= vp_h || self.content_lines.is_empty() {
            return None;
        }
        let line = (self.scroll_y + y).min(self.content_lines.len() - 1);
        let col = x.saturating_sub(TUI_HPAD);
        let w = ansi_line_width(&self.content_lines[line]);
        Some((line, col.min(w)))
    }

    /// The OSC 8 URI under a viewport position, if any.
    pub fn link_hit(&self, x: usize, y: usize) -> Option<String> {
        let (line, col) = self.content_pos_at(x, y)?;
        let region = self
            .link_lines
            .get(line)?
            .iter()
            .find(|r| col >= r.c0 && col < r.c1)?;
        Some(region.uri.clone())
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn ansi_line_width(line: &Line<'_>) -> usize {
    use unicode_width::UnicodeWidthStr;
    line.spans.iter().map(|s| s.content.width()).sum()
}

pub(crate) fn new_turn_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default()
}

/// Lists project files relative to `cwd`, filtered by query.
/// Walks the directory tree respecting .gitignore and common ignores,
/// returning up to 50 matches sorted by relevance.
fn list_project_files(cwd: &str, query: &str) -> Vec<String> {
    use std::path::Path;

    let root = Path::new(cwd);
    if !root.is_dir() {
        return Vec::new();
    }

    let mut files: Vec<String> = Vec::new();
    collect_files(root, root, &mut files, 0);
    files.sort();

    // Filter by query (case-insensitive substring match on any component)
    let query_lower = query.to_lowercase();
    let filtered: Vec<String> = if query_lower.is_empty() {
        files.into_iter().take(50).collect()
    } else {
        files
            .into_iter()
            .filter(|f| f.to_lowercase().contains(&query_lower))
            .take(50)
            .collect()
    };
    filtered
}

/// Recursively collect files, skipping common non-project directories.
fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<String>,
    depth: usize,
) {
    const MAX_DEPTH: usize = 6;
    const MAX_FILES: usize = 5000;
    const IGNORE_DIRS: &[&str] = &[
        "target",
        "node_modules",
        ".git",
        ".hg",
        ".svn",
        "dist",
        "build",
        "__pycache__",
        ".next",
        ".venv",
        "venv",
    ];

    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden files/dirs (starting with .) except a few
        if name_str.starts_with('.') && name_str != ".github" {
            continue;
        }

        if path.is_dir() {
            if IGNORE_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_files(root, &path, out, depth + 1);
        } else {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

pub(crate) fn join_prompt(cur: &str, insert: &str) -> String {
    let cur = cur.trim_end_matches([' ', '\t']);
    if cur.is_empty() {
        return insert.to_string();
    }
    format!("{cur} {insert}")
}

pub(crate) fn empty_session_info() -> SessionInfo {
    SessionInfo {
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
        created_at: chrono_now(),
        updated_at: chrono_now(),
    }
}

fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Returns the first non-empty line of `text`, trimmed and clipped to
/// a friendly preview length. Used for /fork row labels and similar
/// one-line title summaries.
fn first_line(text: &str) -> String {
    let mut iter = text.lines();
    let mut line = iter.next().unwrap_or("").trim().to_string();
    // Skip leading blank lines.
    while line.is_empty() {
        line = iter.next().unwrap_or("").trim().to_string();
    }
    // Collapse internal whitespace so a wrapped preview fits one visual line.
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 80;
    if collapsed.chars().count() > MAX {
        let mut out: String = collapsed.chars().take(MAX - 1).collect();
        out.push('…');
        out
    } else {
        collapsed
    }
}

/// Formats a DateTime as a local HH:MM stamp for the /fork picker.
/// Returns "—" when the timestamp is missing or unparseable.
fn fork_format_timestamp(created_at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(ts) = created_at else {
        return "—".into();
    };
    let local: chrono::DateTime<chrono::Local> = ts.with_timezone(&chrono::Local);
    local.format("%H:%M").to_string()
}

fn picker_items(commands: &[DynamicCommand], kind: &str) -> Vec<PickerItem> {
    commands
        .iter()
        .filter(|command| command.kind == kind)
        .map(|command| {
            let title = command.name.trim_start_matches('/').to_string();
            // Catalog rows are slash-prefixed (e.g. "/meta-ads"); the
            // picker title strips the leading "/" but Enter still
            // inserts the full "/name". The MCP server name is the bare
            // form, so keep both so the picker can resolve back to a
            // config entry on activation.
            let mcp_server = if command.kind == "mcp" {
                Some(title.clone())
            } else {
                None
            };
            PickerItem {
                title,
                meta: command.desc.clone(),
                mcp_server,
            }
        })
        .collect()
}

// Last-model persistence (settings.go port).

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct LastModel {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    thinking: String,
}

fn last_model_path() -> std::path::PathBuf {
    atom_core::session::store::data_dir().join("last-model.json")
}

pub(crate) fn load_last_model_thinking() -> Option<String> {
    let b = std::fs::read(last_model_path()).ok()?;
    let lm: LastModel = serde_json::from_slice(&b).ok()?;
    if lm.model.is_empty() {
        return None;
    }
    Some(lm.thinking)
}

pub(crate) fn save_last_model_state(provider_name: &str, model: &str, thinking: &str) {
    if model.is_empty() {
        return;
    }
    let mut lm = LastModel {
        provider: provider_name.to_string(),
        model: model.to_string(),
        thinking: thinking.to_string(),
    };
    if lm.thinking.is_empty() {
        lm.thinking = load_last_model_thinking().unwrap_or_default();
    }
    if let Ok(json) = serde_json::to_string_pretty(&lm) {
        let _ = std::fs::write(last_model_path(), json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::parse_stream_event;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn approval_app() -> App {
        let mut app = App::new_test(90, 30);
        app.approval = Some(ApprovalPrompt {
            id: "req1".into(),
            command: "npm install".into(),
            cwd: "/work".into(),
            rule_id: "pkg-install".into(),
            reason: "package manager with install".into(),
            session_id: "sess1".into(),
            child_title: String::new(),
            from_subagent: false,
        });
        app
    }

    #[test]
    fn approval_keys_map_to_wire_decisions() {
        let cases = [
            // v2 spec: y/a/n/d map to the four decisions. Esc is not
            // bound — the four visible buttons are the only choices.
            (KeyCode::Char('y'), "allow_once"),
            (KeyCode::Char('a'), "allow_always"),
            (KeyCode::Char('n'), "deny_once"),
            (KeyCode::Char('d'), "deny_always"),
        ];
        for (code, wire) in cases {
            let mut app = approval_app();
            let fx = app.key(key(code, KeyModifiers::NONE));
            assert!(app.approval.is_none(), "{wire}: overlay closed");
            assert_eq!(fx.len(), 1);
            match &fx[0] {
                Effect::RespondApproval { sid, id, decision } => {
                    assert_eq!(sid, "sess1");
                    assert_eq!(id, "req1");
                    assert_eq!(decision, wire);
                }
                other => panic!("unexpected effect {other:?}"),
            }
        }
    }

    #[test]
    fn approval_pauses_other_input() {
        let mut app = approval_app();
        // Typing and Enter are swallowed while the prompt is up.
        assert!(app
            .key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_empty());
        assert!(app.input.value.is_empty());
        assert!(app.key(key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        // Ctrl+C still quits.
        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(fx.last(), Some(Effect::Quit)));
    }

    // -- shell mode ----------------------------------------------------------

    #[test]
    fn bang_enters_shell_mode_and_ctrl_c_exits_without_quitting() {
        let mut app = App::new_test(90, 30);
        // `!` on an empty prompt enters shell mode; the bang is not inserted.
        assert!(app
            .key(key(KeyCode::Char('!'), KeyModifiers::NONE))
            .is_empty());
        assert!(app.shell_mode);
        assert!(app.input.value.is_empty());
        // Ctrl+C clears shell mode instead of quitting the app.
        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.shell_mode);
        assert!(!matches!(fx.last(), Some(Effect::Quit)));
        // `!` mid-text in normal mode is an ordinary character.
        let mut app = App::new_test(90, 30);
        app.input.set_value("hi");
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(app.input.value, "hi!");
        assert!(!app.shell_mode);
    }

    #[test]
    fn backspace_out_of_empty_prompt_exits_shell_mode() {
        let mut app = App::new_test(90, 30);
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        assert!(app.shell_mode);
        app.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(!app.shell_mode);
    }

    #[test]
    fn shell_mode_enter_spawns_command_block() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.input.set_value("echo hi");
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        match &fx[..] {
            [Effect::RunShell { cmd, cwd }] => {
                assert_eq!(cmd, "echo hi");
                assert_eq!(cwd, &app.cwd);
            }
            other => panic!("unexpected effects {other:?}"),
        }
        assert!(app.shell_mode, "mode persists after a command");
        assert!(app.shell_running);
        assert!(app.input.value.is_empty());
        let block = app.blocks.last().unwrap();
        assert_eq!(block.kind, BlockKind::Tool);
        assert_eq!(block.tool_name, "shell");
        assert_eq!(block.title, "! echo hi");
        assert!(!block.tool_done);
        // Enter while a command runs is a no-op.
        app.input.set_value("echo again");
        assert!(app.key(key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn shell_done_fills_block_and_cd_moves_the_app() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.cwd = "/work".into();
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.input.set_value("cd /tmp && echo hi");
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(fx[0], Effect::RunShell { .. }));

        let fx = app.handle_msg(AppMsg::ShellDone {
            cmd: "cd /tmp && echo hi".into(),
            cwd: "/work".into(),
            output: "hi\n".into(),
            code: Some(0),
            new_cwd: "/tmp".into(),
        });
        let block = app.blocks.last().unwrap();
        assert!(block.tool_done);
        assert_eq!(block.result, "hi\n");
        assert!(!app.shell_running);
        assert_eq!(app.cwd, "/tmp", "app follows the shell's cd");
        match &fx[..] {
            [Effect::PatchSessionCwd { id, cwd }] => {
                assert_eq!(id, "sess1");
                assert_eq!(cwd, "/tmp");
            }
            other => panic!("unexpected effects {other:?}"),
        }
        // Unchanged cwd patches nothing, even though no block matches.
        let fx = app.handle_msg(AppMsg::ShellDone {
            cmd: "echo again".into(),
            cwd: "/tmp".into(),
            output: String::new(),
            code: Some(0),
            new_cwd: "/tmp".into(),
        });
        assert!(fx.is_empty());
        assert_eq!(app.cwd, "/tmp");
    }

    #[test]
    fn shell_done_reports_nonzero_exit_and_kill() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.input.set_value("false");
        app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_msg(AppMsg::ShellDone {
            cmd: "false".into(),
            cwd: app.cwd.clone(),
            output: String::new(),
            code: Some(1),
            new_cwd: String::new(),
        });
        assert!(app.blocks.last().unwrap().result.contains("exit code 1"));

        // A killed command (Ctrl+C during a run) marks the block killed.
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.input.set_value("sleep 100");
        app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_msg(AppMsg::ShellDone {
            cmd: "sleep 100".into(),
            cwd: app.cwd.clone(),
            output: String::new(),
            code: None,
            new_cwd: String::new(),
        });
        assert!(app.blocks.last().unwrap().result.contains("killed"));
    }

    #[test]
    fn shell_ctrl_c_kills_the_running_command() {
        let mut app = App::new_test(90, 30);
        app.key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.input.set_value("sleep 100");
        app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        app.handle_msg(AppMsg::ShellKillArmed(tx));
        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.shell_mode);
        assert!(!matches!(fx.last(), Some(Effect::Quit)));
        assert!(rx.try_recv().is_ok(), "kill switch fired");
    }

    #[test]
    fn bang_prefix_one_shots_into_shell_mode() {
        let mut app = App::new_test(90, 30);
        app.input.set_value("!echo hi");
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        match &fx[..] {
            [Effect::RunShell { cmd, .. }] => assert_eq!(cmd, "echo hi"),
            other => panic!("unexpected effects {other:?}"),
        }
        assert!(app.shell_mode, "! prefix leaves shell mode enabled");
    }

    // -- subagent read-only views --------------------------------------------

    #[test]
    fn subagent_view_hides_prompt_and_reclaims_rows() {
        let mut parent = App::new_test(90, 30);
        let mut child = App::new_test(90, 30);
        child.session.parent_id = "parent".into();
        assert!(child.read_only_view());
        assert_eq!(child.prompt_height(), 0);
        let pgeo = crate::view::Layout::compute(&parent);
        let cgeo = crate::view::Layout::compute(&child);
        assert!(
            cgeo.viewport_h > pgeo.viewport_h,
            "subagent viewport reclaims the prompt rows: {} vs {}",
            cgeo.viewport_h,
            pgeo.viewport_h
        );
        // The status bar stays bottom-anchored; only the viewport grows.
        assert_eq!(cgeo.status_y, pgeo.status_y);
    }

    #[test]
    fn subagent_view_swallows_input_keys() {
        let mut app = App::new_test(90, 30);
        app.session.parent_id = "parent".into();
        // Typing does nothing — no shell mode either.
        assert!(app
            .key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_empty());
        assert!(app
            .key(key(KeyCode::Char('!'), KeyModifiers::NONE))
            .is_empty());
        assert!(!app.shell_mode);
        assert!(app.input.value.is_empty());
        // Enter sends nothing: no SendTurn, no RunShell.
        app.input.set_value("stale draft");
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            fx.is_empty(),
            "subagent views send nothing on Enter: {fx:?}"
        );
        // Backspace/Delete are swallowed too.
        assert!(app
            .key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .is_empty());
    }

    #[test]
    fn subagent_view_esc_pauses_the_running_turn() {
        let mut app = App::new_test(90, 30);
        app.session.parent_id = "parent".into();
        app.streaming = true;
        let fx = app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(fx.as_slice(), [Effect::PauseTurn]),
            "Esc stops the running child turn: {fx:?}"
        );
        // Esc with no live turn just clears selection.
        app.streaming = false;
        assert!(app.key(key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn subagent_view_esc_pauses_a_detached_turn() {
        let mut app = App::new_test(90, 30);
        app.session.parent_id = "parent".into();
        // A dispatch child never opens a TUI /send stream: streaming stays
        // false while its turn runs; only remote events mark it working.
        app.streaming = false;
        app.remote_working = true;
        // A turn id left over from a previous send must not leak into the
        // pause: a non-matching id would only record a pending pause that
        // never matches the child's "dispatch-<id>" turn.
        app.turn_id = "stale-turn".into();
        let fx = app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(fx.as_slice(), [Effect::PauseTurn]),
            "Esc stops a detached subagent turn: {fx:?}"
        );
        assert!(
            app.turn_id.is_empty(),
            "Esc must pause with an empty turn_id, got {:?}",
            app.turn_id
        );
        // No live remote activity: Esc does nothing.
        app.remote_working = false;
        assert!(app.key(key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn subagent_view_keeps_global_bindings() {
        let mut app = App::new_test(90, 30);
        app.session.parent_id = "parent".into();
        // Shift+Up returns to the parent.
        let fx = app.key(key(KeyCode::Up, KeyModifiers::SHIFT));
        assert!(matches!(
            fx.as_slice(),
            [Effect::LoadSession { id }] if id == "parent"
        ));
        // Ctrl+P still opens the slash menu.
        app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.menu_visible);
    }

    #[test]
    fn subagent_view_refuses_compact() {
        let mut app = App::new_test(90, 30);
        app.session.parent_id = "parent".into();
        app.session.id = "sess1".into();
        let fx = app.handle_input("/compact");
        assert!(fx.is_empty());
        assert!(app.err_msg.contains("managed by their parent"));
    }

    #[test]
    fn duplicate_approval_request_yields_one_card() {
        // The server fans each approval_request out on both the /send
        // stream and the session subscription, and sub_event forwards it
        // even while this client's own stream is live. Both copies reach
        // the app; only one approval card may be added per unique id.
        let mut app = App::new_test(90, 30);
        let ev = parse_stream_event(&serde_json::json!({
            "type": "approval_request",
            "id": "req1",
            "session_id": "sess1",
            "command": "grep -n resvg Cargo.toml",
            "cwd": "/work",
            "rule_id": "grep-search",
            "reason": "grep-search rule",
        }));
        app.handle_stream_event(&ev);
        app.handle_stream_event(&ev);
        let cards = app
            .blocks
            .iter()
            .filter(|b| b.approval.as_ref().is_some_and(|a| a.id == "req1"))
            .count();
        assert_eq!(cards, 1, "one approval card per unique request id");
        assert_eq!(app.approval.as_ref().map(|p| p.id.as_str()), Some("req1"));

        // A different request id still gets its own card.
        let mut ev2 = ev.clone();
        ev2.id = "req2".into();
        app.handle_stream_event(&ev2);
        let cards = app.blocks.iter().filter(|b| b.approval.is_some()).count();
        assert_eq!(cards, 2);
    }

    #[test]
    fn click_on_approval_button_resolves_decision() {
        let mut app = App::new_test(90, 30);
        let request = serde_json::json!({
            "type": "approval_request",
            "id": "req1",
            "session_id": "sess1",
            "command": "npm install",
            "cwd": "/work",
            "rule_id": "pkg-install",
            "reason": "package manager with install",
        });
        app.handle_stream_event(&parse_stream_event(&request));
        app.viewport_dirty = true;
        app.refresh_viewport();

        let bi = app
            .blocks
            .iter()
            .position(|b| b.approval.is_some())
            .expect("approval card block exists");
        let lines = app.blocks[bi].lines.as_ref().expect("block rendered");
        // Button line is the penultimate rendered row (bottom pad is last).
        let row = app.block_start[bi] + lines.len() - 2;

        // Click the middle of "A Always" (button col + box pad cell).
        let btn = &blocks::approval_buttons()[1];
        let em = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (TUI_HPAD + btn.col_start + 2) as u16,
            row: (VIEWPORT_VPAD + row - app.scroll_y) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(em);
        assert!(app.approval.is_none(), "click closed the prompt");
        assert!(
            matches!(
                fx.first(),
                Some(Effect::RespondApproval { decision, .. }) if decision == "allow_always"
            ),
            "click resolved approval: {fx:?}"
        );

        // A click on the gap between buttons resolves nothing.
        let mut app = App::new_test(90, 30);
        app.handle_stream_event(&parse_stream_event(&request));
        app.viewport_dirty = true;
        app.refresh_viewport();
        let bi = app
            .blocks
            .iter()
            .position(|b| b.approval.is_some())
            .unwrap();
        let lines = app.blocks[bi].lines.as_ref().unwrap();
        let row = app.block_start[bi] + lines.len() - 3; // row above the buttons
        let btn = &blocks::approval_buttons()[1];
        let em = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (TUI_HPAD + btn.col_start + 2) as u16,
            row: (VIEWPORT_VPAD + row - app.scroll_y) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(em);
        assert!(fx.is_empty(), "row above buttons is not clickable: {fx:?}");
        assert!(app.approval.is_some(), "prompt stays up on a miss");
    }

    #[test]
    fn tab_and_ctrl_t_cycle_thinking() {
        let mut app = App::new_test(90, 30);
        app.thinking_levels = vec!["none".into(), "low".into(), "high".into()];
        app.thinking_idx = modelsdev::default_thinking_index(&app.thinking_levels);
        let start = app.thinking_idx;
        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.thinking_idx, (start + 1) % 3);
        app.key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.thinking_idx, (start + 2) % 3);
    }

    #[test]
    fn enter_sends_and_esc_pauses() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.input.set_value("hello world");
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.streaming);
        assert!(app
            .blocks
            .last()
            .map(|b| b.kind == BlockKind::User)
            .unwrap_or(false));
        assert!(fx.iter().any(|e| matches!(e, Effect::SendTurn(_))));
        // Esc pauses the live stream.
        let fx = app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.paused);
        assert!(fx.iter().any(|e| matches!(e, Effect::PauseTurn)));
    }

    #[test]
    fn shift_enter_inserts_newline_ctrl_j_too() {
        let mut app = App::new_test(90, 30);
        app.input.set_value("line");
        app.key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(app.input.value, "line\n");
        app.key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value, "line\n\n");
        // Plain enter would send; ensure it did not run here.
        assert!(!app.streaming);
    }

    #[test]
    fn shift_up_closes_subagent_menu_on_parent() {
        let mut app = App::new_test(90, 30);
        app.session.id = "parent".into();

        // Opening the subagent menu on the parent, then Shift+Up closes it.
        let fx = app.key(key(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(app.manage_visible);
        assert!(fx.iter().any(|e| matches!(e, Effect::ListChildren { .. })));
        let fx = app.key(key(KeyCode::Up, KeyModifiers::SHIFT));
        assert!(!app.manage_visible);
        assert!(fx.is_empty());

        // Inside a subagent, Shift+Up still navigates to the parent.
        app.session.parent_id = "parent".into();
        let fx = app.key(key(KeyCode::Up, KeyModifiers::SHIFT));
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { id } if id == "parent")));
    }

    #[test]
    fn menus_capture_only_their_keys() {
        // With the slash menu open, Esc closes it...
        let mut app = App::new_test(90, 30);
        app.input.set_value("/");
        app.after_input_change();
        assert!(app.menu_visible);
        app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.menu_visible);
        // ...and Ctrl+C clears the prompt before it can quit.
        app.input.set_value("/");
        app.after_input_change();
        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(fx.is_empty());
        assert!(app.input.value.is_empty());
        assert!(!app.menu_visible);
        assert!(!app.quitting);
    }

    #[test]
    fn ctrl_p_toggles_slash_menu_without_touching_prompt() {
        let mut app = App::new_test(90, 30);
        app.input.set_value("hello world");
        // Ctrl+P opens the menu, leaving the prompt untouched, and the
        // palette shows every command (sessions ahead of settings).
        let fx = app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value, "hello world");
        assert!(app.menu_visible);
        assert!(fx.is_empty());
        let matches = overlays::match_commands(&app.menu_typed(), &app.slash_commands);
        assert_eq!(matches[0].name, "/new");
        assert_eq!(matches[1].name, "/sessions");
        assert_eq!(matches[2].name, "/fork");
        assert_eq!(matches[3].name, "/settings");
        // Enter runs the highlighted row (/new); the prompt is cleared
        // as with any submitted command.
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input.value, "");
        assert!(!app.menu_visible);
        assert!(fx.iter().any(|e| matches!(e, Effect::CreateSession { .. })));
        // Ctrl+P again closes an open menu without touching the prompt.
        app.input.set_value("hello world");
        app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.menu_visible);
        app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value, "hello world");
        assert!(!app.menu_visible);
    }

    #[test]
    fn reasoning_menu_selects_level_and_closes() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.thinking_levels = vec!["none".into(), "low".into(), "high".into()];
        app.thinking_idx = 1;
        app.thinking_pref = "low".into();
        // /reasoning opens a footer menu, not the full-screen overlay.
        app.handle_input("/reasoning");
        assert!(app.reasoning_visible);
        assert!(app.overlay.is_none());
        assert_eq!(app.reasoning_sel, 1);
        // Down moves to "high"; Enter applies it and closes the menu.
        app.key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.reasoning_sel, 2);
        let fx = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.reasoning_visible);
        assert_eq!(app.thinking_idx, 2);
        assert_eq!(app.thinking_pref, "high");
        assert!(fx.iter().any(|e| matches!(e, Effect::PatchSessionThinking)));
        // Reopen and Esc closes without changing the level.
        app.handle_input("/reasoning");
        assert!(app.reasoning_visible);
        app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.reasoning_visible);
        assert_eq!(app.thinking_idx, 2);
    }

    fn model_entry(provider: &str, model: &str) -> atom_core::providers::providers::ModelEntry {
        atom_core::providers::providers::ModelEntry {
            provider: Provider {
                name: provider.into(),
                id: provider.into(),
                ..Default::default()
            },
            model: model.into(),
        }
    }

    #[test]
    fn model_selection_records_recent_and_ctrl_p_pins() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Model);
        app.overlay_entries = vec![model_entry("openai", "gpt-5")];
        app.overlay_sel = overlays::first_model_row(&app);

        app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.picker_settings.favorites.len(), 1);
        assert_eq!(app.picker_settings.favorites[0].model, "gpt-5");

        let effects = app.confirm_overlay();
        assert_eq!(app.picker_settings.recents.len(), 1);
        assert_eq!(app.picker_settings.recents[0].model, "gpt-5");
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::CreateSession {
                provider,
                model,
                ..
            } if provider == "openai" && model == "gpt-5"
        )));
    }

    #[test]
    fn model_switch_updates_provider_with_model() {
        let mut app = App::new_test(80, 24);
        app.session.id = "session-1".into();
        app.session.provider = "openai".into();
        app.session.model = "gpt-5".into();
        app.overlay = Some(OverlayKind::Model);
        app.overlay_entries = vec![model_entry("ollama", "deepseek-v4-flash:0731")];
        app.overlay_sel = overlays::first_model_row(&app);

        let effects = app.confirm_overlay();

        assert_eq!(app.session.provider, "ollama");
        assert_eq!(app.session.model, "deepseek-v4-flash:0731");
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::PatchSessionModel {
                provider,
                model,
                ..
            } if provider == "ollama" && model == "deepseek-v4-flash:0731"
        )));
    }

    #[test]
    fn settings_compaction_picker_saves_without_switching_chat_model() {
        let mut app = App::new_test(80, 24);
        app.sel_model = "chat-model".into();
        app.overlay = Some(OverlayKind::Settings);
        app.overlay_sel = 0;
        let effects = app.confirm_overlay();
        assert_eq!(app.overlay, Some(OverlayKind::Model));
        assert_eq!(
            app.model_picker_purpose,
            overlays::ModelPickerPurpose::Compaction
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchModels)));

        app.overlay_entries = vec![model_entry("openai", "compact-model")];
        app.overlay_sel = overlays::first_model_row(&app);
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::Settings));
        assert_eq!(app.sel_model, "chat-model");
        assert_eq!(
            app.atom_config.compaction,
            Some(atom_core::config::CompactionConfig {
                provider: "openai".into(),
                model: "compact-model".into(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn settings_toggle_disables_and_reenables_auto_compaction() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Settings);
        app.overlay_sel = 1;
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::Settings));
        assert_eq!(
            app.atom_config.compaction,
            Some(atom_core::config::CompactionConfig {
                enabled: Some(false),
                ..Default::default()
            })
        );

        app.confirm_overlay();
        assert!(app.atom_config.compaction.unwrap().resolved_enabled());

        // Picking a compaction model keeps the disabled flag.
        app.atom_config.compaction = Some(atom_core::config::CompactionConfig {
            enabled: Some(false),
            ..Default::default()
        });
        app.model_picker_purpose = overlays::ModelPickerPurpose::Compaction;
        app.overlay = Some(OverlayKind::Model);
        app.overlay_entries = vec![model_entry("openai", "compact-model")];
        app.overlay_sel = overlays::first_model_row(&app);
        assert!(app.confirm_overlay().is_empty());
        let compaction = app.atom_config.compaction.unwrap();
        assert_eq!(compaction.model, "compact-model");
        assert!(!compaction.resolved_enabled());
    }

    #[test]
    fn settings_web_search_picker_saves_bundled_tool() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Settings);
        app.overlay_sel = 2;
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::WebSearch));

        let rows = overlays::web_search_rows(&app);
        app.overlay_sel = rows.iter().position(|row| row.0 == "exa").unwrap();
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::Settings));
        assert_eq!(
            app.atom_config.web_search,
            Some(atom_core::config::WebSearchConfig {
                server: "exa".into(),
                tool: "web_search_exa".into(),
            })
        );
    }

    #[test]
    fn settings_web_fetch_picker_saves_bundled_tool() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Settings);
        app.overlay_sel = 3;
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::WebFetch));

        let rows = overlays::web_fetch_rows(&app);
        app.overlay_sel = rows.iter().position(|row| row.0 == "exa").unwrap();
        assert!(app.confirm_overlay().is_empty());
        assert_eq!(app.overlay, Some(OverlayKind::Settings));
        assert_eq!(
            app.atom_config.web_fetch,
            Some(atom_core::config::WebFetchConfig {
                server: "exa".into(),
                tool: "web_fetch".into(),
            })
        );
    }

    #[test]
    fn onboarding_escape_accepts_and_persists_defaults_in_memory() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Settings);
        app.settings_onboarding = true;
        assert!(app.key(key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
        assert!(app.overlay.is_none());
        assert!(!app.settings_onboarding);
        assert!(app.atom_config.setup_complete());
    }

    #[test]
    fn session_ctrl_p_pins_selected_session() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Session);
        app.overlay_sessions = vec![SessionInfo {
            id: "s1".into(),
            title: "Pinned chat".into(),
            ..empty_session_info()
        }];
        app.overlay_sel = overlays::first_session_row(&app);

        app.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));

        assert_eq!(app.picker_settings.pinned_sessions, vec!["s1"]);
        let rows = overlays::session_rows(&app);
        assert_eq!(rows[0].label, "Pinned");
        assert_eq!(rows[1].sess.as_ref().unwrap().id, "s1");
    }

    #[test]
    fn provider_add_opens_prefilled_model_picker() {
        let mut app = App::new_test(80, 24);
        app.open_models_for_provider("ollama-cloud");
        let effects = app.providers_rebuilt(vec![Provider {
            name: "ollama".into(),
            id: "ollama-cloud".into(),
            ..Default::default()
        }]);

        assert_eq!(app.overlay, Some(OverlayKind::Model));
        assert_eq!(app.overlay_q, "ollama");
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchModels)));

        app.handle_msg(AppMsg::ModelsLoaded(vec![model_entry("ollama", "qwen3")]));
        assert_eq!(
            app.overlay_q, "ollama",
            "model loading keeps provider filter"
        );
    }

    #[test]
    fn ctrl_c_clears_nonempty_prompt_then_quits() {
        let mut app = App::new_test(90, 30);
        app.input.set_value("draft");
        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(fx.is_empty());
        assert!(app.input.value.is_empty());
        assert!(!app.quitting);

        let fx = app.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quitting);
        assert!(fx.iter().any(|e| matches!(e, Effect::Quit)));

        let mut app = App::new_test(90, 30);
        let fx = app.key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(fx.iter().any(|e| matches!(e, Effect::Quit)));
    }

    #[test]
    fn click_toggles_tool_collapse() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hi".into(),
            ..Default::default()
        });
        let long: String = "row line\n".repeat(20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Bash".into(),
            tool_name: "bash".into(),
            text: "ls".into(),
            result: long,
            ..Default::default()
        });
        app.refresh_viewport();
        // The tool block starts collapsed at the top of the viewport.
        assert!(!app.blocks[1].expanded);
        // Click inside the tool block's first row.
        let y = VIEWPORT_VPAD + app.block_start[1] - app.scroll_y;
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: y as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert!(app.blocks[1].expanded, "click expanded the block");
        // And clicking again collapses it.
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: y as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert!(!app.blocks[1].expanded, "second click collapsed the block");
    }

    #[test]
    fn click_on_subagent_indicator_opens_manage_menu() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(100, 40);
        app.session.id = "sess-1".into();
        app.manage_agents = vec![empty_session_info()];
        app.refresh_viewport();
        let geo = crate::view::Layout::compute(&app);
        let regions = crate::statusbar::nav_hit_regions(&app);
        assert_eq!(regions.len(), 1, "subagent indicator is clickable");
        let (row, c0, _, action) = regions[0];
        assert_eq!(action, crate::statusbar::NavAction::OpenSubagents);
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (TUI_HPAD + c0) as u16,
            row: (geo.status_y + row) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let effects = app.mouse(ev);
        assert!(app.manage_visible, "click opened the subagent menu");
        assert!(
            matches!(effects.first(), Some(Effect::ListChildren { .. })),
            "click listed children: {effects:?}"
        );
    }

    #[test]
    fn click_on_return_hint_loads_parent_session() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(100, 40);
        app.session.parent_id = "parent-1".into();
        app.refresh_viewport();
        let geo = crate::view::Layout::compute(&app);
        let regions = crate::statusbar::nav_hit_regions(&app);
        let (row, c0, _, action) = regions[0];
        assert_eq!(action, crate::statusbar::NavAction::ReturnToParent);
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (TUI_HPAD + c0) as u16,
            row: (geo.status_y + row) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let effects = app.mouse(ev);
        assert!(
            matches!(
                effects.first(),
                Some(Effect::LoadSession { id }) if id == "parent-1"
            ),
            "click returned to the parent: {effects:?}"
        );
    }

    #[test]
    fn click_on_overlay_esc_hint_closes_overlay() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(100, 40);
        app.open_overlay(OverlayKind::Settings);
        assert!(app.overlay.is_some());
        // The `esc` hint sits on the title row's last three columns, at
        // raw y = EDGE_PAD (content y 0).
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (app.width - 2) as u16,
            row: crate::fullscreen_view::EDGE_PAD as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert!(
            app.overlay.is_none(),
            "clicking the esc hint dismissed the overlay"
        );
    }

    #[test]
    fn stats_overlay_scroll_tracks_arrow_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new_test(100, 30);
        app.open_overlay(OverlayKind::Stats);
        // Enough tool rows that the report overflows a short terminal.
        let mut tool_usage = std::collections::HashMap::new();
        for i in 0..30 {
            tool_usage.insert(format!("tool_{i}"), (30 - i) as i64);
        }
        app.overlay_stats = Some(StatsReport {
            total_sessions: 1,
            tool_usage,
            ..Default::default()
        });
        app.overlay_sel = 0;
        app.overlay_scroll = 0;
        assert!(
            overlays::stats_scroll_max(&app) > 5,
            "report should overflow"
        );
        for _ in 0..3 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        }
        assert_eq!(app.overlay_scroll, 3, "scroll follows the Down key");
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.overlay_scroll, 2, "scroll follows the Up key");
        // Clamped at the last scrollable row.
        for _ in 0..100 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        }
        assert_eq!(
            app.overlay_scroll,
            overlays::stats_scroll_max(&app),
            "scroll clamps at max"
        );
    }

    #[test]
    fn wheel_scrolls_footer_menu_selection() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(100, 40);
        app.session.id = "sess-1".into();
        app.manage_agents = (0..5).map(|_| empty_session_info()).collect();
        let _ = app.open_manage_menu();
        assert_eq!(app.manage_sel, 0);
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert_eq!(app.manage_sel, 1, "wheel down moved the menu selection");
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert_eq!(app.manage_sel, 0, "wheel up moved back");
        // With the menu closed the wheel no longer touches the selection.
        app.dismiss_manage_menu();
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert_eq!(app.manage_sel, 0);
    }

    #[test]
    fn wheel_scrolls_modal_overlay_selection() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(100, 40);
        app.overlay = Some(OverlayKind::Settings);
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert_eq!(app.overlay_sel, 1, "wheel down moved the overlay selection");
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert_eq!(app.overlay_sel, 0, "wheel up moved back");
    }

    #[test]
    fn click_anywhere_on_diagram_block_opens_viewer() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Visualize".into(),
            tool_name: "visualize".into(),
            tool_done: true,
            result:
                "rendered diagram\n [atom-diagram] png=/a.png html=/a.html width=400 height=200"
                    .into(),
            diagram: Some(crate::blocks::DiagramRef {
                png: "/a.png".into(),
                html: "file:///a.html".into(),
                w: 400,
                h: 200,
                ..Default::default()
            }),
            ..Default::default()
        });
        app.refresh_viewport();

        // The whole card is the click target. Header row: block start + 1
        // (offset 0 is the top pad row).
        let y = (VIEWPORT_VPAD + app.block_start[0] + 1 - app.scroll_y) as u16;
        let inner = app.inner_width().saturating_sub(2).max(1);
        let x_hint = (TUI_HPAD + inner) as u16; // last text column
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_hint,
            row: y,
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(ev);
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::OpenLink { uri } if uri == "file:///a.html")),
            "hint click must open the viewer: {fx:?}"
        );

        // A click on the left side of the same header row also opens.
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: y,
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(ev);
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::OpenLink { uri } if uri == "file:///a.html")),
            "header click outside the hint must also open: {fx:?}"
        );

        // A click on a body row (the inline image grid) opens too.
        let lines = app.blocks[0].lines.as_ref().expect("block rendered");
        let y_body = (VIEWPORT_VPAD + app.block_start[0] + lines.len().saturating_sub(1)
            - app.scroll_y) as u16;
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_hint,
            row: y_body,
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(ev);
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::OpenLink { uri } if uri == "file:///a.html")),
            "body click must open the viewer: {fx:?}"
        );

        // A click strictly above the card does nothing.
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_hint,
            row: y.saturating_sub(2),
            modifiers: KeyModifiers::empty(),
        };
        let fx = app.mouse(ev);
        assert!(!fx.iter().any(|e| matches!(e, Effect::OpenLink { .. })));
    }

    #[test]
    fn click_toggles_user_collapse() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        let long: String = "word ".repeat(400);
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: long,
            ..Default::default()
        });
        app.refresh_viewport();
        // Long cards start collapsed to the preview budget.
        assert!(!app.blocks[0].expanded);
        assert!(
            app.blocks[0].user_collapsible(app.inner_width().saturating_sub(2).max(1), &app.cwd)
        );
        assert_eq!(app.content_lines.len(), blocks::USER_PREVIEW_LINES + 2);

        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: (VIEWPORT_VPAD + row) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(click(1));
        assert!(app.blocks[0].expanded, "click expanded the user card");
        assert!(
            app.content_lines.len() > blocks::USER_PREVIEW_LINES + 2,
            "expanded card shows the full message"
        );
        // And clicking again collapses it.
        let _ = app.mouse(click(1));
        assert!(!app.blocks[0].expanded, "second click collapsed the card");
        assert_eq!(app.content_lines.len(), blocks::USER_PREVIEW_LINES + 2);
    }

    #[test]
    fn click_on_link_opens_and_drag_selects() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        app.blocks.push(Block {
            kind: BlockKind::Assistant,
            text: "see [the docs](https://docs.example.com) for more".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let line_idx = app
            .content_lines
            .iter()
            .position(|l| crate::ansi::line_plain(l).contains("the docs"))
            .expect("label in content lines");
        let col = crate::ansi::line_plain(&app.content_lines[line_idx])
            .find("the docs")
            .unwrap() as u16;
        assert_eq!(
            app.link_lines.len(),
            app.content_lines.len(),
            "link table stays parallel to content lines"
        );

        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: TUI_HPAD as u16 + col,
            row: (VIEWPORT_VPAD + line_idx - app.scroll_y) as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(press);
        assert_eq!(
            app.link_pending.as_deref(),
            Some("https://docs.example.com")
        );
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        };
        let effects = app.mouse(release);
        let opened = effects.iter().find_map(|e| match e {
            Effect::OpenLink { uri } => Some(uri.clone()),
            _ => None,
        });
        assert_eq!(opened.as_deref(), Some("https://docs.example.com"));
        assert!(app.link_pending.is_none());

        // Press then drag: a selection, not an open.
        let _ = app.mouse(press);
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: press.column + 6,
            ..press
        };
        let _ = app.mouse(drag);
        assert!(app.link_pending.is_none());
        assert!(app.sel_active);
    }

    #[test]
    fn click_toggles_only_the_reasoning_header() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        app.blocks.push(Block {
            kind: BlockKind::Reasoning,
            text: "first detail\nsecond detail".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let header_y = VIEWPORT_VPAD + app.block_start[0] - app.scroll_y;
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: (TUI_HPAD + 2) as u16,
            row: row as u16,
            modifiers: KeyModifiers::empty(),
        };

        let _ = app.mouse(click(header_y));
        assert!(app.blocks[0].expanded, "header click expands reasoning");

        let _ = app.mouse(click(header_y + 1));
        assert!(
            app.blocks[0].expanded,
            "body click does not collapse reasoning"
        );

        let _ = app.mouse(click(header_y));
        assert!(
            !app.blocks[0].expanded,
            "second header click collapses reasoning"
        );
    }

    #[test]
    fn click_on_prompt_text_repositions_cursor() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(80, 40);
        app.input.set_value("hello world");

        let geo = crate::view::Layout::compute(&app);
        let text_row = geo.prompt_top_y + PROMPT_PAD;
        let click = |col: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        };

        // Click on the 'w' (cell 6) of the first editable row.
        let x = (TUI_HPAD + PROMPT_PAD + 6) as u16;
        let _ = app.mouse(click(x, text_row as u16));
        assert_eq!(app.input.cursor, 6, "click on 'w' placed the cursor there");

        // A click in the top padding row must NOT reposition the cursor.
        let _ = app.mouse(click(x, geo.prompt_top_y as u16));
        assert_eq!(app.input.cursor, 6, "top padding row keeps the cursor put");

        // A click in the bottom padding row (inside the box, below the text)
        // must also NOT reposition the cursor.
        let bottom_pad = (geo.prompt_top_y + PROMPT_PAD + app.input_height()) as u16;
        let _ = app.mouse(click(x, bottom_pad));
        assert_eq!(
            app.input.cursor, 6,
            "bottom padding row keeps the cursor put"
        );
    }

    #[test]
    fn click_on_prompt_wrapped_row_respects_scroll() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new_test(50, 20);
        let long = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.input.set_value(&long);

        // Simulate an internal scroll so the last logical line is at the top
        // of the field (as `view()` would after pinning the cursor there).
        app.input.scroll_y = app.input.content_lines(app.input_width()) - 1;

        let geo = crate::view::Layout::compute(&app);
        let text_row = geo.prompt_top_y + PROMPT_PAD;
        // Click column 0 on the first visible (scrolled) text row.
        let x = (TUI_HPAD + PROMPT_PAD) as u16;
        let y = text_row as u16;
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        let expected = long.find("line19").unwrap();
        assert_eq!(
            app.input.cursor, expected,
            "scrolled click lands on the start of line19"
        );
    }

    #[test]
    fn usage_event_updates_session_status_data() {
        let mut app = App::new_test(90, 30);
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"usage","prompt":"12345","completion":"678","total":"13023",
                "cache_read":"9000","cache_write":"2100","prompt_all":"120000"}"#,
        )
        .unwrap();
        let _ = app.handle_stream_event(&parse_stream_event(&v));
        let u = app.session.usage.expect("usage stored");
        assert_eq!(u.total_tokens, 13023);
        assert_eq!(u.prompt_tokens_all, 120000);
    }

    #[test]
    fn done_event_attaches_model_and_duration_to_assistant_reply() {
        let mut app = App::new_test(90, 30);
        app.blocks.push(Block {
            kind: BlockKind::Assistant,
            text: "answer".into(),
            ..Default::default()
        });
        let event = parse_stream_event(&serde_json::json!({
            "type": "done",
            "duration_ms": 134_600,
            "model": "model-b",
        }));

        app.handle_stream_event(&event);

        let reply = app.blocks.last().unwrap();
        assert_eq!(reply.model, "model-b");
        assert_eq!(reply.turn_duration, Some(Duration::from_millis(134_600)));
    }

    #[test]
    fn empty_content_event_does_not_create_an_assistant_block() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Grep".into(),
            tool_name: "grep".into(),
            tool_done: true,
            ..Default::default()
        });

        app.handle_stream_event(&StreamEvent {
            event_type: "content".into(),
            ..Default::default()
        });

        assert_eq!(app.blocks.len(), 1);
    }

    #[test]
    fn loading_dispatched_child_selects_its_provider() {
        let mut app = App::new_test(80, 20);
        app.providers = vec![
            Provider {
                name: "ollama".into(),
                base_url: "https://ollama.com/v1".into(),
                ..Default::default()
            },
            Provider {
                name: "opencode-go".into(),
                base_url: "https://opencode.ai/zen/go/v1".into(),
                key: "go-key".into(),
                reasoning_field: "reasoning_content".into(),
                ..Default::default()
            },
        ];
        app.sel_provider = app.providers[0].clone();
        let session = atom_core::session::store::Session {
            id: "child".into(),
            model: "ox-alpha-free".into(),
            provider: "opencode-go".into(),
            ..Default::default()
        };

        app.session_loaded(session);

        assert_eq!(app.sel_provider.name, "opencode-go");
        assert_eq!(app.sel_provider.key, "go-key");
        assert_eq!(app.sel_model, "ox-alpha-free");

        app.session_loaded(atom_core::session::store::Session {
            id: "parent".into(),
            model: "glm-5.2".into(),
            messages: vec![atom_core::types::Message {
                role: "assistant".into(),
                provider: "ollama".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(app.sel_provider.name, "ollama");
    }

    #[test]
    fn loading_same_sized_session_rebuilds_cached_viewport() {
        fn session(id: &str, user: &str, assistant: &str) -> atom_core::session::store::Session {
            atom_core::session::store::Session {
                id: id.into(),
                messages: vec![
                    atom_core::types::Message {
                        role: "user".into(),
                        content: user.into(),
                        ..Default::default()
                    },
                    atom_core::types::Message {
                        role: "assistant".into(),
                        content: assistant.into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }
        }
        fn viewport_text(app: &App) -> String {
            app.content_lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .join(" ")
        }

        let mut app = App::new_test(80, 20);
        app.session_loaded(session("a", "first user", "first answer"));
        assert!(viewport_text(&app).contains("first answer"));

        app.session_loaded(session("b", "second user", "second answer"));
        let text = viewport_text(&app);
        assert!(text.contains("second answer"));
        assert!(!text.contains("first answer"));
    }

    #[test]
    fn tool_result_invalidates_cached_viewport() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            text: "file".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        assert!(!app.viewport_dirty);

        app.handle_stream_event(&StreamEvent {
            event_type: "tool_result".into(),
            text: "new result".into(),
            ..Default::default()
        });

        assert!(app.viewport_dirty);
        app.refresh_viewport();
        assert!(app.content_lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("new result"))
        }));
    }

    #[test]
    fn visualize_tool_result_schedules_inline_preview_paint() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Visualize".into(),
            tool_name: "visualize".into(),
            text: "Architecture".into(),
            ..Default::default()
        });

        let fx = app.handle_stream_event(&StreamEvent {
            event_type: "tool_result".into(),
            text: "rendered diagram\n[atom-diagram] png=/a.png png-dark=/a-dark.png html=/a.html width=400 height=200".into(),
            ..Default::default()
        });

        let diagram = app.blocks[0].diagram.as_ref().expect("diagram attached");
        assert!(diagram.id >= crate::blocks::MIN_KITTY_DIAGRAM_ID);
        assert!(app.preview_dirty);
        assert!(fx
            .iter()
            .any(|effect| matches!(effect, Effect::PaintPreviews)));
    }

    #[test]
    fn resizing_a_diagram_schedules_a_matching_kitty_placement() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Visualize".into(),
            tool_name: "visualize".into(),
            tool_done: true,
            diagram: Some(crate::blocks::DiagramRef {
                png: "/a.png".into(),
                png_dark: "/a-dark.png".into(),
                html: "/a.html".into(),
                w: 400,
                h: 200,
                id: crate::blocks::MIN_KITTY_DIAGRAM_ID,
                ..Default::default()
            }),
            ..Default::default()
        });
        app.preview_dirty = false;

        let fx = app.handle_msg(AppMsg::Resize(120, 20));

        assert!(app.preview_dirty);
        assert!(fx
            .iter()
            .any(|effect| matches!(effect, Effect::PaintPreviews)));
    }

    #[test]
    fn round_start_shows_live_thinking_before_first_token() {
        let mut app = App::new_test(80, 20);

        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });

        assert_eq!(app.blocks.len(), 1);
        let b = &app.blocks[0];
        assert_eq!(b.kind, BlockKind::Reasoning);
        assert!(b.active, "placeholder must render the live Thinking label");
    }

    #[test]
    fn round_start_reuses_an_already_active_reasoning_block() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "reasoning".into(),
            text: "partial thought".into(),
            ..Default::default()
        });
        // Second model round while reasoning is still streaming.
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });

        assert_eq!(app.blocks.len(), 1, "must not stack a second placeholder");
        assert_eq!(app.blocks[0].text, "partial thought");
    }

    #[test]
    fn compaction_events_carry_model_onto_the_block() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "compaction".into(),
            model: "deepseek-v4-flash:0731".into(),
            ..Default::default()
        });
        assert_eq!(app.blocks.len(), 1);
        assert_eq!(app.blocks[0].kind, BlockKind::Compaction);
        assert_eq!(app.blocks[0].model, "deepseek-v4-flash:0731");
        assert!(app.blocks[0].active);

        app.handle_stream_event(&StreamEvent {
            event_type: "compaction_end".into(),
            model: "deepseek-v4-flash:0731".into(),
            text: "brief".into(),
            ..Default::default()
        });
        assert!(!app.blocks[0].active);
        assert_eq!(app.blocks[0].model, "deepseek-v4-flash:0731");
        assert_eq!(app.blocks[0].text, "brief");
    }

    #[test]
    fn placeholder_is_dropped_when_reply_has_no_reasoning() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });

        app.handle_stream_event(&StreamEvent {
            event_type: "content".into(),
            text: "just an answer".into(),
            ..Default::default()
        });

        assert_eq!(
            app.blocks.len(),
            1,
            "empty placeholder must not linger above pure content"
        );
        assert_eq!(app.blocks[0].kind, BlockKind::Assistant);
    }

    #[test]
    fn placeholder_becomes_reasoning_block_on_first_delta() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "reasoning".into(),
            text: "thinking...".into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "content".into(),
            text: "answer".into(),
            ..Default::default()
        });

        let b = &app.blocks[0];
        assert_eq!(b.kind, BlockKind::Reasoning);
        assert!(!b.active, "finalized with a duration");
        assert_eq!(b.text, "thinking...");
        assert_eq!(app.blocks[1].kind, BlockKind::Assistant);
    }

    #[test]
    fn tool_pending_after_content_restores_live_thinking() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "content".into(),
            text: "Retrying with low.".into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "tool_pending".into(),
            ..Default::default()
        });

        let pending = app.blocks.last().unwrap();
        assert_eq!(pending.kind, BlockKind::Reasoning);
        assert!(pending.active);

        app.handle_stream_event(&StreamEvent {
            event_type: "tool".into(),
            name: "dispatch".into(),
            arguments: r#"{"tasks":[]}"#.into(),
            ..Default::default()
        });
        assert_eq!(app.blocks.last().unwrap().kind, BlockKind::Tool);
        assert!(!app
            .blocks
            .iter()
            .any(|block| block.kind == BlockKind::Reasoning && block.active));
    }

    #[test]
    fn round_start_after_tool_closes_the_dead_zone() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "tool".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            ..Default::default()
        });
        app.handle_stream_event(&StreamEvent {
            event_type: "tool_result".into(),
            text: "file.rs".into(),
            ..Default::default()
        });

        // Before the next round starts the transcript looks finished;
        // round_start must bring back a live indicator.
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });

        let last = app.blocks.last().unwrap();
        assert_eq!(last.kind, BlockKind::Reasoning);
        assert!(last.active);
    }

    #[test]
    fn done_clears_a_still_active_placeholder() {
        let mut app = App::new_test(80, 20);
        app.handle_stream_event(&StreamEvent {
            event_type: "round_start".into(),
            ..Default::default()
        });

        app.handle_stream_event(&StreamEvent {
            event_type: "done".into(),
            ..Default::default()
        });

        assert!(
            app.blocks.is_empty(),
            "placeholder must not survive past done"
        );
    }

    #[test]
    fn scroll_detaches_and_bottom_reattaches_following() {
        let mut app = App::new_test(80, 20);
        for i in 0..50 {
            app.blocks.push(Block {
                kind: BlockKind::Assistant,
                text: format!("paragraph {i} with some content to wrap around a bit"),
                ..Default::default()
            });
        }
        app.following = true;
        app.refresh_viewport();
        assert!(app.following);
        app.scroll_viewport(-10);
        assert!(!app.following, "scrolling up detaches from the tail");
        let max = app
            .content_lines
            .len()
            .saturating_sub(app.viewport_height());
        app.scroll_y = max;
        app.scroll_viewport(1);
        assert!(app.following, "reaching the bottom re-attaches");
    }

    #[test]
    fn rebuild_shares_lines_between_block_and_content_caches() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "shared allocation".into(),
            ..Default::default()
        });

        app.refresh_viewport();

        let block_lines = app.blocks[0].lines.as_ref().expect("block cache built");
        assert_eq!(block_lines.len(), app.content_lines.len());
        assert!(block_lines
            .iter()
            .zip(&app.content_lines)
            .all(|(block, content)| Arc::ptr_eq(block, content)));
    }

    #[test]
    fn viewport_owns_uniform_block_spacing() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "first".into(),
            ..Default::default()
        });
        app.blocks.push(Block {
            kind: BlockKind::Assistant,
            text: "response".into(),
            ..Default::default()
        });
        app.blocks.push(Block {
            kind: BlockKind::Compaction,
            active: true,
            ..Default::default()
        });
        app.blocks.push(Block {
            kind: BlockKind::Assistant,
            ..Default::default()
        });
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            tool_name: "read_file".into(),
            text: "src/main.rs".into(),
            tool_done: true,
            ..Default::default()
        });

        app.refresh_viewport();

        let visible_starts: Vec<_> = app
            .blocks
            .iter()
            .zip(&app.block_start)
            .filter(|(block, _)| block.lines.as_ref().is_some_and(|lines| !lines.is_empty()))
            .map(|(_, start)| *start)
            .collect();
        for start in visible_starts.iter().skip(1).copied() {
            assert!(app.content_lines[start - 1].spans.is_empty());
            assert!(
                !app.content_lines[start - 2].spans.is_empty(),
                "exactly one separator row before block at {start}"
            );
        }
        let tool = app.block_start[4];
        assert_eq!(
            ansi_line_width(&app.content_lines[tool]),
            app.content_width,
            "tool card starts immediately after the viewport gap"
        );
        assert_eq!(
            app.content_lines[tool].spans[0].style.bg,
            Some(crate::ansi::c_card_dark())
        );

        app.blocks[4].title = "Updated".into();
        app.blocks[4].lines = None;
        app.rebuild_content_from(4, app.content_width);
        let tool = app.block_start[4];
        assert!(app.content_lines[tool - 1].spans.is_empty());
        assert!(!app.content_lines[tool - 2].spans.is_empty());
    }

    #[test]
    fn slash_menu_visibility_follows_input() {
        let mut app = App::new_test(90, 30);
        app.input.set_value("/mo");
        app.after_input_change();
        assert!(app.menu_visible);
        app.input.set_value("hello /mo");
        app.after_input_change();
        assert!(!app.menu_visible);
    }

    #[test]
    fn subagents_command_opens_and_refreshes_empty_menu() {
        let mut app = App::new_test(90, 30);
        app.session.id = "parent".into();
        app.input.set_value("/subagents");

        let effects = app.handle_input("/subagents");

        assert!(app.manage_visible);
        assert!(app.input.value.is_empty());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::ListChildren { id } if id == "parent"
        )));
        app.handle_msg(AppMsg::ChildrenLoaded {
            id: "parent".into(),
            agents: Vec::new(),
        });
        assert!(
            app.manage_visible,
            "explicit empty menu should stay visible"
        );
    }

    #[test]
    fn new_session_clears_stale_subagent_menu() {
        let mut app = App::new_test(90, 30);
        app.session.id = "parent".into();
        app.manage_visible = true;
        app.manage_sel = 1;
        app.manage_agents = vec![SessionInfo {
            id: "child".into(),
            parent_id: "parent".into(),
            ..empty_session_info()
        }];
        app.manage_sticky.insert("parent".into(), true);

        let mut fresh = empty_session_info();
        fresh.id = "fresh".into();
        app.handle_msg(AppMsg::CreatedSession(Box::new(fresh)));

        assert_eq!(app.session.id, "fresh");
        assert!(!app.manage_visible);
        assert!(app.manage_agents.is_empty());
        assert_eq!(app.manage_sel, 0);
    }

    #[test]
    fn stale_children_response_is_ignored_after_session_switch() {
        let mut app = App::new_test(90, 30);
        app.session.id = "fresh".into();
        app.handle_msg(AppMsg::ChildrenLoaded {
            id: "old".into(),
            agents: vec![SessionInfo {
                id: "old-child".into(),
                ..empty_session_info()
            }],
        });
        assert!(app.manage_agents.is_empty());
    }

    #[test]
    fn enter_during_stream_pauses_and_sends_interruption() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.sel_provider.base_url = "http://test:11434/v1".into();
        app.streaming = true;
        app.turn_id = "turn-1".into();

        // While streaming, typing "interrupt" and pressing Enter pauses
        // the running turn and sends the interruption in one effect; the
        // prompt is cleared immediately so the next draft can start.
        app.input.set_value("interrupt");
        let text = app.input.value.trim().to_string();
        let fx = app.handle_input(&text);

        // The message is not stored in the App: it rides in the effect,
        // and the input is already free for the next draft.
        assert!(app.paused);
        assert!(app.streaming);
        assert!(app.interrupting);
        assert!(app.input.value.is_empty());
        let last = app.blocks.last().expect("user block pushed");
        assert_eq!(last.kind, BlockKind::User);
        assert_eq!(last.text, "interrupt");
        let (pause_turn_id, req) = fx
            .iter()
            .find_map(|e| match e {
                Effect::InterruptTurn { pause_turn_id, req } => {
                    Some((pause_turn_id.as_str(), req.message.as_str()))
                }
                _ => None,
            })
            .expect("InterruptTurn effect");
        // Pause targets the turn that was streaming; the request carries
        // the fresh turn id for the interruption itself.
        assert_eq!(pause_turn_id, "turn-1");
        assert_eq!(req, "interrupt");
        assert!(fx.iter().any(|e| matches!(e, Effect::PaintPreviews)));
    }

    #[test]
    fn send_closed_after_pause_finalizes_without_resend() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.streaming = true;
        app.paused = true;

        // A plain pause (Esc) closes the stream; SendClosed must not
        // invent a message to send.
        let fx = app.handle_msg(AppMsg::SendClosed);

        assert!(!app.streaming);
        assert!(!fx.iter().any(|e| matches!(e, Effect::SendTurn(_))));
    }

    #[test]
    fn send_started_resumes_streaming_after_interrupt() {
        // The old stream can close before the interruption's stream is
        // dialed (SendClosed first). SendStarted must bring the app back
        // to a live, unpaused turn.
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.streaming = true;
        app.paused = true;
        app.handle_msg(AppMsg::SendClosed);
        assert!(!app.streaming);

        let fx = app.handle_msg(AppMsg::SendStarted {
            sid: "sess1".into(),
        });

        assert!(app.streaming);
        assert!(!app.paused);
        assert!(fx.is_empty());
    }

    #[test]
    fn user_message_from_other_client_appends_block() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        // Not streaming: this client is only viewing the session.
        app.streaming = false;

        let fx = app.handle_msg(AppMsg::SubEvent(serde_json::json!({
            "type": "user_message",
            "text": "from another client",
        })));

        assert!(fx.is_empty());
        let last = app.blocks.last().unwrap();
        assert_eq!(last.kind, BlockKind::User);
        assert_eq!(last.text, "from another client");
    }

    #[test]
    fn user_message_while_live_is_skipped() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        // This client is streaming its own turn, so the echo of the
        // message it just sent must not append a duplicate block.
        app.streaming = true;

        let fx = app.handle_msg(AppMsg::SubEvent(serde_json::json!({
            "type": "user_message",
            "text": "our own message",
        })));

        assert!(fx.is_empty());
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn stale_saved_during_interrupt_does_not_reload() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.sel_provider.base_url = "http://test:11434/v1".into();
        app.streaming = true;
        app.turn_id = "turn-1".into();

        // Mid-stream submit arms the interruption guard.
        app.input.set_value("interrupt");
        let text = app.input.value.trim().to_string();
        let fx = app.handle_input(&text);
        assert!(app.interrupting);
        assert!(fx.iter().any(|e| matches!(e, Effect::InterruptTurn { .. })));
        assert_eq!(
            app.blocks.last().map(|b| b.text.as_str()),
            Some("interrupt")
        );

        // The paused turn persists and broadcasts "saved" while the
        // interruption is still dialing: it must not arm a reload.
        let fx = app.handle_msg(AppMsg::SubEvent(serde_json::json!({"type": "saved"})));
        assert!(fx.is_empty());
        assert!(!app.pending_saved);

        // The old stream closing must not reload either: the server
        // transcript doesn't contain the interruption message yet, so a
        // reload would drop the user block from the view.
        let fx = app.handle_msg(AppMsg::SendClosed);
        assert!(!app.streaming);
        assert!(!fx.iter().any(|e| matches!(e, Effect::LoadSession { .. })));
        assert_eq!(
            app.blocks.last().map(|b| b.text.as_str()),
            Some("interrupt")
        );
    }

    #[test]
    fn interrupt_guard_clears_when_stream_starts() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.streaming = true;
        app.turn_id = "turn-1".into();
        app.input.set_value("interrupt");
        let text = app.input.value.trim().to_string();
        app.handle_input(&text);
        assert!(app.interrupting);

        // The interruption's stream is live: the guard clears, so the
        // turn's own "saved" broadcast and SendClosed reload normally.
        app.handle_msg(AppMsg::SendStarted {
            sid: "sess1".into(),
        });
        assert!(!app.interrupting);
        assert!(app.streaming);
        assert!(!app.paused);

        app.handle_msg(AppMsg::SubEvent(serde_json::json!({"type": "saved"})));
        assert!(app.pending_saved);
        let fx = app.handle_msg(AppMsg::SendClosed);
        assert!(fx.iter().any(|e| matches!(e, Effect::LoadSession { .. })));
    }

    #[test]
    fn entering_mid_stream_without_session_still_pauses() {
        // Even without a session id, the message should be carried in the
        // effect and the stream paused.
        let mut app = App::new_test(90, 30);
        app.session.id.clear();
        app.streaming = true;

        app.input.set_value("say something");
        let text = app.input.value.trim().to_string();
        let fx = app.handle_input(&text);

        assert!(app.paused);
        assert!(app.input.value.is_empty());
        let interrupt = fx
            .iter()
            .find_map(|e| match e {
                Effect::InterruptTurn { req, .. } => Some(req.message.as_str()),
                _ => None,
            })
            .expect("InterruptTurn effect");
        assert_eq!(interrupt, "say something");
    }

    #[test]
    fn submit_attaches_pending_images_to_user_block() {
        let mut app = App::new_test(90, 30);
        app.session.id = "sess1".into();
        app.sel_provider.base_url = "http://test:11434/v1".into();

        // Synthesize a pending image (skip normalization).
        app.pending.push(crate::preview::PendingImage {
            img: atom_core::types::ImageData {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
            name: "shot.png".into(),
            cols: crate::preview::PREVIEW_COLS,
            rows: crate::preview::PREVIEW_ROWS,
            num: 1,
        });

        app.input.set_value("look at this [IMG 1]");
        let text = app.input.value.trim().to_string();
        let fx = app.handle_input(&text);

        // Pending cleared, block carries the image, and a PaintPreviews
        // effect is scheduled so the kitty terminal can render it.
        assert!(app.pending.is_empty());
        let last = app.blocks.last().expect("user block pushed");
        assert_eq!(last.kind, BlockKind::User);
        assert_eq!(last.text, "look at this [IMG 1]");
        assert_eq!(last.images.len(), 1);
        assert_eq!(last.images[0].num, 1);
        assert!(fx.iter().any(|e| matches!(e, Effect::PaintPreviews)));
        assert!(fx.iter().any(|e| matches!(e, Effect::SendTurn(_))));
    }

    #[test]
    fn scrollbar_track_click_navigates() {
        let mut app = App::new_test(80, 24);
        // Fill content with many lines so scrollbar is visible.
        app.content_lines = (0..200)
            .map(|i| std::sync::Arc::new(Line::from(format!("line {i}"))))
            .collect();
        app.scroll_y = 0;
        let vp = app.viewport_height();
        // Click near the bottom of the scrollbar track (left column of the
        // 2-wide scrollbar gutter: width - SCROLLBAR_WIDTH = 78).
        let click_row = VIEWPORT_VPAD + vp - 2;
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 78, // left column of scrollbar gutter (width - 2)
            row: click_row as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        // scroll_y should have jumped significantly from 0.
        assert!(
            app.scroll_y > 0,
            "scrollbar track click should scroll: scroll_y = {}",
            app.scroll_y
        );
        // It should be proportional to where we clicked (near the bottom).
        let max_scroll = app
            .content_lines
            .len()
            .saturating_sub(app.content_viewport_height());
        assert!(
            app.scroll_y > max_scroll / 2,
            "clicking near bottom should scroll past halfway: scroll_y={}, max={}",
            app.scroll_y,
            max_scroll
        );
    }

    #[test]
    fn scrollbar_track_click_works_at_rightmost_column() {
        // Clicking the right column of the 2-wide scrollbar also works.
        let mut app = App::new_test(80, 24);
        app.content_lines = (0..200)
            .map(|i| std::sync::Arc::new(Line::from(format!("line {i}"))))
            .collect();
        app.scroll_y = 0;
        let vp = app.viewport_height();
        let click_row = VIEWPORT_VPAD + vp - 2;
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 79, // rightmost column (width - 1)
            row: click_row as u16,
            modifiers: KeyModifiers::empty(),
        };
        let _ = app.mouse(ev);
        assert!(
            app.scroll_y > 0,
            "scrollbar click at width-1 should scroll: scroll_y = {}",
            app.scroll_y
        );
    }

    #[test]
    fn theme_switch_drops_cached_block_colors() {
        let mut app = App::new_test(80, 20);
        app.blocks.push(Block {
            kind: BlockKind::Tool,
            title: "Fetch".into(),
            tool_name: "webfetch".into(),
            text: "example".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        assert!(app.blocks[0].lines.as_ref().is_some(), "block cache built");

        // Re-select the active theme: the palette is unchanged, so this
        // stays race-free for other tests that pin default-theme colors,
        // while still exercising the switch path end to end. The cached
        // lines must be dropped so the next frame re-renders against
        // whatever palette is active.
        let active = atom_core::render::colors::active_theme_name();
        let target = overlays::theme_rows()
            .iter()
            .position(|entry| entry.id == active)
            .expect("active theme is selectable");
        app.overlay = Some(OverlayKind::Theme);
        app.overlay_sel = target;

        let effects = app.key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert!(app.preview_dirty);
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::PaintPreviews));
        assert!(
            app.blocks[0].lines.as_ref().is_none(),
            "theme switch must drop cached block colors"
        );
    }
}
