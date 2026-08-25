//! blocks.rs is the conversation model: one `Block` per rendered unit
//! (user message, assistant reply, reasoning, compaction phase, tool
//! call, error), the stream-event helpers that build them
//! (messagesToBlocks/attachToolResult/toolAction ports), and
//! render_block which wraps a block into styled ratatui Lines at a
//! fixed width with a per-block cache.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};

use crate::ansi;
use crate::preview::{self, PendingImage};
use atom_core::types::Message;
use atom_core::util::first_line_trunc;

/// toolResultPreviewLines: wrapped result/diff lines shown while a tool
/// block is collapsed.
pub const TOOL_RESULT_PREVIEW_LINES: usize = 8;

/// userPreviewLines: total body rows shown for a collapsed user card,
/// including the trailing "… click to expand" hint row.
pub const USER_PREVIEW_LINES: usize = 8;

/// The last row of a collapsed user card.
pub const USER_EXPAND_HINT: &str = "… click to expand";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    User,
    Assistant,
    Reasoning,
    Compaction,
    Tool,
    Error,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::User => "user",
            BlockKind::Assistant => "assistant",
            BlockKind::Reasoning => "reasoning",
            BlockKind::Compaction => "compaction",
            BlockKind::Tool => "tool",
            BlockKind::Error => "error",
        }
    }
}

/// One rendered unit in the conversation.
#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
    /// muted tool name in the call box; empty for result-only
    pub title: String,
    /// raw tool id (bash, edit_file, …)
    pub tool_name: String,
    /// unified diff of the file a tool edited, "" when none
    pub diff: String,
    /// tool output under a muted separator; successful tools may return ""
    pub result: String,
    /// true once a tool_result arrives, even when its output is empty
    pub tool_done: bool,
    /// dispatch child session; empty for other tools
    pub session_id: String,
    /// expanded shows the full tool result/diff.
    pub expanded: bool,
    /// active marks a reasoning/compaction block still streaming;
    /// started_at/dur back the collapsed "Thinking (8.3s)" line.
    pub active: bool,
    pub started_at: Option<Instant>,
    pub dur: Option<Duration>,
    /// Completed-turn metadata shown under the final assistant reply.
    pub model: String,
    pub turn_duration: Option<Duration>,
    /// Pasted images attached to a user message. Their [IMG n] markers
    /// in `text` render as kitty placeholder grids (or chip text on
    /// terminals without kitty support).
    pub images: Vec<PendingImage>,

    // Render cache: wrapped lines at line_width. None is a miss.
    pub lines: Option<Vec<Arc<Line<'static>>>>,
    pub line_width: usize,
    pub line_show_r: bool,
    pub line_expanded: bool,
}

impl Default for Block {
    fn default() -> Self {
        Block {
            kind: BlockKind::User,
            text: String::new(),
            title: String::new(),
            tool_name: String::new(),
            diff: String::new(),
            result: String::new(),
            tool_done: false,
            session_id: String::new(),
            expanded: false,
            active: false,
            started_at: None,
            dur: None,
            model: String::new(),
            turn_duration: None,
            images: Vec::new(),
            lines: None,
            line_width: 0,
            line_show_r: true,
            line_expanded: false,
        }
    }
}

impl Block {
    pub fn new(kind: BlockKind) -> Self {
        Block {
            kind,
            ..Default::default()
        }
    }

    pub fn lines_valid(&self, width: usize, _show_reasoning: bool) -> bool {
        let Some(_) = self.lines else {
            return false;
        };
        if self.line_width != width {
            return false;
        }
        if matches!(
            self.kind,
            BlockKind::Reasoning | BlockKind::Tool | BlockKind::User
        ) && self.line_expanded != self.expanded
        {
            return false;
        }
        true
    }

    pub fn resolved_tool_name(&self) -> String {
        if !self.tool_name.is_empty() {
            return self.tool_name.clone();
        }
        self.title.to_lowercase().replace(' ', "_")
    }

    fn tool_output_exceeds_preview(&self, result_inner: usize, diff_width: usize) -> bool {
        wrapped_line_count(&tool_result_summary(&self.result, &self.diff), result_inner)
            > TOOL_RESULT_PREVIEW_LINES
            || wrapped_line_count(&self.diff, diff_width) > TOOL_RESULT_PREVIEW_LINES
    }

    pub fn tool_collapsible(&self, result_inner: usize, diff_width: usize) -> bool {
        if hidden_output_tool(&self.resolved_tool_name()) {
            return !self.result.is_empty() || !self.diff.is_empty();
        }
        self.tool_output_exceeds_preview(result_inner, diff_width)
    }

    /// userCollapsible: a user card collapses when its wrapped, rendered
    /// body (text rows plus image placeholder rows) exceeds the preview
    /// budget. One extremely long pasted line wraps into many rows and
    /// therefore collapses like a multi-line message would.
    pub fn user_collapsible(&self, inner: usize) -> bool {
        render_user_body(&self.text, &self.images, inner, None).len() > USER_PREVIEW_LINES
    }
}

// ---------------------------------------------------------------------------
// History → blocks (session.go transcript shape).
// ---------------------------------------------------------------------------

/// messagesToBlocks converts persisted history into TUI blocks.
pub fn messages_to_blocks(msgs: &[Message]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for msg in msgs {
        match msg.role.as_str() {
            "user" => blocks.push(Block {
                kind: BlockKind::User,
                text: msg.content.clone(),
                images: msg
                    .images
                    .iter()
                    .map(|img| PendingImage {
                        img: img.clone(),
                        name: String::new(),
                        // num is reassigned later by assign_block_image_nums.
                        num: 0,
                        cols: preview::PREVIEW_COLS,
                        rows: preview::PREVIEW_ROWS,
                    })
                    .collect(),
                ..Default::default()
            }),
            // Injected continue prompt after truncated reasoning; not shown.
            "nudge" => {}
            "compaction" => blocks.push(Block {
                kind: BlockKind::Compaction,
                text: msg.content.clone(),
                model: msg.model.clone(),
                dur: (msg.duration_ms > 0).then(|| Duration::from_millis(msg.duration_ms as u64)),
                ..Default::default()
            }),
            "error" => blocks.push(Block {
                kind: BlockKind::Error,
                text: msg.content.clone(),
                ..Default::default()
            }),
            "assistant" => {
                if !msg.reasoning.is_empty() || msg.reasoning_ms > 0 {
                    let mut b = Block::new(BlockKind::Reasoning);
                    b.text = msg.reasoning.clone();
                    if msg.reasoning_ms > 0 {
                        b.dur = Some(Duration::from_millis(msg.reasoning_ms as u64));
                    }
                    blocks.push(b);
                }
                if !msg.content.is_empty() {
                    blocks.push(Block {
                        kind: BlockKind::Assistant,
                        text: msg.content.clone(),
                        model: msg.model.clone(),
                        turn_duration: (msg.duration_ms > 0)
                            .then(|| Duration::from_millis(msg.duration_ms as u64)),
                        ..Default::default()
                    });
                }
                for tc in &msg.tool_calls {
                    blocks.push(Block {
                        kind: BlockKind::Tool,
                        title: tool_display_name(&tc.function.name),
                        tool_name: tc.function.name.clone(),
                        text: tool_action(&tc.function.name, &tc.function.arguments),
                        ..Default::default()
                    });
                }
            }
            "tool" => attach_tool_result(&mut blocks, &msg.content, &msg.diff),
            _ => {}
        }
    }
    blocks
}

