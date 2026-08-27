//! view.rs renders the whole frame into the ratatui buffer: the
//! conversation viewport (windowed by scroll offset, with mouse-drag
//! selection wash and footer-menu overlays), the animated splash, the
//! bordered prompt with image previews, the status row, the
//! full-screen overlays, and the sandbox approval box.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Clear};

use crate::ansi;
use crate::app::{App, ApprovalPrompt};
use crate::overlays::{self, OverlayKind, PickerKind};
use crate::preview;
use crate::prompt::wrap_plain;

/// Geometry of the fixed footer chrome, in screen rows (0-based tops).
pub struct Layout {
    pub viewport_h: usize,
    pub prompt_top_y: usize,
    pub preview_y: usize,
    pub status_y: usize,
    /// Row of the card-dark cwd footer (the old status bottom padding).
    pub cwd_y: usize,
}

impl Layout {
    /// Pure geometry used by tests and hit-testing.
    pub fn compute_static(
        _width: u16,
        height: u16,
        input_h: usize,
        preview_rows: usize,
        status_rows: usize,
    ) -> (usize, usize) {
        // returns (viewport_h, prompt_top)
        let reserved = status_rows
            + crate::app::STATUS_FOOTER_ROWS
            + 2 * crate::app::PROMPT_PAD
            + input_h
            + preview_rows;
        let vp_h = (height as usize).saturating_sub(reserved).max(1);
        let prompt_top = (height as usize).saturating_sub(
            status_rows
                + crate::app::STATUS_FOOTER_ROWS
                + preview_rows
                + input_h
                + 2 * crate::app::PROMPT_PAD,
        );
        (vp_h, prompt_top)
    }

    pub fn compute(app: &App) -> Self {
        let status_rows = crate::statusbar::status_bar_rows(app);
        let input_h = app.input_height();
        let preview_rows = preview::preview_row_count(app);
        let reserved = status_rows
            + crate::app::STATUS_FOOTER_ROWS
            + 2 * crate::app::PROMPT_PAD
            + input_h
            + preview_rows;
        let viewport_h = (app.height as usize).saturating_sub(reserved).max(1);
        // The prompt's top border sits directly below the viewport; the
        // former working row now shares the status bar (see statusbar.rs),
        // so no chrome is reserved above the prompt.
        let prompt_top_y = viewport_h;
        let preview_y = prompt_top_y + crate::app::PROMPT_PAD + input_h;
        let status_y = preview_y + preview_rows + crate::app::PROMPT_PAD;
        let cwd_y = status_y + status_rows;
        Layout {
            viewport_h,
            prompt_top_y,
            preview_y,
            status_y,
            cwd_y,
        }
    }
}

/// Renders one full frame of the app into the buffer. Returns the
/// terminal cursor position when it belongs over the prompt.
pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    // Sync terminal size before laying out: crossterm only delivers
    // Resize events after startup, so App's boot defaults (80x24) can
    // be stale. Go's Bubble Tea sends an initial WindowSizeMsg before
    // the first View; mirror that here from the render area.
    if area.width != app.width || area.height != app.height {
        app.apply_resize(area.width, area.height);
    }

    // Base paint: theme background everywhere.
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(ansi::frame_style());
        }
    }
    if app.quitting {
        return None;
    }

    if let Some(kind) = app.overlay {
        draw_overlay(app, kind, area, buf);
        return None;
    }

    let geo = Layout::compute(app);
    let inner_w = app.inner_width().min(
        (area.width as usize)
            .saturating_sub(2 * crate::app::TUI_HPAD + crate::app::SCROLLBAR_WIDTH),
    );

    // --- conversation viewport -------------------------------------------
    let vp_rect = Rect::new(
        area.x + 1,
        area.y + crate::app::VIEWPORT_VPAD as u16,
        inner_w as u16,
        app.viewport_height() as u16,
    );
    draw_viewport(app, vp_rect, buf);

    // Footer menus take their own rows between the conversation and prompt,
    // so opening one never paints over conversation content.
    let menu_h = overlays::footer_menu_height(app) as u16;
    if menu_h > 0 {
        draw_footer_menu(
            app,
            Rect::new(vp_rect.x, vp_rect.bottom(), vp_rect.width, menu_h),
            buf,
        );
    }
    draw_scrollbar(
        app,
        Rect::new(
            area.right()
                .saturating_sub(crate::app::SCROLLBAR_WIDTH as u16),
            vp_rect.y,
            crate::app::SCROLLBAR_WIDTH as u16,
            vp_rect.height,
        ),
        buf,
    );

    // --- prompt input ------------------------------------------------------
    let in_h = app.input_height();
    let input_w = app.input_width().min(inner_w).max(1);
    let (rows, cur) = app.input.view(input_w, in_h);
    let pad = card_line(Line::from(""), inner_w, 0);
    write_line(
        buf,
        area.x + 1,
        area.y + geo.prompt_top_y as u16,
        inner_w,
        &pad,
    );
    for (i, row) in rows.iter().enumerate() {
        let y = area.y + (geo.prompt_top_y + crate::app::PROMPT_PAD + i) as u16;
        let chipped = card_line(
            line_with_chips_styled(row, app),
            inner_w,
            crate::app::PROMPT_PAD,
        );
        write_line(buf, area.x + 1, y, inner_w, &chipped);
    }

    // --- preview placeholders inside the input box -------------------------
    let preview_lines = render_previews(app);
    for (i, line) in preview_lines.iter().enumerate() {
        let y = area.y + (geo.preview_y + i) as u16;
        write_line(
            buf,
            area.x + 1,
            y,
            inner_w,
            &card_line(line.clone(), inner_w, crate::app::PROMPT_PAD),
        );
    }
    let bottom_y = geo.status_y.saturating_sub(crate::app::PROMPT_PAD);
    write_line(buf, area.x + 1, area.y + bottom_y as u16, inner_w, &pad);

    // --- status bar ----------------------------------------------------------
    for (i, line) in crate::statusbar::status_bar_lines(app).iter().enumerate() {
        let y = area.y + (geo.status_y + i) as u16;
        write_line(buf, area.x + 1, y, inner_w, line);
    }

    let cwd_line = cwd_footer_line(app, inner_w);
    write_line(
        buf,
        area.x + 1,
        area.y + geo.cwd_y as u16,
        inner_w,
        &cwd_line,
    );

    // --- sandbox approval renders inline as a tool block now ----------------
    // Hide cursor while approval is pending so the user focuses on the block.
    if app.approval.is_some() {
        return None;
    }
    cur.map(|(cx, cy)| {
        let x = area.x + 1 + crate::app::PROMPT_PAD as u16 + cx as u16;
        let y = area.y + (geo.prompt_top_y + crate::app::PROMPT_PAD + cy) as u16;
        (
            x.min(buf.area.width.saturating_sub(1)),
            y.min(buf.area.height.saturating_sub(1)),
        )
    })
}

/// Footer line showing the directory the agent was invoked from.
fn cwd_footer_line(app: &App, width: usize) -> Line<'static> {
    // Show the Knight Rider loader bar while streaming (main + subagents)
    if app.streaming || app.remote_working {
        return crate::spinner::loader_line(app.spinner_frame);
    }
    let path = if app.cwd.is_empty() {
        ".".to_string()
    } else {
        app.cwd.clone()
    };
    let path = crate::statusbar::truncate_width(&path, width.saturating_sub(1));
    Line::from(Span::styled(
        path,
        ratatui::style::Style::new().fg(ansi::c_muted_extra()),
    ))
}

