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

/// splashTickInterval drives the empty-session atom animation.
pub const SPLASH_TICK_MS: u64 = 33;

/// MiniDot runs at 12 fps (bubbles).
pub const SPINNER_TICK_MS: u64 = 83;

/// outputTestSceneDuration backs the --output-test scene timer.
pub const TEST_SCENE_TICK_SECS: u64 = 3;

pub struct RunOptions {
    pub providers: Vec<Provider>,
    pub sel_provider: Provider,
    pub sel_model: String,
    pub session: SessionInfo,
    pub hot_state_path: Option<std::path::PathBuf>,
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
    pub block_start: Vec<usize>,
    pub content_width: usize,
    /// pinned to the newest output until the user scrolls up
    pub following: bool,
    /// viewport YOffset analog
    pub scroll_y: usize,

    // mouse text selection over the viewport ((line,col) pairs)
    pub sel_anchor: Option<(usize, usize)>,
    pub sel_end: Option<(usize, usize)>,
    pub selecting: bool,
    pub sel_active: bool,
    pub prompt_selecting: bool,
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
    pub overlay_sel: usize,
    pub overlay_scroll: usize,
    pub overlay_entries: Vec<atom_core::providers::providers::ModelEntry>,
    pub overlay_sessions: Vec<SessionInfo>,
    pub overlay_stats: Option<StatsReport>,
    pub stats_days: i64,
    pub overlay_providers: Vec<ProviderListEntry>,
    pub overlay_auth_id: String,
    pub overlay_auth_type: String,
    pub picker_settings: crate::settings::PickerSettings,
    pub atom_config: atom_core::config::AtomConfig,
    pub model_picker_purpose: overlays::ModelPickerPurpose,
    pub settings_onboarding: bool,
    pub pending_model_provider: String,

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
            block_start: Vec::new(),
            content_width: 0,
            following: true,
            scroll_y: 0,
            sel_anchor: None,
            sel_end: None,
            selecting: false,
            sel_active: false,
            prompt_selecting: false,
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
            overlay_sel: 0,
            overlay_scroll: 0,
            overlay_entries: Vec::new(),
            overlay_sessions: Vec::new(),
            overlay_stats: None,
            stats_days: 0,
            overlay_providers: Vec::new(),
            overlay_auth_id: String::new(),
            overlay_auth_type: String::new(),
            picker_settings: crate::settings::load(),
            atom_config: atom_core::config::load(),
            model_picker_purpose: overlays::ModelPickerPurpose::Chat,
            settings_onboarding: false,
            pending_model_provider: String::new(),
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
            pending: Vec::new(),
            preview_dirty: false,
            approval: None,
            quitting: false,
            test_mode: false,
            test_scene: -1,
            input: Prompt::new(),
            cwd,
        };
        m.refresh_thinking_levels();
        m.apply_thinking(&m.session.thinking.clone());
        // If no model was found, auto-open the provider selector.
        if m.sel_model.is_empty() && m.session.id.is_empty() {
            m.overlay = Some(OverlayKind::Providers);
            m.working_msg = "loading providers...".into();
        } else if !m.atom_config.setup_complete() {
            m.overlay = Some(OverlayKind::Settings);
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
        (self.width as usize).saturating_sub(2 * TUI_HPAD).max(1)
    }

    pub fn input_width(&self) -> usize {
        self.inner_width().saturating_sub(2 * PROMPT_PAD).max(1)
    }

    /// Rows reserved inside the prompt box for image previews.
    pub fn preview_row_count(&self) -> usize {
        crate::preview::preview_row_count(self)
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
            .min(INPUT_MAX_HEIGHT)
            .max(1);
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
                    }
                }
                BlockKind::Compaction => {
                    if b.active {
                        b.lines = None;
                    }
                }
                BlockKind::Tool => {
                    if !b.tool_done {
                        b.lines = None;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn invalidate_all_blocks(&mut self) {
        for block in &mut self.blocks {
            block.lines = None;
        }
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
        } else {
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

    pub fn rebuild_content_from(&mut self, _idx: usize, width: usize) {
        let frame = MINIDOT_FRAMES[self.spinner_frame % MINIDOT_FRAMES.len()].to_string();
        for i in 0..self.blocks.len() {
            let show_r = self.show_reasoning;
            let b = &mut self.blocks[i];
            if !b.lines_valid(width, show_r) {
                let rendered = blocks::render_block(b, width, show_r, &frame)
                    .into_iter()
                    .map(Arc::new)
                    .collect::<Vec<_>>();
                b.lines = Some(rendered.clone());
                b.line_width = width;
                b.line_show_r = show_r;
                b.line_expanded = b.expanded;
            }
        }

        self.content_lines.clear();
        self.block_start.clear();
        let mut has_visible_block = false;
        for block in &self.blocks {
            let lines = block.lines.as_deref().unwrap_or_default();
            let visible = lines
                .iter()
                .any(|line| line.spans.iter().any(|span| !span.content.is_empty()));
            if !visible {
                self.block_start.push(self.content_lines.len());
                continue;
            }
            if has_visible_block {
                self.content_lines.push(Arc::new(Line::from("")));
            }
            self.block_start.push(self.content_lines.len());
            self.content_lines.extend(lines.iter().cloned());
            has_visible_block = true;
        }
    }

    /// Conversation rows available before a footer menu is accounted for.
    pub fn base_viewport_height(&self) -> usize {
        let vp = (self.height as usize).saturating_sub(
            crate::statusbar::status_bar_rows(self)
                + STATUS_FOOTER_ROWS
                + 2 * PROMPT_PAD
                + self.input_height()
                + self.preview_row_count(),
        );
        // Reserve only the top viewport padding: the prompt card sits
        // directly below the scrolling region (and below an open footer
        // menu) with no empty row in between.
        vp.saturating_sub(VIEWPORT_VPAD).max(1)
    }

    pub fn viewport_height(&self) -> usize {
        self.base_viewport_height()
            .saturating_sub(crate::overlays::footer_menu_height(self))
            .max(1)
    }

    /// Number of content rows actually drawn in the message viewport.
    /// The scrollbar and viewport rect keep their full height
    /// (`viewport_height()`); this reserves a small bottom padding row
    /// of blank space between the last content row and the prompt when
    /// no footer menu is open. A footer menu already separates content
    /// from the prompt, so the padding is only applied without one.
    pub fn content_viewport_height(&self) -> usize {
        let vp = self.viewport_height();
        let h = if crate::overlays::footer_menu_height(self) > 0 {
            vp
        } else {
            vp.saturating_sub(VIEWPORT_BOTTOM_PAD)
        };
        h.max(1)
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
                // Pause input behind the sandbox approval box. When the
                // request comes from a dispatched subagent, `session_id`
                // names the child session the decision must be posted to.
                self.approval = Some(ApprovalPrompt {
                    id: ev.id.clone(),
                    command: ev.command.clone(),
                    cwd: ev.cwd.clone(),
                    rule_id: ev.rule_id.clone(),
                    reason: ev.reason.clone(),
                    session_id: if ev.session_id.is_empty() {
                        self.session.id.clone()
                    } else {
                        ev.session_id.clone()
                    },
                    child_title: ev.child_title.clone(),
                    from_subagent: ev.from_subagent,
                });
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
        }
    }

    fn finalize_tools(&mut self) {
        for b in &mut self.blocks {
            if b.kind == BlockKind::Tool && !b.tool_done {
                b.tool_done = true;
                b.lines = None;
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

        let passthrough =
            !text.starts_with('/') || overlays::is_catalog_prompt(text, &self.slash_commands);
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
            self.overlay = Some(OverlayKind::Stats);
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
                self.overlay = Some(OverlayKind::Settings);
                self.overlay_sel = 0;
                self.overlay_q.clear();
                self.settings_onboarding = false;
                Vec::new()
            }
            "/model" => {
                self.model_picker_purpose = overlays::ModelPickerPurpose::Chat;
                self.overlay = Some(OverlayKind::Model);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.pending_model_provider.clear();
                self.working_msg = "loading models...".into();
                vec![Effect::ReloadProviders]
            }
            "/reasoning" => {
                self.reasoning_visible = true;
                self.reasoning_sel = self
                    .thinking_idx
                    .min(self.thinking_levels.len().saturating_sub(1));
                Vec::new()
            }
            "/providers" => {
                self.overlay = Some(OverlayKind::Providers);
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
                self.overlay = Some(OverlayKind::Session);
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                self.working_msg = "loading sessions...".into();
                vec![Effect::FetchSessions]
            }
            "/thinking" => {
                self.show_reasoning = !self.show_reasoning;
                for block in &mut self.blocks {
                    if block.kind == BlockKind::Reasoning {
                        block.expanded = self.show_reasoning;
                        block.lines = None;
                    }
                }
                self.refresh_viewport();
                Vec::new()
            }
            other => {
                self.err_msg = format!("unknown command: {other}");
                Vec::new()
            }
        }
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
        let name = self.picker_items[self.picker_sel].title.trim().to_string();
        if !name.is_empty() {
            self.apply_picker_insert(&format!("/{name}"));
        }
        if close {
            self.close_picker();
        }
        Vec::new()
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
                self.overlay_entries = entries;
                self.overlay_sel = overlays::first_model_row(self);
                self.overlay_scroll = 0;
                overlays::sync_model_scroll(self);
                self.pending_model_provider.clear();
                self.working_msg.clear();
                Vec::new()
            }
            AppMsg::SessionsLoaded(sessions) => {
                self.overlay_sessions = sessions;
                self.overlay_q.clear();
                self.overlay_sel = 0;
                self.overlay_scroll = 0;
                if self.overlay == Some(OverlayKind::Session) {
                    self.overlay_sel = overlays::first_session_row(self);
                    overlays::sync_session_scroll(self);
                }
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
            AppMsg::ClipboardText(text) => {
                if self.overlay.is_some() {
                    if overlays::overlay_has_query(Some(self.overlay.unwrap())) && !text.is_empty()
                    {
                        self.replace_or_append_overlay_query(&text);
                    }
                    return Vec::new();
                }
                if !text.is_empty() {
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
                if let Err(e) = preview::paste_image(self, &name, &data) {
                    self.err_msg = e.to_string();
                }
                vec![Effect::PaintPreviews]
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
                    return vec![Effect::SubscribeAfter {
                        id: sid,
                        delay_ms: 1000,
                    }];
                }
                Vec::new()
            }
            AppMsg::TickSpinner => {
                self.spinner_frame = (self.spinner_frame + 1) % MINIDOT_FRAMES.len();
                if self.streaming || self.remote_working || self.test_mode {
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
            // Handled by the loop before reaching the state machine.
            AppMsg::SubscribeNow(_)
            | AppMsg::HotRebuilt(_)
            | AppMsg::ThemeReloaded(_)
            | AppMsg::Redraw
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
            self.overlay = Some(OverlayKind::Settings);
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
        self.refresh_viewport();

        let mut fx = Vec::new();
        if !same_session {
            // A stale subscription keeps draining; the runtime swaps to
            // the new session's channel on SubStarted.
        }
        fx.push(Effect::Subscribe {
            id: self.session.id.clone(),
        });
        fx.push(Effect::PaintPreviews);
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
                    self.overlay = Some(OverlayKind::Providers);
                    self.overlay_q.clear();
                    self.overlay_sel = 0;
                    self.overlay_providers = providers::list_addable_providers();
                    return Vec::new();
                }
                self.open_models_for_provider("openai");
                return vec![Effect::ReloadProviders];
            }
        }
        self.overlay = Some(OverlayKind::Providers);
        self.overlay_q.clear();
        self.overlay_sel = 0;
        self.overlay_auth_type.clear();
        self.overlay_providers = providers::list_addable_providers();
        vec![Effect::ReloadProviders]
    }

    fn open_models_for_provider(&mut self, id: &str) {
        self.overlay = Some(OverlayKind::Model);
        self.overlay_q.clear();
        self.overlay_sel = 0;
        self.overlay_scroll = 0;
        self.overlay_auth_type.clear();
        self.pending_model_provider = id.to_string();
        self.working_msg = "loading models...".into();
    }

    /// Shared resize path for AppMsg::Resize and view::draw's
    /// area-size sync: update dims, re-wrap content when width moved.
    pub(crate) fn apply_resize(&mut self, w: u16, h: u16) {
        let prev_w = self.width;
        self.width = w;
        self.height = h;
        if w != prev_w {
            self.refresh_viewport();
        }
    }

    fn resize(&mut self, w: u16, h: u16) -> Vec<Effect> {
        self.apply_resize(w, h);
        Vec::new()
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
        self.input.insert_str(&content);
        self.after_input_change()
    }

    /// afterInputChange syncs menus/previews after prompt edits.
    pub fn after_input_change(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
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
        if typed.starts_with('/') || self.menu_virtual {
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

        let mods = k.modifiers;
        let shift = mods.contains(KeyModifiers::SHIFT);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);

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
            }
            KeyCode::Enter => {
                let text = self.input.value.trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                self.set_menu_visible(false);
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

        match k.code {
            KeyCode::Esc => {
                match kind {
                    OverlayKind::ProviderMethod | OverlayKind::ProviderKey => {
                        self.overlay = Some(OverlayKind::Providers);
                        self.overlay_q.clear();
                        self.overlay_sel = 0;
                        self.overlay_providers = providers::list_addable_providers();
                    }
                    OverlayKind::WebSearch => {
                        self.overlay = Some(OverlayKind::Settings);
                        self.overlay_sel = 1;
                    }
                    OverlayKind::Model
                        if self.model_picker_purpose
                            == overlays::ModelPickerPurpose::Compaction =>
                    {
                        self.overlay = Some(OverlayKind::Settings);
                        self.overlay_sel = 0;
                        self.overlay_q.clear();
                        self.working_msg.clear();
                    }
                    OverlayKind::Settings if self.settings_onboarding => {
                        self.accept_settings_defaults();
                        self.overlay = None;
                    }
                    _ => {
                        self.overlay = None;
                        self.overlay_q.clear();
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
                    }
                    OverlayKind::Session => overlays::move_session_sel(self, -1),
                    OverlayKind::Model => overlays::move_model_sel(self, -1),
                    OverlayKind::ProviderKey => {}
                    _ => {
                        if self.overlay_sel > 0 {
                            self.overlay_sel -= 1;
                        }
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
                    }
                    OverlayKind::Session => overlays::move_session_sel(self, 1),
                    OverlayKind::Model => overlays::move_model_sel(self, 1),
                    OverlayKind::ProviderKey => {}
                    _ => {
                        let cnt = overlays::overlay_count(self);
                        if cnt > 0 && self.overlay_sel < cnt - 1 {
                            self.overlay_sel += 1;
                        }
                    }
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                if matches!(kind, OverlayKind::Stats | OverlayKind::ProviderMethod) {
                    return Vec::new();
                }
                if self.overlay_q_sel {
                    self.overlay_q.clear();
                    self.overlay_q_sel = false;
                    self.reset_overlay_sel_after_query();
                    return Vec::new();
                }
                if !self.overlay_q.is_empty() {
                    self.overlay_q.pop();
                    self.reset_overlay_sel_after_query();
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
        } else if self.overlay != Some(OverlayKind::ProviderKey) {
            self.overlay_sel = 0;
        }
    }

    pub fn replace_or_append_overlay_query(&mut self, text: &str) {
        if self.overlay_q_sel {
            self.overlay_q = text.to_string();
            self.overlay_q_sel = false;
            return;
        }
        self.overlay_q.push_str(text);
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
        return vec![Effect::ReloadProviders];
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
                    });
                    self.save_atom_config();
                    self.model_picker_purpose = overlays::ModelPickerPurpose::Chat;
                    self.overlay = Some(OverlayKind::Settings);
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
                self.working_msg.clear();
                vec![Effect::LoadSession { id: picked }]
            }
            OverlayKind::Stats => {
                self.overlay = None;
                self.overlay_q.clear();
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
                self.overlay = Some(OverlayKind::ProviderMethod);
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
                self.overlay = Some(OverlayKind::ProviderKey);
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
                    self.working_msg.clear();
                    return Vec::new();
                }
                self.open_models_for_provider(&id);
                vec![Effect::ReloadProviders]
            }
            OverlayKind::Settings => match self.overlay_sel {
                0 => {
                    self.model_picker_purpose = overlays::ModelPickerPurpose::Compaction;
                    self.overlay = Some(OverlayKind::Model);
                    self.overlay_q.clear();
                    self.overlay_sel = 0;
                    self.overlay_scroll = 0;
                    self.working_msg = "loading models...".into();
                    vec![Effect::ReloadProviders]
                }
                1 => {
                    self.overlay = Some(OverlayKind::WebSearch);
                    let selected = self.atom_config.resolved_web_search().server;
                    let rows = overlays::web_search_rows(self);
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
                self.overlay = Some(OverlayKind::Settings);
                self.overlay_sel = 1;
                Vec::new()
            }
        }
    }

    // -- approval ----------------------------------------------------------

    fn approval_key(&mut self, k: KeyEvent, req: ApprovalPrompt) -> Vec<Effect> {
        let decision = match k.code {
            KeyCode::Char('a') => Some(("allow_once", "allow once")),
            KeyCode::Char('s') => Some(("allow_session", "allowed this session")),
            KeyCode::Char('g') => Some(("allow_global", "always allowed")),
            KeyCode::Char('d') | KeyCode::Esc => Some(("deny", "denied")),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quitting = true;
                return vec![Effect::Quit];
            }
            _ => None,
        };
        match decision {
            Some((wire, note)) => {
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

    // -- mouse ---------------------------------------------------------------

    pub fn mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        let (x, y) = (m.column as usize, m.row as usize);
        if self.overlay.is_some() {
            return match m.kind {
                MouseEventKind::Down(MouseButton::Left) => overlays::click_overlay(self, y),
                MouseEventKind::Drag(MouseButton::Left) => {
                    overlays::hover_overlay_row(self, y);
                    Vec::new()
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
        if self.mouse_in_prompt(y) {
            if self.sel_active {
                self.clear_selection();
            }
            self.prompt_selecting = true;
            return Vec::new();
        }
        self.input.clear_selection();
        if self.sel_active {
            self.clear_selection();
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
            if self.blocks[bi].kind == BlockKind::User && self.blocks[bi].user_collapsible(inner) {
                self.blocks[bi].expanded = !self.blocks[bi].expanded;
                self.blocks[bi].lines = None;
                self.refresh_viewport();
                return Vec::new();
            }
            if self.blocks[bi].kind == BlockKind::Tool {
                if self.blocks[bi].tool_collapsible(inner, inner) {
                    self.blocks[bi].expanded = !self.blocks[bi].expanded;
                    self.blocks[bi].lines = None;
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
                }
            }
        }
        Vec::new()
    }

    fn release(&mut self, _x: usize, _y: usize) -> Vec<Effect> {
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
        let geo = crate::view::Layout::compute(self);
        y >= geo.prompt_top_y
            && y < geo.prompt_top_y
                + 2 * PROMPT_PAD
                + self.input_height()
                + self.preview_row_count()
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

fn picker_items(commands: &[DynamicCommand], kind: &str) -> Vec<PickerItem> {
    commands
        .iter()
        .filter(|command| command.kind == kind)
        .map(|command| PickerItem {
            title: command.name.trim_start_matches('/').to_string(),
            meta: command.desc.clone(),
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
            (KeyCode::Char('a'), "allow_once"),
            (KeyCode::Char('s'), "allow_session"),
            (KeyCode::Char('g'), "allow_global"),
            (KeyCode::Char('d'), "deny"),
            (KeyCode::Esc, "deny"),
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
        assert_eq!(matches[2].name, "/settings");
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
            .any(|effect| matches!(effect, Effect::ReloadProviders)));

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
            })
        );
    }

    #[test]
    fn settings_web_search_picker_saves_bundled_tool() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(OverlayKind::Settings);
        app.overlay_sel = 1;
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
        let long: String = std::iter::repeat("row line\n").take(20).collect();
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
        assert!(app.blocks[0].user_collapsible(app.inner_width().saturating_sub(2).max(1)));
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
}