/// sessionToBlocks renders the transcript plus the compaction brief when
/// the session was folded but never stored a display copy (older saves).
pub fn session_to_blocks(sess: &atom_core::session::store::Session) -> Vec<Block> {
    let mut blocks = messages_to_blocks(&sess.messages);
    if sess.compaction_summary.is_empty() {
        return blocks;
    }
    if blocks
        .iter()
        .any(|b| b.kind == BlockKind::Compaction && !b.text.is_empty())
    {
        return blocks;
    }
    blocks.push(Block {
        kind: BlockKind::Compaction,
        text: atom_core::session::compaction::compaction_prompt_text(&sess.compaction_summary),
        ..Default::default()
    });
    blocks
}

/// assignBlockImageNums assigns a unique kitty image id to every image
/// across the block list, skipping any ids already in use by the caller
/// (e.g. the prompt's pending set). Renumbers from 1 upward so the
/// kitty image protocol can safely reuse previously transmitted slots.
pub fn assign_block_image_nums(blocks: &mut [Block], reserved: &[usize]) {
    let mut used: std::collections::HashSet<usize> = reserved.iter().copied().collect();
    let mut next = 1usize;
    for block in blocks.iter_mut() {
        if block.kind != BlockKind::User {
            continue;
        }
        for img in block.images.iter_mut() {
            if img.num != 0 && used.contains(&img.num) {
                // Collision with an in-use id; reassign to a fresh slot.
                img.num = 0;
            }
            if img.num == 0 {
                // Wrap at MAX_KITTY_PREVIEW_ID so paint_kitty_previews
                // can still clean up orphaned slots after scrolls.
                let mut guard = 0;
                while used.contains(&next) && guard <= preview::MAX_KITTY_PREVIEW_ID {
                    next = next % preview::MAX_KITTY_PREVIEW_ID + 1;
                    guard += 1;
                }
                img.num = next;
                used.insert(next);
                next = next % preview::MAX_KITTY_PREVIEW_ID + 1;
            } else {
                used.insert(img.num);
            }
        }
    }
}