fn card_line(line: Line<'static>, width: usize, horizontal_pad: usize) -> Line<'static> {
    let used = ansi::line_width(&line);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    spans.push(Span::styled(" ".repeat(horizontal_pad), ansi::style_user()));
    spans.extend(
        line.spans
            .into_iter()
            .map(|span| Span::styled(span.content, ansi::style_user().patch(span.style))),
    );
    let occupied = horizontal_pad.saturating_add(used).min(width);
    if occupied < width {
        spans.push(Span::styled(
            " ".repeat(width - occupied),
            ansi::style_user(),
        ));
    }
    Line::from(spans)
}

fn write_line(buf: &mut Buffer, x: u16, y: u16, max_w: usize, line: &Line<'_>) {
    if y < buf.area.top() || y >= buf.area.bottom() || x >= buf.area.right() || max_w == 0 {
        return;
    }
    let mut col = x;
    let mut last_col = None;
    let end = x
        .saturating_add(max_w.min(u16::MAX as usize) as u16)
        .min(buf.area.right());
    for span in &line.spans {
        for ch in span.content.chars() {
            use unicode_width::UnicodeWidthChar;
            if ch == '\t' {
                for _ in 0..4 {
                    if col >= end {
                        return;
                    }
                    buf[(col, y)]
                        .set_char(' ')
                        .set_style(ansi::frame_style().patch(span.style));
                    last_col = Some(col);
                    col += 1;
                }
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w == 0 {
                if let Some(last_col) = last_col {
                    let symbol = format!("{}{ch}", buf[(last_col, y)].symbol());
                    buf[(last_col, y)].set_symbol(&symbol);
                }
                continue;
            }
            if col.saturating_add(w as u16) > end {
                return;
            }
            buf[(col, y)]
                .set_char(ch)
                .set_style(ansi::frame_style().patch(span.style));
            last_col = Some(col);
            col += w.max(1) as u16;
        }
    }
}

// ---------------------------------------------------------------------------
// Conversation viewport.
// ---------------------------------------------------------------------------

fn draw_viewport(app: &mut App, rect: Rect, buf: &mut Buffer) {
    // The scrollbar and viewport rect span the full `rect.height`, but
    // content is clipped to `content_viewport_height()` so a small blank
    // padding row is left at the bottom (when no footer menu is open).
    let vp_h = app.content_viewport_height().min(rect.height as usize);
    if app.blocks.is_empty() {
        // Empty-conversation atom animation fills the content rows; the
        // remaining bottom padding row(s) stay blank (base background).
        let art =
            atom_core::render::atom3d::render_atom3d(rect.width as i64, vp_h as i64, app.splash_t);
        let lines = ansi::ansi_to_lines(&art);
        for (i, line) in lines.iter().take(vp_h).enumerate() {
            write_line(buf, rect.x, rect.y + i as u16, rect.width as usize, line);
        }
        return;
    }
    app.refresh_viewport();
    let total = app.content_lines.len();
    let sel_range = if app.sel_active {
        Some(app.selection_range())
    } else {
        None
    };
    for row in 0..vp_h {
        let idx = app.scroll_y + row;
        if idx >= total {
            break;
        }
        let line = &app.content_lines[idx];
        let styled = match sel_range {
            Some(((a0, a1), (b0, b1))) if idx >= a0 && idx <= b0 => {
                let w = ansi::line_width(line);
                let c0 = if idx == a0 { a1 } else { 0 }.min(w);
                let c1 = (if idx == b0 { b1 + 1 } else { w }).min(w);
                if c1 > c0 {
                    Some(ansi::style_line_range(line, c0, c1, ansi::style_select()))
                } else {
                    None
                }
            }
            _ => None,
        };
        write_line(
            buf,
            rect.x,
            rect.y + row as u16,
            rect.width as usize,
            styled.as_ref().unwrap_or(line),
        );
    }
}

fn draw_scrollbar(app: &App, rect: Rect, buf: &mut Buffer) {
    let track = rect.height.min(buf.area.bottom().saturating_sub(rect.y)) as usize;
    let total = app.content_lines.len();
    if track == 0 || total <= track || rect.x >= buf.area.right() {
        return;
    }

    // The track spans the full viewport height, but the thumb represents
    // the visible *content* (which is padded short of the track when no
    // footer menu is open), so its proportion reflects content_viewport.
    let content_visible = if crate::overlays::footer_menu_height(app) > 0 {
        track
    } else {
        track.saturating_sub(crate::app::VIEWPORT_BOTTOM_PAD)
    };
    let thumb_h = (track.saturating_mul(content_visible))
        .div_ceil(total)
        .max(1)
        .min(track.saturating_sub(1).max(1));
    let max_scroll = total.saturating_sub(content_visible);
    let thumb_top = if max_scroll == 0 {
        0
    } else {
        app.scroll_y.min(max_scroll).saturating_mul(track - thumb_h) / max_scroll
    };

    for row in 0..track {
        let thumb = row >= thumb_top && row < thumb_top + thumb_h;
        let fg = if thumb {
            ansi::c_muted()
        } else {
            ansi::c_card_dark()
        };
        // Draw only the first column; remaining width is right padding.
        buf[(rect.x, rect.y + row as u16)]
            .set_symbol("█")
            .set_fg(fg);
    }
}

/// Overlays the active footer menu (slash/manage/picker/context) onto
/// the last viewport rows with the app background behind it.
fn draw_footer_menu(app: &mut App, rect: Rect, buf: &mut Buffer) {
    let menu: Vec<Line<'static>> = if app.menu_visible {
        render_slash_menu(app)
    } else if app.manage_visible {
        render_manage_menu(app)
    } else if !matches!(app.picker_kind, PickerKind::None) {
        render_picker_menu(app)
    } else if app.context_visible {
        render_context_menu(app)
    } else if app.reasoning_visible {
        render_reasoning_menu(app)
    } else if app.at_menu_visible {
        render_at_menu(app)
    } else {
        return;
    };
    if menu.is_empty() {
        return;
    }
    let n = menu.len();
    let (sel, title_rows) = overlays::footer_menu_sel(app);
    let region_h = rect.height as usize;
    let menu_h = region_h.saturating_sub(1).max(1);
    let (start, vis, pin_title, item_start) =
        overlays::footer_menu_window(n, menu_h, sel, title_rows);
    if vis < 1 {
        return;
    }
    // The first row in this dedicated region is always the divider.
    let top = region_h - vis;
    let overlay_top = top.saturating_sub(1);
    for row in overlay_top..region_h {
        let y = rect.y + row as u16;
        for x in rect.x..rect.right() {
            buf[(x, y)].reset();
            buf[(x, y)].set_style(ansi::frame_style());
        }
    }
    if top > 0 {
        let divider = Line::from(Span::styled(
            "─".repeat(rect.width as usize),
            ansi::style_prompt_border(),
        ));
        write_line(
            buf,
            rect.x,
            rect.y + top as u16 - 1,
            rect.width as usize,
            &divider,
        );
    }
    for i in 0..vis {
        let src_row = overlays::map_footer_menu_row(i, start, pin_title, item_start);
        let Some(line) = menu.get(src_row) else {
            continue;
        };
        // Pad to the full inner width with the background color so the
        // conversation underneath doesn't bleed through.
        let mut spans: Vec<Span> = line.spans.clone();
        let used = ansi::line_width(line);
        let pad = (rect.width as usize).saturating_sub(used);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        let y = rect.y + (top + i) as u16;
        write_line(buf, rect.x, y, rect.width as usize, &Line::from(spans));
    }
}

fn render_slash_menu(app: &mut App) -> Vec<Line<'static>> {
    let typed = app.menu_typed();
    let matches = overlays::match_commands(&typed, &app.slash_commands);
    if matches.is_empty() {
        return Vec::new();
    }
    matches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name_style = if i == app.menu_sel {
                ansi::style_selected()
            } else {
                ansi::style_inactive()
            };
            let desc = command_desc(app, c);
            Line::from(vec![
                Span::styled(c.name.clone(), name_style),
                Span::styled(format!("  {desc}"), ansi::style_dim()),
            ])
        })
        .collect()
}

fn command_desc(app: &App, c: &overlays::DynamicCommand) -> String {
    if c.name == "/thinking" {
        let state = if app.show_reasoning { "on" } else { "off" };
        return format!("{} ({})", c.desc, state);
    }
    if c.name == "/reasoning" {
        let lvl = app.thinking_level();
        if !lvl.is_empty() {
            return format!("{} ({})", c.desc, lvl);
        }
        if !app.thinking_pref.is_empty() {
            return format!("{} ({})", c.desc, app.thinking_pref);
        }
    }
    c.desc.clone()
}