/// Restores transient reasoning state onto matching blocks after a saved reload.
pub fn restore_reasoning_durations(blocks: &mut [Block], prev: &[Block]) {
    let mut state = std::collections::HashMap::new();
    for b in prev {
        if b.kind == BlockKind::Reasoning {
            state.insert(b.text.clone(), (b.dur, b.expanded));
        }
    }
    for b in blocks.iter_mut() {
        if b.kind == BlockKind::Reasoning {
            if let Some((dur, expanded)) = state.get(&b.text) {
                if b.dur.map(|d| d == Duration::ZERO).unwrap_or(true) {
                    b.dur = *dur;
                }
                b.expanded = *expanded;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool naming / summaries.
// ---------------------------------------------------------------------------

/// toolDisplayName turns a snake_case tool id into a title ("bash" → "Bash").
/// Web search tools show the configured provider's display name
/// ("web_search" → "Parallel Web Search"/"Ollama Web Search",
/// "web_search_exa" → "Exa Web Search") so the call reads like the
/// service that answered it, not the generic function id.
pub fn tool_display_name(name: &str) -> String {
    tool_display_name_for(name, &atom_core::config::load().resolved_web_search())
}

fn tool_display_name_for(name: &str, selected: &atom_core::config::WebSearchConfig) -> String {
    if let Some(profile) = atom_core::config::bundled_web_search_profile(&selected.server) {
        if name == profile.tool {
            return profile.name.clone();
        }
    }
    name.split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut cs = p.chars();
            match cs.next() {
                Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// toolAction is the one-line summary of what a tool call did: the shell
/// command, search query, or file path. Falls back to the raw arguments.
pub fn tool_action(name: &str, arguments: &str) -> String {
    let args: Result<serde_json::Value, _> = serde_json::from_str(arguments);
    let ok = |v: &Result<serde_json::Value, _>| v.as_ref().ok().cloned().unwrap_or_default();
    match name {
        "bash" => {
            let v = ok(&args);
            if let Some(c) = v.get("command").and_then(|c| c.as_str()) {
                if !c.is_empty() {
                    return c.to_string();
                }
            }
        }
        "web_search" | "vector_search" => {
            let v = ok(&args);
            if let Some(q) = v.get("query").and_then(|q| q.as_str()) {
                if !q.is_empty() {
                    return q.to_string();
                }
            }
        }
        "grep" | "glob" => {
            let v = ok(&args);
            if let Some(p) = v.get("pattern").and_then(|p| p.as_str()) {
                if !p.is_empty() {
                    return p.to_string();
                }
            }
        }
        "read_file" => {
            let v = ok(&args);
            let mut out = String::new();
            if let Some(p) = v.get("path").and_then(|p| p.as_str()) {
                if !p.is_empty() {
                    out.push_str(p);
                }
            }
            let mut bracket: Vec<String> = Vec::new();
            if let Some(offset) = v.get("offset").and_then(|o| o.as_i64()) {
                if offset > 0 {
                    bracket.push(format!("offset={offset}"));
                }
            }
            if let Some(limit) = v.get("limit").and_then(|l| l.as_i64()) {
                if limit > 0 {
                    bracket.push(format!("limit={limit}"));
                }
            }
            if !bracket.is_empty() {
                out.push_str(&format!(" [{}]", bracket.join(", ")));
            }
            if !out.is_empty() {
                return out;
            }
        }
        "skill" => {
            let v = ok(&args);
            if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                if !n.is_empty() {
                    return n.to_string();
                }
            }
        }
        "dispatch" => {
            let v = ok(&args);
            let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
            if action == "spawn" {
                if let Some(tasks) = v.get("tasks").and_then(|tasks| tasks.as_array()) {
                    let first = tasks.first().and_then(|task| task.as_str()).unwrap_or("");
                    let snippet = first_line_trunc(first, 40);
                    return if tasks.len() == 1 {
                        snippet
                    } else {
                        format!("{} subagents: {snippet}", tasks.len())
                    };
                }
            }
            if !action.is_empty() {
                let target = v
                    .get("batch_id")
                    .and_then(|id| id.as_str())
                    .or_else(|| {
                        v.get("ids")
                            .and_then(|ids| ids.as_array())
                            .and_then(|ids| ids.first())
                            .and_then(|id| id.as_str())
                    })
                    .unwrap_or("");
                return if target.is_empty() {
                    action.to_string()
                } else {
                    format!("{action} {target}")
                };
            }
            let cancel = v.get("cancel").and_then(|c| c.as_bool()).unwrap_or(false);
            let sid = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let prompt = v
                .get("prompt")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let model = v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if cancel && !sid.is_empty() {
                return format!("cancel {sid}");
            }
            let snippet = first_line_trunc(&prompt, 40);
            if !snippet.is_empty() {
                return snippet;
            }
            if !sid.is_empty() {
                return sid;
            }
            if !model.is_empty() {
                return model;
            }
        }
        _ => {
            if let Some(v) = args.ok() {
                if name.starts_with("mcp_") {
                    for key in ["query", "name", "url", "path"] {
                        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                            if !s.is_empty() {
                                return s.to_string();
                            }
                        }
                    }
                    if arguments.len() > 80 {
                        return truncate_bytes(arguments, 80) + "...";
                    }
                    if !arguments.is_empty() {
                        return arguments.to_string();
                    }
                } else if let Some(p) = v.get("path").and_then(|p| p.as_str()) {
                    if !p.is_empty() {
                        return p.to_string();
                    }
                }
            }
        }
    }
    arguments.to_string()
}

fn truncate_bytes(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// attachToolResult puts a tool's output on the oldest unfinished tool
/// call; an unmatched result becomes its own block.
pub fn attach_tool_result(blocks: &mut Vec<Block>, content: &str, diff: &str) {
    let id = atom_tools::parse_dispatch_session_id(content);
    for b in blocks.iter_mut() {
        if b.kind == BlockKind::Tool && !b.tool_done {
            b.result = content.to_string();
            b.tool_done = true;
            if !diff.is_empty() {
                b.diff = diff.to_string();
            }
            if !id.is_empty() {
                b.session_id = id.clone();
            }
            b.lines = None;
            return;
        }
    }
    blocks.push(Block {
        kind: BlockKind::Tool,
        result: content.to_string(),
        tool_done: true,
        diff: diff.to_string(),
        session_id: id,
        ..Default::default()
    });
}

/// toolResultSummary strips an embedded unified diff from the result so
/// the colored diff renders once in the same output region.
pub fn tool_result_summary<'a>(result: &'a str, diff: &str) -> &'a str {
    if diff.is_empty() {
        return result;
    }
    result
        .strip_suffix(diff)
        .map(|r| r.trim())
        .unwrap_or(result)
}

/// stripReadMetadata drops a leading "hash: <hex>" (or similar metadata)
/// line that some read_file results prefix, so the rendered output starts
/// at the file content itself.
pub fn strip_read_metadata(s: &str) -> String {
    let mut lines = s.split('\n');
    let first = lines.next();
    if first.is_some_and(|l| l.trim_start().starts_with("hash:")) {
        let rest: Vec<&str> = lines.collect();
        // Drop a single blank line that often follows the metadata line.
        let start = if rest.first().is_some_and(|l| l.is_empty()) {
            1
        } else {
            0
        };
        rest[start..].join("\n")
    } else {
        s.to_string()
    }
}

/// langFromAction returns the file-path portion of a read_file action,
/// stripping a trailing "[offset=…, limit=…]" bracket so syntax
/// detection (which keys off the extension) still works.
pub fn lang_from_action(action: &str) -> &str {
    if let Some(idx) = action.rfind(" [") {
        &action[..idx]
    } else {
        action
    }
}

pub fn hidden_output_tool(name: &str) -> bool {
    matches!(
        name,
        "bash" | "grep" | "glob" | "vector_search" | "read_file"
    )
}

/// write/edit success lines the TUI drops; the diff or header suffices.
pub fn file_change_byte_summary(name: &str, summary: &str) -> bool {
    match name {
        "write_file" => summary.starts_with("wrote "),
        "edit_file" => summary.starts_with("edited ") && summary.contains("replaced "),
        _ => false,
    }
}

pub fn file_path_tool(name: &str) -> bool {
    matches!(name, "read_file" | "write_file" | "edit_file")
}

// ---------------------------------------------------------------------------
// Wrapping helpers.
// ---------------------------------------------------------------------------

/// wrappedLineCount: display lines after wrapping to width. Empty is 0.
pub fn wrapped_line_count(s: &str, width: usize) -> usize {
    if s.is_empty() {
        return 0;
    }
    let width = width.max(1);
    display_line_count(&atom_core::render::links::wrap_linked(s, width, "", ""))
}

pub fn display_line_count(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let mut lines: Vec<&str> = s.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.len()
}

/// collapseDisplay shortens already-wrapped tool output to preview lines.
pub fn collapse_lines(
    mut lines: Vec<Line<'static>>,
    preview: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    if expanded || lines.len() <= preview {
        return lines;
    }
    lines.truncate(preview);
    lines
}

// ---------------------------------------------------------------------------
// Rendering. Colors come from atom_core::render via ANSI conversion.
// ---------------------------------------------------------------------------

/// Renders one conversation block wrapped to width. The viewport owns
/// vertical padding and spacing between blocks.
pub fn render_block(
    b: &mut Block,
    width: usize,
    _show_reasoning: bool,
    spinner_frame: &str,
) -> Vec<Line<'static>> {
    match b.kind {
        BlockKind::User => {
            let inner = width.saturating_sub(2 * PAD_CELL).max(1);
            // Render the full body first; collapse only when the wrapped
            // rows exceed the preview budget so short messages stay
            // untouched (and every collapsed card is expandable).
            let mut body = render_user_body(&b.text, &b.images, inner, None);
            if !b.expanded && body.len() > USER_PREVIEW_LINES {
                // Re-render capped at the preview budget: the cap applies
                // to wrapped rows (one extremely long pasted line
                // collapses) and never splits an image placeholder.
                body = render_user_body(&b.text, &b.images, inner, Some(USER_PREVIEW_LINES - 1));
                body.push(vec![Span::styled(
                    USER_EXPAND_HINT,
                    ansi::style_user().fg(ansi::c_muted()),
                )]);
            }
            let mut out = vec![pad_row(width, ansi::style_user())];
            for row in body {
                out.push(box_row(row, width, ansi::style_user()));
            }
            out.push(pad_row(width, ansi::style_user()));
            out
        }
        BlockKind::Assistant => {
            if b.text.is_empty() {
                return Vec::new();
            }
            let md = atom_core::render::markdown::render_markdown(&b.text, width.max(1));
            let mut out = ansi::ansi_to_lines(&md);
            if !b.model.is_empty() {
                let footer = match b.turn_duration {
                    Some(duration) => {
                        format!("{} | {}", b.model, format_turn_duration(duration))
                    }
                    None => b.model.clone(),
                };
                out.extend(
                    crate::prompt::wrap_plain(&footer, width.max(1))
                        .into_iter()
                        .map(|row| Line::from(Span::styled(row, ansi::style_reasoning()))),
                );
            }
            out
        }
        BlockKind::Reasoning => {
            let mut out = vec![Line::from(Span::styled(
                reasoning_label(b, spinner_frame),
                ansi::style_reasoning(),
            ))];
            if b.expanded && !b.text.is_empty() {
                let body = atom_core::render::links::wrap_linked(
                    &b.text,
                    width.max(1),
                    atom_core::render::colors::COLOR_MUTED,
                    "",
                );
                let styled = format!(
                    "{}{}\x1b[39m",
                    atom_core::render::colors::ansi_fg(atom_core::render::colors::COLOR_MUTED),
                    body
                );
                out.extend(ansi::ansi_to_lines(&styled));
            }
            out
        }
        BlockKind::Compaction => {
            if b.active {
                return vec![Line::from(Span::styled(
                    compaction_label(b, spinner_frame),
                    ansi::style_reasoning(),
                ))];
            }
            let mut out = if b.text.is_empty() {
                Vec::new()
            } else {
                let md = atom_core::render::markdown::render_markdown(&b.text, width.max(1));
                ansi::ansi_to_lines(&md)
            };
            out.extend(
                crate::prompt::wrap_plain(&compaction_label(b, spinner_frame), width.max(1))
                    .into_iter()
                    .map(|row| Line::from(Span::styled(row, ansi::style_reasoning()))),
            );
            out
        }
        BlockKind::Tool => render_tool_block(b, width, spinner_frame),
        BlockKind::Error => {
            let text = format!("error: {}", b.text);
            let body = atom_core::render::links::wrap_linked(
                &text,
                width.max(1),
                atom_core::render::colors::COLOR_SECONDARY,
                "",
            );
            let styled = format!(
                "{}{}\x1b[39m",
                atom_core::render::colors::ansi_fg(atom_core::render::colors::COLOR_SECONDARY),
                body
            );
            ansi::ansi_to_lines(&styled)
        }
    }
}

/// Splits text at `[IMG n]` markers. Each piece is either plain text or
/// a marker referencing one of the block's images.
enum UserSegment {
    Text(String),
    Image(usize),
}

fn split_user_segments(text: &str) -> Vec<UserSegment> {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\[IMG (\d+)\]").expect("static regex"));
    let mut out: Vec<UserSegment> = Vec::new();
    let mut last = 0usize;
    for m in RE.find_iter(text) {
        if m.start() > last {
            out.push(UserSegment::Text(text[last..m.start()].to_string()));
        }
        // Capture group 1 is the digit run; fall back to 0 if the
        // extracted value is huge or empty.
        let n = m
            .as_str()
            .trim_start_matches("[IMG ")
            .trim_end_matches(']')
            .parse::<usize>()
            .unwrap_or(0);
        out.push(UserSegment::Image(n));
        last = m.end();
    }
    if last < text.len() {
        out.push(UserSegment::Text(text[last..].to_string()));
    }
    out
}

/// renderUserBody wraps text segments individually and splices kitty
/// preview grids (or chip fallbacks) where `[IMG n]` markers appeared.
/// Text wrapping ignores image widths so the layout stays tight even
/// when the marker is mid-sentence.
///
/// `preview` caps the returned body rows for a collapsed card. The cap
/// is applied to wrapped rows, so one extremely long pasted line
/// collapses; images are treated atomically — a placeholder grid that
/// would straddle the cap is dropped whole rather than cut halfway.
fn render_user_body(
    text: &str,
    images: &[PendingImage],
    inner: usize,
    preview: Option<usize>,
) -> Vec<Vec<Span<'static>>> {
    let segments = split_user_segments(text);
    if segments.is_empty() {
        return vec![Vec::new()];
    }
    let mut by_num: std::collections::HashMap<usize, &PendingImage> =
        images.iter().map(|p| (p.num, p)).collect();
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    'segments: for seg in segments {
        match seg {
            UserSegment::Text(s) => {
                let body = atom_core::render::links::wrap_linked(
                    &s,
                    inner,
                    atom_core::render::colors::COLOR_FOREGROUND,
                    atom_core::render::colors::COLOR_CARD_LIGHT,
                );
                let lines = ansi::ansi_to_lines(&body);
                if lines.is_empty() {
                    // Empty text segment still occupies a row so the
                    // surrounding image rows don't collapse together.
                    if preview.is_some_and(|limit| out.len() >= limit) {
                        break 'segments;
                    }
                    out.push(Vec::new());
                } else {
                    for row in lines {
                        if preview.is_some_and(|limit| out.len() >= limit) {
                            break 'segments;
                        }
                        out.push(row.spans);
                    }
                }
            }
            UserSegment::Image(num) => {
                if let Some(img) = by_num.remove(&num) {
                    if preview::kitty_terminal() && img.cols > 0 && img.rows > 0 {
                        let grid = preview::placeholder_grid(img.num, img.cols, img.rows);
                        let rows: Vec<Vec<Span<'static>>> = grid
                            .split('\n')
                            .map(|row| ansi::ansi_to_line(row).spans)
                            .collect();
                        if preview.is_some_and(|limit| out.len() + rows.len() > limit) {
                            // The placeholder does not fit the preview
                            // whole; drop it rather than splitting it.
                            continue;
                        }
                        out.extend(rows);
                    } else {
                        // Fallback: inline chip with the same character
                        // count as the marker would have occupied.
                        if preview.is_some_and(|limit| out.len() + 1 > limit) {
                            continue;
                        }
                        out.push(vec![Span::styled(
                            preview::image_chip(num),
                            ansi::style_img_chip(),
                        )]);
                    }
                } else {
                    // Unknown marker (e.g. image stripped from history):
                    // render as a small chip so the user sees something.
                    if preview.is_some_and(|limit| out.len() + 1 > limit) {
                        continue;
                    }
                    out.push(vec![Span::styled(
                        preview::image_chip(num),
                        ansi::style_img_chip(),
                    )]);
                }
            }
        }
    }
    out
}

fn format_turn_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(60) {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    let total_seconds = duration.as_secs_f64().round() as u64;
    let hours = total_seconds / 3600;
    let minutes = total_seconds % 3600 / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn render_tool_block(b: &mut Block, width: usize, spinner_frame: &str) -> Vec<Line<'static>> {
    const PAD: usize = 1;
    let inner = width.saturating_sub(2 * PAD).max(1);
    let running = !b.tool_done;

    // Header row spans (built with ratatui styles directly for exact
    // widths), padded to inner then boxed below.
    let mut left: Vec<Span> = Vec::new();
    if !b.title.is_empty() {
        left.push(Span::styled(b.title.clone(), ansi::style_tool_name()));
    }
    let collapsible = b.tool_collapsible(inner, inner);
    let mut right: Vec<Span> = Vec::new();
    if collapsible {
        right.push(Span::styled(
            if b.expanded {
                "collapse".to_string()
            } else {
                "expand".to_string()
            },
            ansi::style_tool_hint(),
        ));
    }
    if running {
        if !right.is_empty() {
            right.push(Span::styled(" ".to_string(), ansi::style_tool_hint()));
        }
        right.push(Span::styled(
            spinner_frame.to_string(),
            ansi::style_tool_hint(),
        ));
    }
    let title_w: usize = left.iter().map(span_width).sum();
    let right_w: usize = right.iter().map(span_width).sum();
    let mut action_budget = inner.saturating_sub(title_w).saturating_sub(right_w);
    if !b.title.is_empty() && !b.text.is_empty() {
        action_budget = action_budget.saturating_sub(1);
    }
    if right_w > 0 {
        action_budget = action_budget.saturating_sub(1);
    }
    if !b.text.is_empty() && action_budget > 0 {
        let action = atom_core::render::highlight::truncate_width(&b.text, action_budget);
        if !b.title.is_empty() {
            left.push(Span::styled(" ".to_string(), ansi::style_tool_hint()));
        }
        if file_path_tool(&b.resolved_tool_name()) {
            let linked = atom_core::render::links::linkify_path(
                &action,
                atom_core::render::colors::COLOR_FOREGROUND,
                atom_core::render::colors::COLOR_CARD_DARK,
            );
            left.extend(split_ansi_spans(&linked, ansi::style_tool()));
        } else {
            left.push(Span::styled(action, ansi::style_tool()));
        }
    }
    let left_w: usize = left.iter().map(span_width).sum();
    let gap = inner.saturating_sub(left_w).saturating_sub(right_w);

    // Body rows (already wrapped to inner by their producers).
    let mut body: Vec<Line<'static>> = Vec::new();
    let name = b.resolved_tool_name();
    let hidden = hidden_output_tool(&name);
    if hidden {
        if b.expanded {
            if !b.diff.is_empty() {
                let d = atom_core::render::diff::render_diff(&b.diff, &b.text, inner, false);
                body.extend(ansi::ansi_to_lines(&d));
            } else if !b.result.is_empty() {
                if name == "read_file" {
                    let summary = strip_read_metadata(tool_result_summary(&b.result, &b.diff));
                    if !summary.is_empty() {
                        let lang = lang_from_action(&b.text);
                        let rendered =
                            atom_core::render::highlight::highlight_code(&summary, lang, inner);
                        body.extend(ansi::ansi_to_lines(&rendered));
                    }
                } else {
                    let w = atom_core::render::links::wrap_linked(
                        &b.result,
                        inner,
                        atom_core::render::colors::COLOR_FOREGROUND,
                        atom_core::render::colors::COLOR_CARD_DARK,
                    );
                    body.extend(ansi::ansi_to_lines(&w));
                }
            }
        }
    } else if !b.diff.is_empty() {
        let d = atom_core::render::diff::render_diff(&b.diff, &b.text, inner, name == "edit_file");
        body = collapse_lines(
            ansi::ansi_to_lines(&d),
            TOOL_RESULT_PREVIEW_LINES,
            b.expanded,
        );
    } else {
        let summary = tool_result_summary(&b.result, &b.diff).to_string();
        if !summary.is_empty() && !file_change_byte_summary(&name, &summary) {
            let rendered = if name == "read_file" {
                atom_core::render::highlight::highlight_code(&summary, &b.text, inner)
            } else {
                atom_core::render::links::wrap_linked(
                    &summary,
                    inner,
                    atom_core::render::colors::COLOR_FOREGROUND,
                    atom_core::render::colors::COLOR_CARD_DARK,
                )
            };
            body = collapse_lines(
                ansi::ansi_to_lines(&rendered),
                TOOL_RESULT_PREVIEW_LINES,
                b.expanded,
            );
        }
    }

    let mut out: Vec<Line<'static>> = Vec::with_capacity(body.len() + 3);
    out.push(pad_row(width, ansi::style_tool()));
    let mut header_spans = left;
    header_spans.push(Span::styled(" ".repeat(gap), ansi::style_tool_hint()));
    header_spans.extend(right);
    out.push(box_row(header_spans, width, ansi::style_tool()));
    for row in body {
        let spans = if row.spans.is_empty() {
            vec![Span::styled(String::new(), ansi::style_tool())]
        } else {
            row.spans
        };
        out.push(box_row(spans, width, ansi::style_tool()));
    }
    out.push(pad_row(width, ansi::style_tool()));
    out
}