fn render_manage_menu(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled("subagents", ansi::style_dim()))];
    if app.manage_agents.is_empty() {
        out.push(Line::from(Span::styled("no subagents", ansi::style_dim())));
        return out;
    }
    for (i, agent) in app.manage_agents.iter().enumerate() {
        let mut title = agent.title.clone();
        if title.is_empty() {
            title = agent.model.clone();
            if title.is_empty() {
                title = "subagent".into();
            }
            if !agent.id.is_empty() {
                let short: String = agent.id.chars().take(8).collect();
                title = format!("{title} {short}");
            }
        }
        let mut meta = agent.model.clone();
        if !agent.thinking.is_empty() {
            meta.push_str(&format!(" ({})", agent.thinking));
        }
        let (status, spinning, status_style) = match agent.status {
            atom_core::session::store::DelegateStatus::Queued
            | atom_core::session::store::DelegateStatus::Working => (
                "Working",
                true,
                ratatui::style::Style::new().fg(ansi::c_foreground()),
            ),
            atom_core::session::store::DelegateStatus::Sandbox => (
                "Sandbox",
                false,
                ratatui::style::Style::new().fg(ansi::c_foreground()),
            ),
            atom_core::session::store::DelegateStatus::Error => {
                ("Error", false, ansi::style_error())
            }
            atom_core::session::store::DelegateStatus::Done => {
                ("Done", false, ansi::style_primary())
            }
            atom_core::session::store::DelegateStatus::Cancelled => {
                ("Cancelled", false, ansi::style_error())
            }
        };
        let mut status_text = String::new();
        if spinning {
            status_text.push_str(
                crate::app::MINIDOT_FRAMES[app.spinner_frame % crate::app::MINIDOT_FRAMES.len()],
            );
            status_text.push(' ');
        }
        status_text.push_str(status);
        let meta = if meta.is_empty() {
            "  ".to_string()
        } else {
            format!("  {meta} | ")
        };
        let name_style = if i == app.manage_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        let spans = vec![
            Span::styled(title, name_style),
            Span::styled(meta, ansi::style_dim()),
            Span::styled(status_text, status_style),
        ];
        out.push(Line::from(spans));
    }
    out
}

fn render_picker_menu(app: &App) -> Vec<Line<'static>> {
    let title = if app.picker_kind == PickerKind::Skills {
        "skills"
    } else {
        "mcp"
    };
    let empty = if app.picker_kind == PickerKind::Skills {
        "no skills"
    } else {
        "no mcp servers"
    };
    let mut out = vec![Line::from(Span::styled(title, ansi::style_dim()))];
    if app.picker_items.is_empty() {
        out.push(Line::from(Span::styled(empty, ansi::style_dim())));
        return out;
    }
    for (i, item) in app.picker_items.iter().enumerate() {
        let mut title_txt = item.title.clone();
        if title_txt.is_empty() {
            title_txt = title.to_string();
        }
        let name_style = if i == app.picker_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        let mut spans = vec![Span::styled(title_txt, name_style)];
        if !item.meta.is_empty() {
            spans.push(Span::styled(format!("  {}", item.meta), ansi::style_dim()));
        }
        out.push(Line::from(spans));
    }
    out
}

fn render_at_menu(app: &App) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, item) in app.at_menu_items.iter().enumerate() {
        let name_style = if i == app.at_menu_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        out.push(Line::from(Span::styled(item.clone(), name_style)));
    }
    out
}

fn render_context_menu(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled("context", ansi::style_dim()))];
    if app.context_rows.is_empty() {
        out.push(Line::from(Span::styled("no context", ansi::style_dim())));
        return out;
    }
    for (i, row) in app.context_rows.iter().enumerate() {
        let meta = atom_core::session::context_breakdown::context_row_meta(row);
        let name_style = if i == app.context_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        out.push(Line::from(vec![
            Span::styled(row.name.clone(), name_style),
            Span::styled(format!("  {meta}"), ansi::style_dim()),
        ]));
    }
    out
}

// ---------------------------------------------------------------------------
// Footer hint helpers.
// ---------------------------------------------------------------------------

/// Footer-menu and parent/child navigation hints. These now render inside
/// the status bar (see [`crate::statusbar::nav_segments`]); this helper
/// remains for direct unit tests of the hint text.
pub fn working_status_line(app: &App) -> Line<'static> {
    let segs = crate::statusbar::nav_segments(app);
    if segs.is_empty() {
        return Line::from(" ");
    }
    let sep = vec![Span::styled(" / ", ansi::style_prompt_border())];
    let mut spans: Vec<Span> = Vec::new();
    for (i, s) in segs.into_iter().enumerate() {
        if i > 0 {
            spans.extend(sep.clone());
        }
        spans.extend(s);
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Image chips and previews.
// ---------------------------------------------------------------------------

/// Same as `line_with_chips` but preserves the per-span styles from the
/// prompt row (e.g. the REVERSED selection style). Chip markers are
/// replaced with same-width `[IMG n]` spans carrying the chip style, and
/// the surrounding text keeps the original span's style.
/// Additionally, `@path/to/file` references are rendered as orange inline
/// tags, and pasted absolute file paths are rendered with a "File name" tag.
fn line_with_chips_styled(line: &Line<'_>, app: &App) -> Line<'static> {
    let max_num = app.pending.iter().map(|p| p.num).max().unwrap_or(0);
    let selected = if app.input.has_selection() {
        app.input.selected_text()
    } else {
        String::new()
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in &line.spans {
        let style = span.style;
        let mut rest = span.content.to_string();

        // Process image markers
        if max_num > 0 {
            loop {
                let mut found: Option<(usize, usize)> = None;
                for i in 1..=max_num {
                    let mark = preview::image_marker(i);
                    if selected.contains(&mark) {
                        continue;
                    }
                    if let Some(pos) = rest.find(&mark) {
                        if found.map(|(p, _)| pos < p).unwrap_or(true) {
                            found = Some((pos, i));
                        }
                    }
                }
                match found {
                    Some((pos, num)) => {
                        let before = rest[..pos].to_string();
                        rest = rest[pos + preview::image_marker(num).len()..].to_string();
                        emit_file_tagged_spans(&before, style, &mut out);
                        out.push(Span::styled(
                            preview::image_chip(num),
                            ansi::style_img_chip(),
                        ));
                    }
                    None => break,
                }
            }
        }

        if !rest.is_empty() {
            emit_file_tagged_spans(&rest, style, &mut out);
        }
    }
    Line::from(out)
}

/// Processes text looking for @file references and absolute file paths,
/// rendering them as orange inline tags. Non-matching text is emitted
/// with the given style.
fn emit_file_tagged_spans(text: &str, base_style: Style, out: &mut Vec<Span<'static>>) {
    if text.is_empty() {
        return;
    }

    // First check if the entire text is a single absolute file path
    let trimmed = text.trim();
    if trimmed.starts_with('/') && overlays::looks_like_file_path(trimmed) && !trimmed.contains(' ')
    {
        let short_name = file_chip_label(trimmed);
        out.push(Span::styled(
            format!(" File {} ", short_name),
            ansi::style_file_chip(),
        ));
        return;
    }

    let mut rest = text;
    while let Some(at_pos) = rest.find('@') {
        // Emit text before the @
        if at_pos > 0 {
            out.push(Span::styled(rest[..at_pos].to_string(), base_style));
        }
        let after_at = &rest[at_pos + 1..];
        // Extract the @word (no spaces)
        let end = after_at
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after_at.len());
        let token = &after_at[..end];
        if !token.is_empty() && token.contains('/') {
            // File reference with path — render full @path in orange
            out.push(Span::styled(format!("@{}", token), ansi::style_file_chip()));
        } else if !token.is_empty() {
            // Just a plain @ followed by a word with no slash — render normally
            out.push(Span::styled(format!("@{}", token), ansi::style_file_chip()));
        } else {
            // Bare @ at end or followed by space
            out.push(Span::styled("@".to_string(), base_style));
        }
        rest = &after_at[end..];
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), base_style));
    }
}

/// Returns the short display label for a file chip: the last path component.
fn file_chip_label(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// renderPreviews: Unicode placeholder cells for kitty virtual
/// placements, top-aligned under the typed prompt.
fn render_previews(app: &App) -> Vec<Line<'static>> {
    let n = preview::preview_thumb_rows(app);
    if n == 0 {
        return Vec::new();
    }
    struct Item {
        cols: usize,
        rows: usize,
        lines: Vec<String>,
    }
    let items: Vec<Item> = app
        .pending
        .iter()
        .filter(|p| p.cols > 0)
        .map(|p| Item {
            cols: p.cols,
            rows: p.rows,
            lines: preview::placeholder_grid(p.num, p.cols, p.rows)
                .split('\n')
                .map(str::to_string)
                .collect(),
        })
        .collect();
    if items.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // Blank row separating thumbnails from the prompt text.
    for _ in 0..preview::PREVIEW_PROMPT_GAP {
        out.push(Line::from(" "));
    }
    for y in 0..n {
        let mut sb = String::new();
        for (j, it) in items.iter().enumerate() {
            if j > 0 {
                sb.push_str(&" ".repeat(preview::PREVIEW_GAP));
            }
            if y < it.rows && y < it.lines.len() {
                sb.push_str(&it.lines[y]);
            } else {
                sb.push_str(&" ".repeat(it.cols));
            }
        }
        if sb.is_empty() {
            out.push(Line::from(" "));
        } else {
            out.push(ansi::ansi_to_line(&sb));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sandbox approval box.
// ---------------------------------------------------------------------------

/// Legacy floating approval box — kept for test coverage of field layout.
#[allow(dead_code)]
fn draw_approval_box(app: &App, req: ApprovalPrompt, area: Rect, buf: &mut Buffer, geo: &Layout) {
    let inner_w = app.inner_width().min(area.width.saturating_sub(4) as usize);
    let body = approval_body(&req, inner_w);
    let box_h = (body.len() + 2).min(area.height as usize) as u16;
    // Float just above the prompt's top border.
    let top = if geo.prompt_top_y >= box_h as usize {
        area.y + (geo.prompt_top_y - box_h as usize) as u16
    } else {
        area.y
    };
    let rect = Rect::new(area.x + 1, top, (inner_w as u16 + 2).min(area.width), box_h);
    ratatui::widgets::Widget::render(Clear, rect, buf);
    buf.set_style(rect, ansi::frame_style());
    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_style(ansi::style_prompt_border());
    ratatui::widgets::Widget::render(block, rect, buf);
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        box_h.saturating_sub(2),
    );
    for (i, line) in body.iter().enumerate() {
        if i >= inner.height as usize {
            break;
        }
        write_line(buf, inner.x, inner.y + i as u16, inner.width as usize, line);
    }
}

#[allow(dead_code)]
fn approval_body(req: &ApprovalPrompt, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut body = vec![Line::from(Span::styled(
        "sandbox approval",
        ansi::style_cursor(),
    ))];
    if req.from_subagent {
        // The parent view surfaces a subagent's request: state who is
        // asking before the command details.
        let who = if req.child_title.is_empty() {
            "a subagent".to_string()
        } else {
            format!("subagent \"{}\"", req.child_title)
        };
        for row in wrap_approval_text(&format!("{who} needs sandbox permission"), width) {
            body.push(Line::from(Span::styled(row, ansi::style_reasoning())));
        }
    }
    approval_field(&mut body, "command ", &req.command, width);
    approval_field(&mut body, "cwd     ", &req.cwd, width);
    approval_field(
        &mut body,
        "rule    ",
        if req.rule_id.is_empty() {
            "-"
        } else {
            &req.rule_id
        },
        width,
    );
    approval_field(&mut body, "reason  ", &req.reason, width);
    for row in wrap_approval_text("a allow · s this session · g always · d/Esc deny", width) {
        body.push(Line::from(Span::styled(row, ansi::style_reasoning())));
    }
    body
}

#[allow(dead_code)]
fn approval_field(body: &mut Vec<Line<'static>>, label: &'static str, value: &str, width: usize) {
    let label_width = label.len();
    if width <= label_width {
        for row in wrap_approval_text(&format!("{label}{value}"), width) {
            body.push(Line::from(row));
        }
        return;
    }

    let rows = wrap_approval_text(value, width - label_width);
    for (index, row) in rows.into_iter().enumerate() {
        let prefix = if index == 0 {
            label.to_string()
        } else {
            " ".repeat(label_width)
        };
        body.push(Line::from(vec![
            Span::styled(prefix, ansi::style_dim()),
            Span::raw(row),
        ]));
    }
}

#[allow(dead_code)]
fn wrap_approval_text(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrap_plain(line, width.max(1)))
        .collect()
}

// ---------------------------------------------------------------------------
// Full-screen overlays.
// ---------------------------------------------------------------------------

fn draw_overlay(app: &mut App, kind: OverlayKind, area: Rect, buf: &mut Buffer) {
    let lines = render_overlay(app, kind);
    for (i, line) in lines.iter().enumerate() {
        if i >= area.height as usize {
            break;
        }
        write_line(buf, area.x, area.y + i as u16, area.width as usize, line);
    }
}

fn overlay_query_lines(app: &App, placeholder: bool, width: usize) -> Vec<Line<'static>> {
    let value = if app.overlay_q.is_empty() && placeholder {
        "search"
    } else {
        &app.overlay_q
    };
    let text = format!("> {value}");
    let style = if app.overlay_q.is_empty() && placeholder {
        ansi::style_dim()
    } else if app.overlay_q_sel {
        ansi::style_query_sel()
    } else {
        ratatui::style::Style::default()
    };
    wrap_plain(&text, width.max(1))
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

fn header_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), ansi::style_dim()))
}

fn wrapped_header(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap_plain(text, width.max(1))
        .into_iter()
        .map(|row| header_line(&row))
        .collect()
}

fn blank() -> Line<'static> {
    Line::from("")
}

fn render_overlay(app: &mut App, kind: OverlayKind) -> Vec<Line<'static>> {
    if !app.working_msg.is_empty() {
        let frame =
            crate::app::MINIDOT_FRAMES[app.spinner_frame % crate::app::MINIDOT_FRAMES.len()];
        return vec![header_line(&format!("{frame} {}", app.working_msg))];
    }
    match kind {
        OverlayKind::Model => render_model_selector(app),
        OverlayKind::Session => render_session_selector(app),
        OverlayKind::Stats => render_stats_overlay(app),
        OverlayKind::Providers => render_providers_overlay(app),
        OverlayKind::ProviderMethod => render_provider_method_overlay(app),
        OverlayKind::ProviderKey => render_provider_key_overlay(app),
        OverlayKind::Settings => render_settings_overlay(app),
        OverlayKind::WebSearch => render_web_search_overlay(app),
    }
}

fn render_model_selector(app: &App) -> Vec<Line<'static>> {
    let filtered = overlays::filter_entries(&app.overlay_entries, &app.overlay_q);
    let rows = overlays::model_rows(app);
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(&overlays::overlay_title(app, OverlayKind::Model), width);
    out.push(blank());
    out.extend(overlay_query_lines(app, false, width));
    out.push(blank());
    if filtered.is_empty() {
        out.push(header_line("no matches"));
        return out;
    }
    let counts: Vec<usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| match &row.entry {
            Some(entry) => {
                let label = format!("{}  {}", entry.provider.name, entry.model);
                let display = if i == app.overlay_sel {
                    format!("▸ {label}")
                } else {
                    label
                };
                wrap_plain(&display, width).len().max(1)
            }
            None => wrap_plain(&row.label, width).len().max(1),
        })
        .collect();
    let mut rendered = vec![false; rows.len()];
    overlays::overlay_for_each_visible(
        app.overlay_scroll,
        app.overlay_sel,
        overlays::overlay_list_max_lines(app),
        rows.len(),
        |i| counts.get(i).copied().unwrap_or(1),
        |i| rendered[i] = true,
    );
    for (i, row) in rows.iter().enumerate() {
        if !rendered[i] {
            continue;
        }
        let Some(entry) = &row.entry else {
            out.extend(wrapped_header(&row.label, width));
            continue;
        };
        let label = format!("{}  {}", entry.provider.name, entry.model);
        let (label, style) = if i == app.overlay_sel {
            (format!("▸ {label}"), ansi::style_selected())
        } else {
            (label, ansi::style_inactive())
        };
        for row in wrap_plain(&label, width) {
            out.push(Line::from(Span::styled(row, style)));
        }
    }
    let selected = rows
        .iter()
        .take(app.overlay_sel.saturating_add(1))
        .filter(|row| row.entry.is_some())
        .count();
    out.push(blank());
    out.push(header_line(&format!(
        "{}/{} models",
        selected,
        filtered.len()
    )));
    out
}