fn span_width(s: &Span) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.content.width()
}

fn pad_row(width: usize, style: ratatui::style::Style) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), style))
}

fn box_row(spans: Vec<Span<'static>>, width: usize, style: ratatui::style::Style) -> Line<'static> {
    let used: usize = spans.iter().map(span_width).sum();
    let pad = width.saturating_sub(used);
    let mut all = Vec::with_capacity(spans.len() + 1);
    all.push(Span::styled(" ".repeat(PAD_CELL), style));
    all.extend(
        spans
            .into_iter()
            .map(|span| Span::styled(span.content, style.patch(span.style))),
    );
    if pad >= PAD_CELL {
        all.push(Span::styled(" ".repeat(pad - PAD_CELL), style));
    }
    Line::from(all)
}

const PAD_CELL: usize = 1;

/// Splits an ANSI string into styled spans using the given base style
/// for any unstyled runs.
fn split_ansi_spans(s: &str, base: ratatui::style::Style) -> Vec<Span<'static>> {
    let line = ansi::ansi_to_line(s);
    if line.spans.is_empty() {
        return vec![Span::styled(String::new(), base)];
    }
    line.spans
        .into_iter()
        .map(|span| Span::styled(span.content, base.patch(span.style)))
        .collect()
}

// ---------------------------------------------------------------------------
// Live labels.
// ---------------------------------------------------------------------------

/// reasoningLabel: muted "Thinking" line, with duration once finished.
pub fn reasoning_label(b: &Block, spinner_frame: &str) -> String {
    if b.active {
        return active_label("Thinking", spinner_frame);
    }
    if let Some(d) = b.dur {
        if d > Duration::ZERO {
            return format!("Thinking ({:.1}s)", d.as_secs_f64());
        }
    }
    "Thinking".to_string()
}

/// compactionLabel shows the active phase or the muted footer placed below
/// completed compaction output ("Compaction | deepseek-v4-flash:0731 | 12.3s").
pub fn compaction_label(b: &Block, spinner_frame: &str) -> String {
    if b.active {
        let mut label = active_label("Compacting", spinner_frame);
        if !b.model.is_empty() {
            label.push_str(&format!(" | {}", b.model));
        }
        return label;
    }

    let mut parts = vec!["Compaction".to_string()];
    if !b.model.is_empty() {
        parts.push(b.model.clone());
    }
    if let Some(duration) = b.dur.filter(|duration| *duration > Duration::ZERO) {
        parts.push(format_turn_duration(duration));
    }
    parts.join(" | ")
}

/// activeLabel keeps every live-state label on the same columns.
pub fn active_label(label: &str, spinner_frame: &str) -> String {
    format!("{spinner_frame} {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_display_name_snake_case() {
        assert_eq!(tool_display_name("bash"), "Bash");
        assert_eq!(tool_display_name("edit_file"), "Edit File");
        assert_eq!(tool_display_name("vector_search"), "Vector Search");
    }

    #[test]
    fn tool_display_name_uses_selected_web_search_provider() {
        use atom_core::config::WebSearchConfig;
        let config = |server: &str| WebSearchConfig {
            server: server.into(),
            tool: String::new(),
        };
        assert_eq!(
            tool_display_name_for("web_search", &config("parallel")),
            "Parallel Web Search"
        );
        assert_eq!(
            tool_display_name_for("web_search", &config("ollama")),
            "Ollama Web Search"
        );
        assert_eq!(
            tool_display_name_for("web_search_exa", &config("exa")),
            "Exa Web Search"
        );
        // An exa selection does not rename the generic parallel tool id,
        // and unrelated tools keep their snake-case titles.
        assert_eq!(
            tool_display_name_for("web_search", &config("exa")),
            "Web Search"
        );
        assert_eq!(tool_display_name_for("bash", &config("exa")), "Bash");
        assert_eq!(
            tool_display_name_for("web_search", &config("custom-mcp")),
            "Web Search"
        );
    }

    #[test]
    fn compaction_label_shows_model_and_duration() {
        let mut b = Block {
            kind: BlockKind::Compaction,
            model: "deepseek-v4-flash:0731".into(),
            ..Default::default()
        };
        assert_eq!(
            compaction_label(&b, "*"),
            "Compaction | deepseek-v4-flash:0731"
        );
        b.active = true;
        assert_eq!(
            compaction_label(&b, "*"),
            "* Compacting | deepseek-v4-flash:0731"
        );
        b.active = false;
        b.dur = Some(Duration::from_millis(12_300));
        assert_eq!(
            compaction_label(&b, "*"),
            "Compaction | deepseek-v4-flash:0731 | 12.3s"
        );
        b.model.clear();
        b.dur = None;
        assert_eq!(compaction_label(&b, "*"), "Compaction");
        b.dur = Some(Duration::from_millis(8300));
        assert_eq!(compaction_label(&b, "*"), "Compaction | 8.3s");
    }

    #[test]
    fn completed_compaction_renders_muted_metadata_below_output() {
        let mut b = Block {
            kind: BlockKind::Compaction,
            text: "Compacted summary".into(),
            model: "compact-model".into(),
            dur: Some(Duration::from_millis(2300)),
            ..Default::default()
        };

        let lines = render_block(&mut b, 80, true, "*");
        assert_eq!(
            ansi::line_plain(lines.last().unwrap()),
            "Compaction | compact-model | 2.3s"
        );
        assert!(lines[..lines.len() - 1]
            .iter()
            .any(|line| ansi::line_plain(line).contains("Compacted summary")));
        assert_eq!(
            lines.last().unwrap().spans[0].style.fg,
            Some(ansi::c_muted())
        );
    }

    #[test]
    fn persisted_error_message_restores_error_block() {
        let blocks = messages_to_blocks(&[Message {
            role: "error".into(),
            content: "provider returned an empty response".into(),
            ..Default::default()
        }]);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Error);
        assert_eq!(blocks[0].text, "provider returned an empty response");
    }

    #[test]
    fn tool_action_extracts_summaries() {
        assert_eq!(
            tool_action("bash", r#"{"command":"go test ./..."}"#),
            "go test ./..."
        );
        assert_eq!(
            tool_action("grep", r#"{"pattern":"output-test","path":"tui.go"}"#),
            "output-test"
        );
        assert_eq!(
            tool_action("read_file", r#"{"path":"/tmp/x.go"}"#),
            "/tmp/x.go"
        );
        assert_eq!(
            tool_action("dispatch", r#"{"model":"m","prompt":"Review scenes"}"#),
            "Review scenes"
        );
        assert_eq!(
            tool_action(
                "dispatch",
                r#"{"cancel":true,"session_id":"0123456789abcdef"}"#
            ),
            "cancel 0123456789abcdef"
        );
    }

    #[test]
    fn attach_pairs_results_in_order() {
        let mut blocks = vec![
            Block {
                kind: BlockKind::Tool,
                title: "Bash".into(),
                tool_name: "bash".into(),
                ..Default::default()
            },
            Block {
                kind: BlockKind::Tool,
                title: "Grep".into(),
                tool_name: "grep".into(),
                ..Default::default()
            },
        ];
        attach_tool_result(&mut blocks, "one", "");
        attach_tool_result(&mut blocks, "two", "");
        assert_eq!(blocks[0].result, "one");
        assert_eq!(blocks[1].result, "two");
        attach_tool_result(&mut blocks, "orphan", "");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2].result, "orphan");
    }

    #[test]
    fn empty_result_still_completes_tool() {
        let mut blocks = vec![
            Block {
                kind: BlockKind::Tool,
                title: "Mkdir".into(),
                ..Default::default()
            },
            Block {
                kind: BlockKind::Tool,
                title: "Next".into(),
                ..Default::default()
            },
        ];
        let running = render_block(&mut blocks[0], 40, true, "*");
        assert!(running
            .iter()
            .any(|line| crate::ansi::line_plain(line).contains('*')));
        attach_tool_result(&mut blocks, "", "");
        attach_tool_result(&mut blocks, "done", "");
        assert!(blocks[0].tool_done);
        assert!(blocks[0].result.is_empty());
        assert_eq!(blocks[1].result, "done");
        let finished = render_block(&mut blocks[0], 40, true, "*");
        assert!(!finished
            .iter()
            .any(|line| crate::ansi::line_plain(line).contains('*')));
    }

    #[test]
    fn dispatch_result_sets_session_id() {
        let mut blocks = vec![Block {
            kind: BlockKind::Tool,
            title: "Dispatch".into(),
            tool_name: "dispatch".into(),
            ..Default::default()
        }];
        attach_tool_result(&mut blocks, "id: 0123456789abcdef\nstarted", "");
        assert_eq!(blocks[0].session_id, "0123456789abcdef");
    }

    #[test]
    fn collapsible_hidden_tools_vs_preview_overflow() {
        let mut b = Block {
            kind: BlockKind::Tool,
            tool_name: "bash".into(),
            ..Default::default()
        };
        assert!(!b.tool_collapsible(80, 80), "running bash not collapsible");
        b.result = "done".into();
        assert!(
            b.tool_collapsible(80, 80),
            "finished hidden tool always collapsible"
        );

        let mut small = Block {
            kind: BlockKind::Tool,
            tool_name: "web_search".into(),
            result: "short".into(),
            ..Default::default()
        };
        assert!(!small.tool_collapsible(80, 80));
        small.result = "line\n".repeat(20);
        assert!(small.tool_collapsible(80, 80));
    }

    #[test]
    fn collapse_display_truncates() {
        let long: String = (0..20).map(|_| "row\n").collect();
        let lines = ansi::ansi_to_lines(long.trim_end());
        let got = collapse_lines(lines, TOOL_RESULT_PREVIEW_LINES, false);
        assert_eq!(got.len(), TOOL_RESULT_PREVIEW_LINES);
        let lines = ansi::ansi_to_lines("a\nb");
        assert_eq!(collapse_lines(lines, 8, false).len(), 2);
    }

    #[test]
    fn tool_box_rows_are_full_width() {
        let mut b = Block {
            kind: BlockKind::Tool,
            title: "Bash".into(),
            text: "ls".into(),
            tool_name: "bash".into(),
            result: "file\nlist".into(),
            ..Default::default()
        };
        let lines = render_block(&mut b, 40, true, "*");
        for l in &lines {
            assert_eq!(ansi::line_width(l), 40, "{l:?}");
        }
    }

    #[test]
    fn user_card_has_full_width_padding() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        };

        let lines = render_block(&mut block, 40, true, "*");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(ansi::line_width(line), 40, "{line:?}");
        }
        assert_eq!(lines[0].spans[0].style.bg, Some(ansi::c_card_light()));
        assert_eq!(ansi::line_plain(&lines[1]), format!(" {:<38} ", "hello"));
        assert_eq!(lines[2].spans[0].style.bg, Some(ansi::c_card_light()));
    }

    #[test]
    fn tool_links_keep_the_tool_background() {
        let mut block = Block {
            kind: BlockKind::Tool,
            title: "Fetch".into(),
            tool_name: "webfetch".into(),
            text: "https://example.com".into(),
            result: "See https://example.com/docs for details".into(),
            ..Default::default()
        };

        let lines = render_block(&mut block, 60, true, "*");
        for line in &lines {
            for span in &line.spans {
                assert_eq!(span.style.bg, Some(ansi::c_card_dark()), "{span:?}");
            }
        }
    }

    #[test]
    fn read_file_results_are_syntax_highlighted() {
        let mut block = Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            tool_name: "read_file".into(),
            text: "src/main.rs".into(),
            result: "fn main() { let message: &str = \"hello\"; }".into(),
            expanded: true,
            ..Default::default()
        };

        let lines = render_block(&mut block, 80, true, "*");
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect();
        assert!(
            colors.contains(&ratatui::style::Color::Rgb(232, 160, 122)),
            "missing syntax type color: {lines:?}"
        );
        assert!(
            colors.contains(&ratatui::style::Color::Rgb(216, 201, 176)),
            "missing syntax string color: {lines:?}"
        );
    }

    #[test]
    fn read_file_collapsed_hides_output() {
        let mut block = Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            tool_name: "read_file".into(),
            text: "src/main.rs".into(),
            result: "fn main() { let message: &str = \"hello\"; }".into(),
            ..Default::default()
        };
        let lines = render_block(&mut block, 80, true, "*");
        // Header + padding only; no syntax-highlighted body rows.
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect();
        assert!(
            !colors.contains(&ratatui::style::Color::Rgb(216, 201, 176)),
            "collapsed read_file must not show output: {lines:?}"
        );
    }

    #[test]
    fn read_file_strips_hash_metadata() {
        let mut block = Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            tool_name: "read_file".into(),
            text: "src/main.rs".into(),
            result: "hash: 9f2c1a8d4b7e3f60\nfn main() {}".into(),
            expanded: true,
            ..Default::default()
        };
        let lines = render_block(&mut block, 80, true, "*");
        let plain: String = lines
            .iter()
            .map(crate::ansi::line_plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!plain.contains("hash:"), "hash stripped: {plain}");
        assert!(plain.contains("fn main()"), "content kept: {plain}");
    }

    #[test]
    fn read_file_action_shows_offset_limit_bracket() {
        assert_eq!(
            tool_action("read_file", r#"{"path":"/tmp/x.go","offset":1,"limit":20}"#),
            "/tmp/x.go [offset=1, limit=20]"
        );
        assert_eq!(
            tool_action("read_file", r#"{"path":"/tmp/x.go"}"#),
            "/tmp/x.go"
        );
        assert_eq!(
            tool_action("read_file", r#"{"path":"/tmp/x.go","offset":5}"#),
            "/tmp/x.go [offset=5]"
        );
    }

    #[test]
    fn read_file_highlight_ignores_offset_bracket() {
        // The action text carries a "[offset=…, limit=…]" bracket, but
        // syntax detection must still key off the .rs extension.
        let mut block = Block {
            kind: BlockKind::Tool,
            title: "Read".into(),
            tool_name: "read_file".into(),
            text: "src/main.rs [offset=1, limit=20]".into(),
            result: "fn main() { let message: &str = \"hello\"; }".into(),
            expanded: true,
            ..Default::default()
        };
        let lines = render_block(&mut block, 80, true, "*");
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect();
        assert!(
            colors.contains(&ratatui::style::Color::Rgb(232, 160, 122)),
            "bracket must not break syntax detection: {lines:?}"
        );
        assert!(
            colors.contains(&ratatui::style::Color::Rgb(216, 201, 176)),
            "string color present: {lines:?}"
        );
    }

    #[test]
    fn reasoning_labels() {
        let mut b = Block::new(BlockKind::Reasoning);
        b.active = true;
        assert_eq!(reasoning_label(&b, "*"), "* Thinking");
        b.active = false;
        b.dur = Some(Duration::from_millis(8300));
        assert_eq!(reasoning_label(&b, "*"), "Thinking (8.3s)");
        b.dur = None;
        assert_eq!(reasoning_label(&b, "*"), "Thinking");
    }

    #[test]
    fn messages_to_blocks_shapes() {
        let msgs = vec![
            Message {
                role: "user".into(),
                content: "hello".into(),
                ..Default::default()
            },
            Message {
                role: "assistant".into(),
                content: "hi".into(),
                reasoning: "hmm".into(),
                reasoning_ms: 1500,
                model: "model-b".into(),
                duration_ms: 134_600,
                ..Default::default()
            },
            Message {
                role: "compaction".into(),
                content: "summary".into(),
                model: "compact-model".into(),
                duration_ms: 2300,
                ..Default::default()
            },
            Message {
                role: "nudge".into(),
                content: "continue".into(),
                ..Default::default()
            },
        ];
        let blocks = messages_to_blocks(&msgs);
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].kind, BlockKind::User);
        assert_eq!(blocks[1].kind, BlockKind::Reasoning);
        assert_eq!(blocks[1].dur, Some(Duration::from_millis(1500)));
        assert_eq!(blocks[2].kind, BlockKind::Assistant);
        assert_eq!(blocks[2].model, "model-b");
        assert_eq!(
            blocks[2].turn_duration,
            Some(Duration::from_millis(134_600))
        );
        assert_eq!(blocks[3].kind, BlockKind::Compaction);
        assert_eq!(blocks[3].model, "compact-model");
        assert_eq!(blocks[3].dur, Some(Duration::from_millis(2300)));
    }

    #[test]
    fn messages_to_blocks_preserves_user_images() {
        let msgs = vec![Message {
            role: "user".into(),
            content: "[IMG 1] hello".into(),
            images: vec![atom_core::types::ImageData {
                mime: "image/png".into(),
                data: "AAAA".into(),
            }],
            ..Default::default()
        }];
        let blocks = messages_to_blocks(&msgs);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::User);
        assert_eq!(blocks[0].text, "[IMG 1] hello");
        assert_eq!(blocks[0].images.len(), 1);
        assert_eq!(blocks[0].images[0].img.data, "AAAA");
        // Assigning nums should give the unassigned image a unique id.
        let mut blocks = blocks;
        assign_block_image_nums(&mut blocks, &[]);
        assert_eq!(blocks[0].images[0].num, 1);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::ansi;

    #[test]
    fn assistant_turn_metadata_renders_below_reply() {
        let mut block = Block {
            kind: BlockKind::Assistant,
            text: "Answer text.".into(),
            model: "model-b".into(),
            turn_duration: Some(Duration::from_millis(134_600)),
            ..Default::default()
        };

        let lines = render_block(&mut block, 60, true, "*");

        assert_eq!(ansi::line_plain(lines.last().unwrap()), "model-b | 2m 15s");
        assert_eq!(
            lines.last().unwrap().spans[0].style,
            ansi::style_reasoning()
        );
    }

    #[test]
    fn assistant_turn_duration_under_a_minute_keeps_tenths() {
        assert_eq!(format_turn_duration(Duration::from_millis(3_140)), "3.1s");
    }

    #[test]
    fn assistant_metadata_and_errors_stay_within_width() {
        let mut assistant = Block {
            kind: BlockKind::Assistant,
            text: "answer".into(),
            model: "provider/model-with-a-very-long-name".into(),
            turn_duration: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let mut error = Block {
            kind: BlockKind::Error,
            text: "a long error message that needs to wrap without losing its prefix".into(),
            ..Default::default()
        };

        for line in render_block(&mut assistant, 20, true, "*")
            .into_iter()
            .chain(render_block(&mut error, 20, true, "*"))
        {
            assert!(
                ansi::line_width(&line) <= 20,
                "line too wide: {:?}",
                ansi::line_plain(&line)
            );
        }
    }

    #[test]
    fn expand_toggle_grows_rendered_output() {
        let long: String = std::iter::repeat("output row\n").take(30).collect();
        let mut b = Block {
            kind: BlockKind::Tool,
            title: "Web Search".into(),
            tool_name: "web_search".into(),
            text: "query".into(),
            result: long.trim_end().to_string(),
            ..Default::default()
        };
        let collapsed = render_block(&mut b, 60, true, "*");
        b.expanded = true;
        b.lines = None;
        let expanded = render_block(&mut b, 60, true, "*");
        assert_eq!(
            collapsed.len(),
            crate::blocks::TOOL_RESULT_PREVIEW_LINES + 3
        );
        assert!(expanded.len() > collapsed.len() + 10);
    }

    #[test]
    fn markdown_wraps_within_width() {
        use unicode_width::UnicodeWidthStr;
        let md = "# Heading\n\nA paragraph with a [link](https://example.com) and `code span`, long enough to wrap several times over the requested width in the rendered output.\n\n- one\n- two\n";
        let out = atom_core::render::markdown::render_markdown(md, 40);
        for line in ansi::ansi_to_lines(&out) {
            assert!(
                ansi::line_width(&line) <= 40,
                "line too wide: {:?}",
                ansi::line_plain(&line)
            );
        }
        let _ = UnicodeWidthStr::width("");
    }

    #[test]
    fn reasoning_block_produces_muted_style() {
        let mut b = Block::new(BlockKind::Reasoning);
        b.text = "The user just sent a message that wraps onto several lines".into();
        b.expanded = true;
        let lines = render_block(&mut b, 20, true, "*");
        assert!(lines.len() > 2, "reasoning should wrap onto multiple lines");
        assert_eq!(ansi::line_plain(&lines[0]), "Thinking");
        for line in &lines {
            for span in &line.spans {
                if !span.content.is_empty() {
                    assert_eq!(
                        span.style.fg,
                        Some(ratatui::style::Color::Rgb(102, 102, 102)),
                        "reasoning text should be muted, got {:?} for {:?}",
                        span.style.fg,
                        span.content
                    );
                }
            }
        }
    }

    #[test]
    fn expanded_active_reasoning_keeps_a_muted_spinner_header() {
        let mut b = Block {
            kind: BlockKind::Reasoning,
            text: "still reasoning".into(),
            expanded: true,
            active: true,
            ..Default::default()
        };

        let lines = render_block(&mut b, 40, true, "*");

        assert_eq!(ansi::line_plain(&lines[0]), "* Thinking");
        assert_eq!(lines[0].spans[0].style.fg, Some(ansi::c_muted()));
        assert!(lines
            .iter()
            .any(|line| ansi::line_plain(line) == "still reasoning"));
    }

    #[test]
    fn active_compaction_renders_in_the_viewport() {
        let mut b = Block {
            kind: BlockKind::Compaction,
            active: true,
            ..Default::default()
        };

        let lines = render_block(&mut b, 40, true, "*");

        assert_eq!(lines.len(), 1);
        assert_eq!(ansi::line_plain(&lines[0]), "* Compacting");
    }

    fn fake_image(num: usize) -> PendingImage {
        PendingImage {
            img: atom_core::types::ImageData {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
            name: format!("img{num}"),
            cols: preview::PREVIEW_COLS,
            rows: preview::PREVIEW_ROWS,
            num,
        }
    }

    #[test]
    fn user_block_with_no_images_keeps_text_only_layout() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        // Top + body + bottom padding rows.
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(ansi::line_width(line), 40, "{line:?}");
        }
    }

    #[test]
    fn user_block_splits_text_at_image_markers() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "[IMG 1] hello [IMG 2]".into(),
            images: vec![fake_image(1), fake_image(2)],
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        // Padding row + image 1 rows + text + image 2 rows + padding row.
        assert!(lines.len() > 4);
        // The image marker text is gone from any line.
        for line in &lines {
            let plain = ansi::line_plain(line);
            assert!(!plain.contains("[IMG"));
        }
        // The "hello" line still appears in the body.
        let body_lines: Vec<_> = lines
            .iter()
            .map(ansi::line_plain)
            .filter(|s| s.contains("hello"))
            .collect();
        assert_eq!(body_lines.len(), 1);
    }

    #[test]
    fn long_user_message_collapses_to_preview_with_hint() {
        // A single extremely long pasted line wraps into many rendered
        // rows and must collapse based on those rows, not source bytes.
        let mut block = Block {
            kind: BlockKind::User,
            text: "word ".repeat(400),
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        // Top pad + (content rows + hint) + bottom pad.
        assert_eq!(lines.len(), USER_PREVIEW_LINES + 2);
        let hint = ansi::line_plain(&lines[USER_PREVIEW_LINES]);
        assert!(hint.contains(USER_EXPAND_HINT), "hint row: {hint:?}");
        for line in &lines {
            assert_eq!(ansi::line_width(line), 40, "{line:?}");
        }
    }

    #[test]
    fn expanded_user_card_shows_the_full_message() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "word ".repeat(400),
            expanded: true,
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        let inner = 40 - 2 * PAD_CELL;
        let full = atom_core::render::links::wrap_linked(
            &block.text,
            inner,
            atom_core::render::colors::COLOR_FOREGROUND,
            atom_core::render::colors::COLOR_CARD_LIGHT,
        );
        assert_eq!(lines.len(), ansi::ansi_to_lines(&full).len() + 2);
        for line in &lines {
            let plain = ansi::line_plain(line);
            assert!(!plain.contains(USER_EXPAND_HINT), "no hint when expanded");
        }
    }

    #[test]
    fn short_user_message_is_not_collapsed() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        assert_eq!(lines.len(), 3);
        assert!(ansi::line_plain(&lines[1]).contains("hello"));
    }

    #[test]
    fn user_cache_invalidates_when_expansion_changes() {
        let mut block = Block {
            kind: BlockKind::User,
            text: "word ".repeat(400),
            ..Default::default()
        };
        let rendered = render_block(&mut block, 40, true, "*");
        block.lines = Some(rendered.into_iter().map(Arc::new).collect());
        block.line_width = 40;
        block.line_show_r = true;
        block.line_expanded = block.expanded;
        assert!(block.lines_valid(40, true));
        block.expanded = true;
        assert!(
            !block.lines_valid(40, true),
            "expanding a user card must invalidate its cached lines"
        );
    }

    #[test]
    fn user_preview_never_splits_an_image() {
        // Five text rows followed by an image: in kitty mode the 3-row
        // placeholder straddles the 7-row preview boundary and must be
        // dropped whole (never cut to a partial grid); in chip mode the
        // 1-row chip fits. Either way the card caps at preview + hint.
        let mut block = Block {
            kind: BlockKind::User,
            text: format!(
                "{}[IMG 1] {}",
                "row\n".repeat(4) + "row",
                "word ".repeat(400)
            ),
            images: vec![fake_image(1)],
            ..Default::default()
        };
        let lines = render_block(&mut block, 40, true, "*");
        assert_eq!(lines.len(), USER_PREVIEW_LINES + 2);
        assert!(ansi::line_plain(&lines[USER_PREVIEW_LINES]).contains(USER_EXPAND_HINT));
        let body = &lines[1..lines.len() - 1];
        let placeholder_rows = body
            .iter()
            .filter(|l| ansi::line_plain(l).contains('\u{10EEEE}'))
            .count();
        if preview::kitty_terminal() {
            // The whole grid is present or entirely absent, never partial.
            assert!(
                placeholder_rows == 0 || placeholder_rows == preview::PREVIEW_ROWS,
                "image must be atomic, saw {placeholder_rows} placeholder rows"
            );
        } else {
            let chips = body
                .iter()
                .filter(|l| ansi::line_plain(l).contains("IMG 1"))
                .count();
            assert!(chips <= 1, "chip must be atomic, saw {chips}");
        }
    }

    #[test]
    fn assign_block_image_nums_avoids_reserved_and_renumbers_zero() {
        let mut blocks = vec![
            Block {
                kind: BlockKind::User,
                images: vec![fake_image(0), fake_image(2)],
                ..Default::default()
            },
            Block {
                kind: BlockKind::User,
                images: vec![fake_image(0)],
                ..Default::default()
            },
        ];
        // Reserved id 2 collides with the second image's existing num,
        // so it gets reassigned to the next free slot.
        assign_block_image_nums(&mut blocks, &[2]);
        let nums: Vec<usize> = blocks
            .iter()
            .flat_map(|b| b.images.iter().map(|i| i.num))
            .collect();
        assert_eq!(nums, vec![1, 3, 4]);
        // Without the collision, an existing num is preserved and only
        // the unassigned (zero) entries get fresh ids.
        let mut blocks = vec![
            Block {
                kind: BlockKind::User,
                images: vec![fake_image(0), fake_image(7)],
                ..Default::default()
            },
            Block {
                kind: BlockKind::User,
                images: vec![fake_image(0)],
                ..Default::default()
            },
        ];
        assign_block_image_nums(&mut blocks, &[2]);
        let nums: Vec<usize> = blocks
            .iter()
            .flat_map(|b| b.images.iter().map(|i| i.num))
            .collect();
        assert_eq!(nums, vec![1, 7, 3]);
    }
}