fn render_session_selector(app: &mut App) -> Vec<Line<'static>> {
    let rows = overlays::session_rows(app);
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(&overlays::overlay_title(app, OverlayKind::Session), width);
    out.push(blank());
    out.extend(overlay_query_lines(app, true, width));
    out.push(blank());
    if rows.is_empty() {
        out.push(header_line("no matches"));
        return out;
    }
    let counts: Vec<usize> = {
        // Recompute per-row visual heights like the scroll math does.
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
                overlays::wrapped_label_line_count(&r.label, width, i == app.overlay_sel, marker)
            })
            .collect()
    };
    let max_items = overlays::overlay_list_max_lines(app);
    let mut rendered = vec![false; rows.len()];
    overlays::overlay_for_each_visible(
        app.overlay_scroll,
        app.overlay_sel,
        max_items,
        rows.len(),
        |i| counts.get(i).copied().unwrap_or(1),
        |i| {
            rendered[i] = true;
        },
    );
    for (i, r) in rows.iter().enumerate() {
        if !rendered[i] {
            continue;
        }
        if r.date {
            out.push(header_line(&r.label));
            continue;
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
        if i == app.overlay_sel {
            let label = format!("▸ {marker}{}", r.label);
            for row in wrap_plain(&label, width) {
                out.push(Line::from(Span::styled(row, ansi::style_selected())));
            }
        } else {
            for row in wrap_plain(&format!("{marker}{}", r.label), width) {
                out.push(Line::from(Span::styled(row, ansi::style_inactive())));
            }
        }
    }
    // Footer counts selectable sessions.
    let mut sel_count = 0usize;
    let mut total = 0usize;
    for (i, r) in rows.iter().enumerate() {
        if r.date {
            continue;
        }
        total += 1;
        if i <= app.overlay_sel {
            sel_count += 1;
        }
    }
    out.push(blank());
    out.push(header_line(&format!("{sel_count}/{total} sessions")));
    out
}

fn render_stats_overlay(app: &App) -> Vec<Line<'static>> {
    let width = app.width.max(1) as i32;
    let mut out = wrapped_header(
        &overlays::overlay_title(app, OverlayKind::Stats),
        width as usize,
    );
    out.push(blank());
    let Some(report) = &app.overlay_stats else {
        out.push(header_line("loading stats..."));
        return out;
    };
    let lines = atom_core::session::stats::render_stats(report, width - 4, true);
    let visible = ((app.height as i32) - 6).clamp(3, i32::MAX) as usize;
    let scroll = app.overlay_sel.min(lines.len().saturating_sub(visible));
    let end = (scroll + visible).min(lines.len());
    for line in &lines[scroll..end] {
        out.push(ansi::ansi_to_line(line));
    }
    if lines.len() > visible {
        out.push(blank());
        out.push(header_line(&format!(
            "{}–{} of {} lines",
            scroll + 1,
            end,
            lines.len()
        )));
    }
    out
}

fn render_reasoning_menu(app: &App) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled("reasoning", ansi::style_dim()))];
    if app.thinking_levels.is_empty() {
        out.push(Line::from(Span::styled(
            "no levels for this model",
            ansi::style_dim(),
        )));
        return out;
    }
    let current = {
        let lvl = app.thinking_level();
        if lvl.is_empty() {
            app.thinking_pref.clone()
        } else {
            lvl
        }
    };
    for (i, level) in app.thinking_levels.iter().enumerate() {
        let mut label = level.clone();
        if *level == current {
            label = format!("{label}  (current)");
        }
        let name_style = if i == app.reasoning_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        out.push(Line::from(Span::styled(label, name_style)));
    }
    out
}

fn render_providers_overlay(app: &App) -> Vec<Line<'static>> {
    let filtered = atom_core::providers::providers::filter_provider_entries(
        &app.overlay_providers,
        &app.overlay_q,
    );
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(&overlays::overlay_title(app, OverlayKind::Providers), width);
    out.push(blank());
    out.extend(overlay_query_lines(app, false, width));
    out.push(blank());
    if filtered.is_empty() {
        out.push(header_line("no matches"));
        return out;
    }
    let max_items = overlays::overlay_list_max_lines(app);
    let scroll = overlays::overlay_list_scroll(app.overlay_sel, max_items, filtered.len());
    let end = (scroll + max_items).min(filtered.len());
    for i in scroll..end {
        let e = &filtered[i];
        let label = if e.status.is_empty() {
            e.label.clone()
        } else {
            format!("{}  {}", e.label, e.status)
        };
        let style = if i == app.overlay_sel {
            ansi::style_selected()
        } else {
            ansi::style_inactive()
        };
        let marker = if i == app.overlay_sel { "▸ " } else { "  " };
        for row in wrap_plain(&format!("{marker}{label}"), width) {
            out.push(Line::from(Span::styled(row, style)));
        }
    }
    out.push(blank());
    out.push(header_line(&format!(
        "{}/{} providers",
        app.overlay_sel + 1,
        filtered.len()
    )));
    out
}

fn render_provider_method_overlay(app: &App) -> Vec<Line<'static>> {
    let mut out = wrapped_header(
        &overlays::overlay_title(app, OverlayKind::ProviderMethod),
        app.width.max(1) as usize,
    );
    out.push(blank());
    for (i, label) in ["API Key", "OAuth"].iter().enumerate() {
        if i == app.overlay_sel {
            out.push(Line::from(Span::styled(
                format!("▸ {label}"),
                ansi::style_selected(),
            )));
        } else {
            out.push(Line::from(Span::styled(
                format!("  {label}"),
                ansi::style_inactive(),
            )));
        }
    }
    out
}

fn render_provider_key_overlay(app: &App) -> Vec<Line<'static>> {
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(
        &overlays::overlay_title(app, OverlayKind::ProviderKey),
        width,
    );
    out.push(blank());
    out.extend(overlay_query_lines(app, false, width));
    out
}

fn render_settings_overlay(app: &App) -> Vec<Line<'static>> {
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(&overlays::overlay_title(app, OverlayKind::Settings), width);
    out.push(blank());
    let rows = overlays::settings_labels(app);
    for (index, label) in rows.into_iter().enumerate() {
        let (prefix, style) = if index == app.overlay_sel {
            ("▸ ", ansi::style_selected())
        } else {
            ("  ", ansi::style_inactive())
        };
        for row in wrap_plain(&format!("{prefix}{label}"), width) {
            out.push(Line::from(Span::styled(row, style)));
        }
    }
    out
}

fn render_web_search_overlay(app: &App) -> Vec<Line<'static>> {
    let width = app.width.max(1) as usize;
    let mut out = wrapped_header(&overlays::overlay_title(app, OverlayKind::WebSearch), width);
    out.push(blank());
    for (index, (_, name, meta)) in overlays::web_search_rows(app).into_iter().enumerate() {
        let label = format!("{name}  {meta}");
        let (prefix, style) = if index == app.overlay_sel {
            ("▸ ", ansi::style_selected())
        } else {
            ("  ", ansi::style_inactive())
        };
        for row in wrap_plain(&format!("{prefix}{label}"), width) {
            out.push(Line::from(Span::styled(row, style)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{Block, BlockKind};
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    fn frame(app: &mut App, w: u16, h: u16) -> ratatui::Terminal<TestBackend> {
        let backend = TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            draw(app, f.area(), f.buffer_mut());
        })
        .unwrap();
        term
    }

    fn text(term: &ratatui::Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("")
            .trim_end()
            .to_string()
    }

    #[test]
    fn loading_overlay_visibly_animates() {
        let mut app = App::new_test(80, 24);
        app.working_msg = "loading models...".into();
        let first = render_overlay(&mut app, OverlayKind::Model)[0].spans[0]
            .content
            .to_string();
        app.spinner_frame += 1;
        let second = render_overlay(&mut app, OverlayKind::Model)[0].spans[0]
            .content
            .to_string();

        assert_ne!(first, second);
        assert!(first.contains("loading models..."));
        assert!(second.contains("loading models..."));
    }

    #[test]
    fn frame_draws_prompt_chrome_and_status() {
        let mut app = App::new_test(80, 24);
        app.sel_model = "test-model".into();
        app.thinking_levels = vec!["none".into()];
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let term = frame(&mut app, 80, 24);
        let s = text(&term);
        assert!(s.contains("test-model (none)"), "status bar head");
        assert!(s.contains("hello"), "conversation content");
        assert!(!s.contains("you:"), "user label removed");
        assert_eq!(cell(&term, 1, 21).bg, ansi::c_card_light());
    }

    #[test]
    fn splash_replaces_empty_conversation() {
        let mut app = App::new_test(80, 24);
        let term = frame(&mut app, 80, 24);
        // The atom3d art fills the viewport; the borderless prompt remains.
        assert_eq!(cell(&term, 1, 21).bg, ansi::c_card_light());
    }

    #[test]
    fn approval_box_lists_fields_and_keys() {
        let mut app = App::new_test(90, 30);
        app.approval = Some(crate::app::ApprovalPrompt {
            id: "abc".into(),
            command: "curl https://x.co".into(),
            cwd: "/tmp".into(),
            rule_id: "net".into(),
            reason: "ask network".into(),
            session_id: "sess1".into(),
            child_title: String::new(),
            from_subagent: false,
        });
        // Inline approval block
        app.blocks.push(crate::blocks::Block {
            kind: crate::blocks::BlockKind::Tool,
            title: "Sandbox".to_string(),
            tool_name: "sandbox".to_string(),
            text: "curl https://x.co".to_string(),
            approval: Some(crate::blocks::InlineApproval {
                id: "abc".into(),
                session_id: "sess1".into(),
                command: "curl https://x.co".into(),
                cwd: "/tmp".into(),
                rule_id: "net".into(),
                reason: "ask network".into(),
                from_subagent: false,
                child_title: String::new(),
            }),
            expanded: true,
            ..Default::default()
        });
        app.viewport_dirty = true;
        app.refresh_viewport();
        let term = frame(&mut app, 90, 30);
        let s = text(&term);
        assert!(s.contains("Sandbox"), "title visible: {s}");
        assert!(s.contains("curl https://x.co"), "command visible: {s}");
        assert!(s.contains("[a] allow"), "buttons visible: {s}");
    }

    #[test]
    fn approval_box_names_subagent_when_request_comes_from_child() {
        let mut app = App::new_test(90, 30);
        app.approval = Some(crate::app::ApprovalPrompt {
            id: "abc".into(),
            command: "git push".into(),
            cwd: "/repo".into(),
            rule_id: "git-push".into(),
            reason: "push to remote".into(),
            session_id: "child123".into(),
            child_title: "push the release".into(),
            from_subagent: true,
        });
        app.blocks.push(crate::blocks::Block {
            kind: crate::blocks::BlockKind::Tool,
            title: "Sandbox".to_string(),
            tool_name: "sandbox".to_string(),
            text: "git push".to_string(),
            approval: Some(crate::blocks::InlineApproval {
                id: "abc".into(),
                session_id: "child123".into(),
                command: "git push".into(),
                cwd: "/repo".into(),
                rule_id: "git-push".into(),
                reason: "push to remote".into(),
                from_subagent: true,
                child_title: "push the release".into(),
            }),
            expanded: true,
            ..Default::default()
        });
        app.viewport_dirty = true;
        app.refresh_viewport();
        let term = frame(&mut app, 90, 30);
        let s = text(&term);
        assert!(
            s.contains("push the release") && s.contains("needs permission"),
            "subagent hint visible: {s}"
        );
        assert!(s.contains("git push"), "command visible: {s}");
        assert!(s.contains("[a] allow"), "buttons visible: {s}");
    }

    #[test]
    fn approval_box_wraps_long_commands_within_its_width() {
        let mut app = App::new_test(40, 24);
        app.approval = Some(crate::app::ApprovalPrompt {
            id: "abc".into(),
            command: "cargo test --workspace --all-features WRAP_SENTINEL".into(),
            cwd: "/tmp".into(),
            rule_id: "exec".into(),
            reason: "run workspace tests".into(),
            session_id: "sess1".into(),
            child_title: String::new(),
            from_subagent: false,
        });
        app.blocks.push(crate::blocks::Block {
            kind: crate::blocks::BlockKind::Tool,
            title: "Sandbox".to_string(),
            tool_name: "sandbox".to_string(),
            text: "cargo test --workspace --all-features WRAP_SENTINEL".to_string(),
            approval: Some(crate::blocks::InlineApproval {
                id: "abc".into(),
                session_id: "sess1".into(),
                command: "cargo test --workspace --all-features WRAP_SENTINEL".into(),
                cwd: "/tmp".into(),
                rule_id: "exec".into(),
                reason: "run workspace tests".into(),
                from_subagent: false,
                child_title: String::new(),
            }),
            expanded: true,
            ..Default::default()
        });
        app.viewport_dirty = true;
        app.refresh_viewport();
        let term = frame(&mut app, 40, 24);
        let s = text(&term);
        assert!(s.contains("Sandbox"), "title visible: {s}");
        assert!(s.contains("[a] allow"), "buttons visible: {s}");
    }

    #[test]
    fn footer_menu_overlays_viewport_bottom() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = App::new_test(80, 24);
        app.input.set_value("/");
        app.after_input_change();
        assert!(app.menu_visible);
        let term = frame(&mut app, 80, 24);
        let s = text(&term);
        assert!(s.contains("/model"), "slash menu rows visible");
        let _ = KeyCode::Esc;
        let _ = KeyModifiers::NONE;
    }

    #[test]
    fn ctrl_p_menu_renders_with_filled_prompt() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new_test(80, 24);
        app.input.set_value("hello world");
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.menu_visible);
        let term = frame(&mut app, 80, 24);
        let s = text(&term);
        assert!(
            s.contains("/model"),
            "slash menu rows visible with filled prompt"
        );
        assert!(s.contains("/settings"));
        assert!(s.contains("hello world"), "prompt text still shown");
    }

    #[test]
    fn overlay_headers_and_queries_wrap_to_terminal_width() {
        let mut app = App::new_test(32, 24);
        app.overlay = Some(OverlayKind::Model);
        app.overlay_q = "provider-with-a-long-name".into();
        app.overlay_entries = vec![atom_core::providers::providers::ModelEntry {
            provider: atom_core::providers::providers::Provider {
                name: app.overlay_q.clone(),
                ..Default::default()
            },
            model: "model-with-a-long-name".into(),
        }];
        app.overlay_sel = overlays::first_model_row(&app);

        let lines = render_overlay(&mut app, OverlayKind::Model);
        assert!(
            lines.iter().all(|line| ansi::line_width(line) <= 32),
            "overlay contains an over-wide line: {lines:?}"
        );
        let plain = lines
            .iter()
            .map(ansi::line_plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("Esc to\ncancel"),
            "header tail was clipped: {plain}"
        );
        assert!(plain.contains("Pinned - None"));
        assert!(plain.contains("Recent - None"));
    }

    #[test]
    fn footer_menu_padding_uses_terminal_cell_width() {
        let mut app = App::new_test(8, 4);
        app.slash_commands = vec![overlays::DynamicCommand {
            name: "/e\u{301}".into(),
            desc: "d".into(),
            kind: "skill".into(),
            dynamic: true,
        }];
        app.input.set_value("/e");
        app.menu_visible = true;
        let rect = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(rect);
        for x in 0..rect.width {
            buf[(x, 0)].set_char('x');
        }

        draw_footer_menu(&mut app, rect, &mut buf);

        assert_eq!(buf[(rect.width - 1, 0)].symbol(), " ");
    }

    #[test]
    fn slash_menu_has_a_subtle_top_border() {
        let mut app = App::new_test(8, 4);
        app.slash_commands = vec![overlays::DynamicCommand {
            name: "/unique".into(),
            desc: "choose".into(),
            kind: "builtin".into(),
            dynamic: false,
        }];
        app.input.set_value("/unique");
        app.menu_visible = true;
        let rect = Rect::new(0, 0, 8, 3);
        let mut buf = Buffer::empty(rect);
        for y in 0..rect.height {
            for x in 0..rect.width {
                buf[(x, y)].set_char('x').set_style(
                    ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::UNDERLINED),
                );
            }
        }

        draw_footer_menu(&mut app, rect, &mut buf);

        for x in 0..rect.width {
            assert_eq!(buf[(x, 1)].symbol(), "─");
            assert_eq!(buf[(x, 1)].fg, ansi::style_prompt_border().fg.unwrap());
            assert!(!buf[(x, 1)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED));
            assert!(!buf[(x, 2)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED));
        }
    }

    #[test]
    fn sub_menus_have_a_subtle_top_border() {
        use atom_core::session::context_breakdown::ContextRow;

        let mut app = App::new_test(8, 4);
        app.context_rows = vec![ContextRow {
            name: "repo".into(),
            tokens: 100,
            pct: 10,
        }];
        app.context_visible = true;
        let rect = Rect::new(0, 0, 8, 3);
        let mut buf = Buffer::empty(rect);
        for y in 0..rect.height {
            for x in 0..rect.width {
                buf[(x, y)].set_char('x').set_style(
                    ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::UNDERLINED),
                );
            }
        }

        draw_footer_menu(&mut app, rect, &mut buf);

        // The pinned "context" title row and the item row follow the divider.
        for x in 0..rect.width {
            assert_eq!(buf[(x, 0)].symbol(), "─");
            assert_eq!(buf[(x, 0)].fg, ansi::style_prompt_border().fg.unwrap());
            assert!(!buf[(x, 0)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED));
        }
        let title_row: String = (0..rect.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(title_row, "context ");
        assert!(!buf[(1, 1)]
            .modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
        assert!(!buf[(1, 2)]
            .modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    // -- cell/row helpers ---------------------------------------------------

    fn cell(term: &ratatui::Terminal<TestBackend>, x: u16, y: u16) -> &ratatui::buffer::Cell {
        &term.backend().buffer()[(x, y)]
    }

    fn row_text(term: &ratatui::Terminal<TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn first_char_x(term: &ratatui::Terminal<TestBackend>, y: u16) -> Option<u16> {
        let buf = term.backend().buffer();
        (0..buf.area.width).find(|&x| buf[(x, y)].symbol() != " ")
    }

    fn draw_into(app: &mut App, term: &mut ratatui::Terminal<TestBackend>) {
        term.draw(|f| {
            draw(app, f.area(), f.buffer_mut());
        })
        .unwrap();
    }

    #[test]
    fn scrollbar_tracks_content_overflow_and_scroll_position() {
        let mut app = App::new_test(10, 10);
        app.content_lines = (0..20)
            .map(|i| std::sync::Arc::new(Line::from(i.to_string())))
            .collect();
        let rect = Rect::new(8, 0, 2, 6);

        let mut top = Buffer::empty(Rect::new(0, 0, 10, 6));
        draw_scrollbar(&app, rect, &mut top);
        // First scrollbar column should be painted; second is padding.
        assert_eq!(top[(8, 0)].symbol(), "█");
        assert_eq!(top[(8, 0)].fg, ansi::c_muted());
        assert_eq!(top[(9, 0)].symbol(), " ");
        assert_eq!(top[(8, 1)].symbol(), "█");
        assert_eq!(top[(8, 5)].symbol(), "█");
        assert_eq!(top[(8, 5)].fg, ansi::c_card_dark());

        app.scroll_y = 15;
        let mut bottom = Buffer::empty(Rect::new(0, 0, 10, 6));
        draw_scrollbar(&app, rect, &mut bottom);
        assert_eq!(bottom[(8, 0)].symbol(), "█");
        assert_eq!(bottom[(8, 0)].fg, ansi::c_card_dark());
        assert_eq!(bottom[(8, 4)].symbol(), "█");
        assert_eq!(bottom[(8, 5)].symbol(), "█");
        assert_eq!(bottom[(8, 5)].fg, ansi::c_muted());

        app.content_lines.truncate(6);
        let mut hidden = Buffer::empty(Rect::new(0, 0, 10, 6));
        draw_scrollbar(&app, rect, &mut hidden);
        assert_eq!(hidden[(8, 0)].symbol(), " ");
        assert_eq!(hidden[(9, 0)].symbol(), " ");
    }

    #[test]
    fn streaming_does_not_add_a_working_indicator() {
        let mut app = App::new_test(80, 24);
        app.streaming = true;

        assert!(ansi::line_plain(&working_status_line(&app))
            .trim()
            .is_empty());
    }

    #[test]
    fn subagent_menu_renders_model_thinking_and_status() {
        let mut app = App::new_test(80, 24);
        let mut working = crate::app::empty_session_info();
        working.title = "worker".into();
        working.model = "model-id".into();
        working.thinking = "high".into();
        working.status = atom_core::session::store::DelegateStatus::Working;
        let mut sandbox = working.clone();
        sandbox.title = "approval".into();
        sandbox.status = atom_core::session::store::DelegateStatus::Sandbox;
        let mut error = working.clone();
        error.title = "failed".into();
        error.status = atom_core::session::store::DelegateStatus::Error;
        let mut done = working.clone();
        done.title = "finished".into();
        done.status = atom_core::session::store::DelegateStatus::Done;
        app.manage_agents = vec![working, sandbox, error, done];

        let lines = render_manage_menu(&app);
        assert!(ansi::line_plain(&lines[1]).contains("model-id (high) | ⠋ Working"));
        assert!(ansi::line_plain(&lines[2]).contains("model-id (high) | Sandbox"));
        assert_eq!(
            lines[1].spans.last().unwrap().style.fg,
            Some(ansi::c_foreground())
        );
        assert_eq!(
            lines[3].spans.last().unwrap().style.fg,
            Some(ansi::c_secondary())
        );
        assert_eq!(
            lines[4].spans.last().unwrap().style.fg,
            Some(ansi::c_primary())
        );

        app.spinner_frame = 1;
        let lines = render_manage_menu(&app);
        assert!(ansi::line_plain(&lines[1]).contains("| ⠙ Working"));
        assert!(ansi::line_plain(&lines[2]).contains("| Sandbox"));
    }

    // -- Defect 1: palette colors must reach the buffer ----------------------

    #[test]
    fn ansi_preserves_fg_bg_bold_underline() {
        let line = ansi::ansi_to_line("\x1b[38;2;95;145;135;48;2;20;20;20;1mhi\x1b[0m");
        assert_eq!(ansi::line_plain(&line), "hi");
        let sp = &line.spans[0];
        assert_eq!(sp.style.fg, Some(Color::Rgb(95, 145, 135)));
        assert_eq!(sp.style.bg, Some(Color::Rgb(20, 20, 20)));
        assert!(sp
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        let line = ansi::ansi_to_line("\x1b[4mun\x1b[24m");
        assert!(line.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED));
    }

    #[test]
    fn kitty_placeholder_keeps_id_and_diacritics() {
        let lines = ansi::ansi_to_lines(&preview::placeholder_grid(1, 1, 2));
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 2));
        write_line(&mut buf, 0, 0, 1, &lines[0]);
        write_line(&mut buf, 0, 1, 1, &lines[1]);
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buf[(0, 1)].fg, Color::Indexed(1));
        assert_eq!(buf[(0, 0)].symbol(), "\u{10EEEE}\u{0305}\u{0305}");
        assert_eq!(buf[(0, 1)].symbol(), "\u{10EEEE}\u{030D}\u{0305}");
    }

    #[test]
    fn wide_kitty_placeholder_uses_distinct_column_diacritics() {
        // Regression: the old 16-entry table clamped every later column
        // to the same diacritic, so wide image tiles aliased and kitty
        // smeared the diagram over subsequent terminal content.
        let lines = ansi::ansi_to_lines(&preview::placeholder_grid(17, 200, 1));
        let mut buf = Buffer::empty(Rect::new(0, 0, 200, 1));
        write_line(&mut buf, 0, 0, 200, &lines[0]);
        let symbols: std::collections::HashSet<&str> =
            (0..200).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(symbols.len(), 200);
        assert_eq!(buf[(15, 0)].symbol(), "\u{10EEEE}\u{0305}\u{0357}");
        assert_eq!(buf[(16, 0)].symbol(), "\u{10EEEE}\u{0305}\u{035B}");

        let rows = ansi::ansi_to_lines(&preview::placeholder_grid(17, 1, 60));
        let row_symbols: std::collections::HashSet<&str> = rows
            .iter()
            .map(|row| row.spans[0].content.as_ref())
            .collect();
        assert_eq!(row_symbols.len(), 60, "row diacritics must not alias");
    }

    #[test]
    fn kitty_transmit_uses_protocol_continuation_chunks() {
        let encoded = preview::kitty_transmit(17, &[7; 5_000]);
        let commands: Vec<&str> = encoded
            .split("\x1b_G")
            .filter(|command| !command.is_empty())
            .collect();
        assert_eq!(commands.len(), 2);
        assert!(commands[0].starts_with("a=t,f=100,i=17,q=2,m=1;"));
        assert!(commands[1].starts_with("q=2,m=0;"));
        assert!(!commands[1].contains("a=t"));
        assert!(!commands[1].contains("i=17"));
    }

    #[test]
    fn write_line_expands_tabs_and_ignores_controls() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        write_line(&mut buf, 0, 0, 10, &Line::from("a\tb\rc\u{0007}d\u{007f}e"));

        let rendered: String = (0..10).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(rendered, "a    bcde ");
    }

    #[test]
    fn write_line_preserves_combining_marks() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        write_line(&mut buf, 0, 0, 2, &Line::from("e\u{0301}x"));

        assert_eq!(buf[(0, 0)].symbol(), "e\u{0301}");
        assert_eq!(buf[(1, 0)].symbol(), "x");
    }

    #[test]
    fn palette_colors_reach_the_buffer() {
        let mut app = App::new_test(100, 30);
        app.sel_model = "test-model".into();
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello world".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let term = frame(&mut app, 100, 30);

        // Full-screen base paint carries the theme background.
        assert_eq!(cell(&term, 0, 0).bg, ansi::c_background());
        assert_eq!(cell(&term, 99, 29).bg, ansi::c_background());

        // The viewport owns its top padding; the user card starts next.
        assert_eq!(cell(&term, 1, 0).bg, ansi::c_background());
        assert_eq!(cell(&term, 1, 1).bg, ansi::c_card_light());
        let x = first_char_x(&term, 2).expect("conversation row has content");
        assert_eq!(x, 2);
        assert_eq!(cell(&term, x, 2).symbol(), "h");
        assert_eq!(cell(&term, x, 2).fg, ansi::c_foreground());
        assert_eq!(cell(&term, 96, 2).bg, ansi::c_card_light());
        assert_eq!(cell(&term, 1, 24).bg, ansi::c_background());

        // Prompt card has full-width card-light wash and one-cell padding.
        for y in 25..=27 {
            assert_eq!(cell(&term, 1, y).bg, ansi::c_card_light());
            assert_eq!(cell(&term, 96, y).bg, ansi::c_card_light());
            assert!(!row_text(&term, y).contains('─'));
        }

        // Status bar head is dim-styled, not frame-colored.
        let sx = first_char_x(&term, 28).expect("status bar drawn");
        assert_eq!(
            cell(&term, sx, 28).fg,
            ansi::c_muted(),
            "status bar must not fall back to the frame fg"
        );
    }

    // -- Defect 2: chrome rows match Go's View() ordering ---------------------

    #[test]
    fn chrome_rows_match_borderless_layout_100x30() {
        let mut app = App::new_test(100, 30);
        app.sel_model = "test-model".into();
        app.cwd = "/work".into();
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello world".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let term = frame(&mut app, 100, 30);

        // viewport_h=25 → card top=25, input=26, card bottom=27,
        // status=28, cwd footer=29. The former working row now shares
        // the status bar, so the viewport grew into row 24.
        assert!(
            row_text(&term, 2).contains("hello world"),
            "user content follows viewport and card padding"
        );
        assert_eq!(row_text(&term, 24).trim(), "", "last viewport row is empty");
        for y in 25..=27 {
            assert!(
                row_text(&term, y).trim().is_empty(),
                "empty prompt card row"
            );
            assert_eq!(cell(&term, 1, y).bg, ansi::c_card_light());
        }
        let status = row_text(&term, 28);
        assert!(
            status.contains("test-model"),
            "status bar above cwd footer: {status:?}"
        );
        assert!(!status.contains('─'), "no border bleeds into status row");
        let cwd_row = row_text(&term, 29);
        assert!(
            cwd_row.contains("/work"),
            "cwd footer replaces the bottom padding: {cwd_row:?}"
        );
        assert_eq!(
            cell(&term, 1, 29).fg,
            ansi::c_muted_extra(),
            "cwd footer text is muted_extra"
        );
        assert_eq!(
            cell(&term, 1, 29).bg,
            ansi::c_background(),
            "cwd footer has no background"
        );
    }

    #[test]
    fn prompt_text_sits_above_status() {
        let mut app = App::new_test(100, 30);
        app.sel_model = "test-model".into();
        app.input.set_value("hi there");
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let term = frame(&mut app, 100, 30);
        assert!(row_text(&term, 26).contains("hi there"), "input row 26");
        assert_eq!(cell(&term, 1, 26).symbol(), " ");
        assert_eq!(cell(&term, 2, 26).symbol(), "h");
        assert!(row_text(&term, 28).contains("test-model"));
    }

    #[test]
    fn stale_boot_dims_self_heal_from_render_area() {
        // Production boots with width=80,height=24 defaults and only
        // learns the real size from the terminal; Go gets an initial
        // WindowSizeMsg. Rendering at 100x30 must land the chrome in
        // the right rows regardless of the stale state.
        let mut app = App::new_test(80, 24);
        app.sel_model = "test-model".into();
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let term = frame(&mut app, 100, 30);
        let status = row_text(&term, 28);
        assert!(
            status.contains("test-model"),
            "status bar must be above the cwd footer, got {status:?}"
        );
        assert!(
            !row_text(&term, 23).contains("test-model"),
            "stale 80x24 geometry must not leak into the frame"
        );
    }

    // -- Resize handling -------------------------------------------------------

    #[test]
    fn resize_rerenders_at_new_size() {
        let mut app = App::new_test(100, 30);
        app.sel_model = "test-model".into();
        app.cwd = "/work".into();
        app.input.set_value("hi there");
        app.blocks.push(Block {
            kind: BlockKind::User,
            text: "hello".into(),
            ..Default::default()
        });
        app.refresh_viewport();
        let backend = TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        draw_into(&mut app, &mut term);

        term.backend_mut().resize(80, 24);
        draw_into(&mut app, &mut term);

        // 80x24: vp_h=19 → card top=19, input=20,
        // card bottom=21, status=22, cwd footer=23.
        assert_eq!(term.backend().buffer().area.width, 80);
        assert_eq!(term.backend().buffer().area.height, 24);
        assert!(
            row_text(&term, 22).contains("test-model"),
            "status above cwd footer"
        );
        assert!(!row_text(&term, 22).contains('─'));
        assert!(
            row_text(&term, 23).contains("/work"),
            "cwd footer replaces the bottom padding"
        );
        assert_eq!(
            cell(&term, 1, 23).fg,
            ansi::c_muted_extra(),
            "cwd footer text is muted_extra"
        );
        assert_eq!(
            cell(&term, 1, 23).bg,
            ansi::c_background(),
            "cwd footer has no background"
        );
        assert!(row_text(&term, 20).contains("hi there"), "input row 20");
        for y in 19..=21 {
            assert_eq!(cell(&term, 1, y).bg, ansi::c_card_light());
        }
        assert!(
            row_text(&term, 18).trim().is_empty(),
            "last viewport row 18"
        );
    }
}
