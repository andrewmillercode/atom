//! fullscreen_view.rs is a reusable fullscreen picker template: a
//! bordered view with a title, a description line, an editable search
//! input, a list of selectable rows (with optional section headers),
//! and a footer summary. Each view provides its own title, description,
//! rows, and search-filter behavior; everything else (chrome, search
//! rendering, scroll math, click hit-testing, key navigation) is shared.
//!
//! This keeps the `OverlayKind::Fork` overlay and any future ones
//! consistent — and trivially extensible: add another `OverlayKind`,
//! build a `ViewSpec`, hand it to `render_view`.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::ansi;
use crate::prompt::wrap_plain;

/// One row in the view's scrollable list. `Header` rows are non-selectable
/// section labels; `Item` rows are clickable and carry an identifier so
/// the caller can map a selection back to a domain object.
#[derive(Debug, Clone)]
pub enum ViewRow {
    Header(String),
    Item(ViewItem),
    /// Pre-rendered line (with its own styling) shown verbatim, e.g.
    /// the /stats report body. Not selectable and not clickable.
    Raw(Vec<Span<'static>>),
}

#[derive(Debug, Clone)]
pub struct ViewItem {
    /// Stable identifier — `None` when the row is a "latest / all" sentinel
    /// (the "fork from latest" row, the "all sessions" row, etc.). The
    /// caller decides what `None` means at confirm time.
    pub id: Option<String>,
    /// Primary label, including any leading bullet/marker the view wants.
    pub label: String,
    /// Right-aligned short tag, e.g. `"14 msg"`, `"sonnet-4"`.
    pub trailing: String,
    /// Muted secondary text, e.g. a model id or timestamp.
    pub meta: String,
    /// Glyph shown on unselected rows instead of the default two-space
    /// indent (e.g. `"→ "` for the current session, `"● "` for the
    /// active theme). Selected rows always use `"▸ "`.
    pub marker: String,
    /// Hex colors rendered as small chips after the label (theme
    /// swatches). Each chip is two cells wide plus a one-cell gap.
    pub swatch: Vec<String>,
}

impl ViewItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            id: None,
            label: label.into(),
            trailing: String::new(),
            meta: String::new(),
            marker: String::new(),
            swatch: Vec::new(),
        }
    }
}

/// Visual treatment for the search row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchStyle {
    /// Three-row bordered box (legacy default). The list chrome has its
    /// own borders and the search box used to match that aesthetic.
    #[default]
    Bordered,
    /// Single inline row: no border, a muted-extra background wash
    /// across the row, and a block cursor on the first character of
    /// the displayed value (placeholder when empty, query otherwise).
    /// Used by `/fork`.
    Inline,
}

/// 1 tile of padding between the terminal edge and the view chrome
/// (title, description, search, list, footer), on every side.
pub const EDGE_PAD: usize = 1;

/// Width of the drawable content area inside the edge padding.
pub fn content_width(term_width: usize) -> usize {
    term_width.saturating_sub(2 * EDGE_PAD).max(1)
}

/// Height of the drawable content area inside the edge padding.
pub fn content_height(term_height: usize) -> usize {
    term_height.saturating_sub(2 * EDGE_PAD).max(1)
}

/// The full-terminal rect inset by [`EDGE_PAD`] on every side. Callers
/// draw a fullscreen view into this rect so the content never touches
/// the terminal edge.
pub fn padded_rect(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x.saturating_add(EDGE_PAD as u16),
        y: area.y.saturating_add(EDGE_PAD as u16),
        width: area.width.saturating_sub(2 * EDGE_PAD as u16),
        height: area.height.saturating_sub(2 * EDGE_PAD as u16),
    }
}

/// State the renderer needs to paint one frame of a fullscreen view.
/// Build one in `view.rs` per overlay kind.
pub struct ViewSpec<'a> {
    /// Big bold title.
    pub title: String,
    /// Smaller description line, drawn below the title. Empty string hides it.
    pub description: String,
    /// Text shown in the search box when empty. Use the empty string to
    /// hide the search row (renders no border, no prompt).
    pub search_placeholder: String,
    /// Current search query value (managed by the App via `overlay_q`).
    pub search_query: &'a str,
    /// True when the user has selected the search contents (Cmd+A); the
    /// row renders with the inverted selection style.
    pub search_selected: bool,
    /// Visual treatment for the search row. `Bordered` keeps the legacy
    /// 3-row boxed look; `Inline` renders a single borderless row with a
    /// muted background and a block cursor on the first character.
    pub search_style: SearchStyle,
    /// Already-filtered rows in display order.
    pub rows: &'a [ViewRow],
    /// Currently selected row index in `rows`. Skipped by navigation helpers.
    pub selected: usize,
    /// Index of the first visible list row (scroll window). Rows before
    /// it are neither rendered nor hit-tested.
    pub scroll: usize,
    /// Footer text drawn below the list, e.g. `"4/42 user messages"`.
    /// Empty string hides the footer.
    pub footer: &'a str,
    /// Loading spinner text — when Some, replaces the list and footer
    /// with a single animated row. Title/description/search still render.
    pub loading: Option<&'a str>,
    /// Spinner frame for the loading state.
    pub spinner_frame: usize,
}

/// Renders a fullscreen view into terminal lines. The output fits in the
/// given width; the caller trims vertical overflow.
pub fn render_view(spec: &ViewSpec<'_>, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let width = width.max(1);

    // --- title + description --------------------------------------------
    // Mocked chrome: title hugs the top-left, the `esc` dismiss hint is
    // right-aligned on the title row, and the description sits directly
    // beneath the title with a larger gap before the search bar.
    out.extend(title_lines(&spec.title, "esc", width));
    if !spec.description.is_empty() {
        out.extend(wrap_lines(&spec.description, ansi::style_dim(), width));
    }

    // --- search input ---------------------------------------------------
    let has_search = !spec.search_placeholder.is_empty();
    if has_search {
        // Two blank rows between the description and the search bar —
        // the mocked spacing leaves the input breathing room.
        out.push(Line::from(""));
        out.push(Line::from(""));
        out.extend(render_search(
            spec.search_query,
            &spec.search_placeholder,
            spec.search_selected,
            spec.search_style,
            width,
        ));
    }

    // --- loading state short-circuits the list -------------------------
    if let Some(loading) = spec.loading {
        out.push(Line::from(""));
        let frame =
            crate::app::MINIDOT_FRAMES[spec.spinner_frame % crate::app::MINIDOT_FRAMES.len()];
        out.push(header_line(&format!("{frame} {loading}")));
        return out;
    }

    // --- list ----------------------------------------------------------
    out.push(Line::from(""));
    if !spec.rows.is_empty() {
        out.extend(render_rows(spec.rows, spec.selected, width, spec.scroll));
    } else if spec.search_query.is_empty() {
        out.push(header_line("nothing here yet"));
    } else {
        out.push(header_line("no matches"));
    }

    // --- footer --------------------------------------------------------
    if !spec.footer.is_empty() {
        out.push(Line::from(""));
        out.push(header_line(spec.footer));
    }

    out
}

/// Returns the row index at the given Y (relative to the view's
/// content origin, i.e. already inside [`EDGE_PAD`] padding) for a view
/// rendered into a content width/height. Returns None when the Y lands
/// above the list (title/description/search chrome) or past the visible
/// rows (scrolled off the bottom).
pub fn hit_test(spec: &ViewSpec<'_>, y: usize, width: usize, height: usize) -> Option<usize> {
    if spec.rows.is_empty() || spec.loading.is_some() {
        return None;
    }
    let top = header_rows(spec, width);
    if y < top {
        return None;
    }
    let list_visible = height.saturating_sub(top).saturating_sub(footer_rows(spec));
    if list_visible == 0 {
        return None;
    }
    let counts = row_line_counts(spec.rows, width);
    let rel = y - top;
    let mut used = 0usize;
    // Scroll window: rows before `scroll` are neither rendered nor
    // hit-tested (rel is relative to the first visible row).
    for (i, count) in counts.iter().enumerate().skip(spec.scroll) {
        let h = (*count).max(1);
        if rel >= used && rel < used + h {
            // Raw rows are display-only; clicking them hits nothing.
            return if matches!(spec.rows[i], ViewRow::Raw(_)) {
                None
            } else {
                Some(i)
            };
        }
        used += h;
        if used > list_visible {
            return None;
        }
    }
    None
}

/// Returns the screen-row index where the list starts (after title,
/// description, and search chrome). Use it to translate `hit_test` row
/// indices back to screen positions.
pub fn list_top(spec: &ViewSpec<'_>, width: usize) -> usize {
    header_rows(spec, width)
}

/// True when a click at `(x, y)` — in content space, i.e. already
/// inside [`EDGE_PAD`] — lands on the right-aligned `esc` dismiss hint
/// the title row carries. Mirrors [`title_lines`]' two layouts: the
/// hint shares the title row when both fit, else it drops to its own
/// right-aligned row below the wrapped title.
pub fn esc_hint_hit(spec: &ViewSpec<'_>, x: usize, y: usize, width: usize) -> bool {
    let hint = "esc";
    let hint_w = unicode_width::UnicodeWidthStr::width(hint);
    if x < width.saturating_sub(hint_w) {
        return false;
    }
    let title_w = unicode_width::UnicodeWidthStr::width(spec.title.as_str());
    if title_w + hint_w < width {
        return y == 0;
    }
    // Hint moved to its own row below the wrapped title.
    let title_rows = wrap_plain(&spec.title, width.max(1)).len();
    y == title_rows
}

/// Returns the number of visual rows the list occupies given `width` and
/// the available `height` after chrome and footer. Callers clamp their
/// row count by this.
pub fn list_visible_rows(spec: &ViewSpec<'_>, width: usize, height: usize) -> usize {
    let top = header_rows(spec, width);
    height.saturating_sub(top).saturating_sub(footer_rows(spec))
}

/// Move the selection in `dir` (typically ±1) while skipping `Header`
/// rows. Clamps at the first/last item.
pub fn move_selection(rows: &[ViewRow], sel: usize, dir: i32) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let mut i = sel as i32 + dir;
    while i >= 0 && i < rows.len() as i32 {
        if matches!(rows[i as usize], ViewRow::Item(_)) {
            return i as usize;
        }
        i += dir;
    }
    sel.min(rows.len().saturating_sub(1))
}

// ---------------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------------

fn wrap_lines(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    wrap_plain(text, width.max(1))
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

/// The title row: title on the left, `esc` dismiss hint right-aligned
/// on the same row (the mocked chrome). When the two don't fit side by
/// side, the hint drops to its own right-aligned row below the title.
fn title_lines(title: &str, hint: &str, width: usize) -> Vec<Line<'static>> {
    let title_w = unicode_width::UnicodeWidthStr::width(title);
    let hint_w = unicode_width::UnicodeWidthStr::width(hint);
    if title_w + hint_w < width {
        let gap = width.saturating_sub(title_w + hint_w);
        return vec![Line::from(vec![
            Span::styled(title.to_string(), ansi::style_title()),
            Span::styled(" ".repeat(gap), ansi::style_dim()),
            Span::styled(hint.to_string(), ansi::style_dim()),
        ])];
    }
    let mut out = wrap_lines(title, ansi::style_title(), width);
    if hint_w <= width {
        let pad = " ".repeat(width - hint_w);
        out.push(Line::from(Span::styled(pad, ansi::style_dim())));
        out.push(Line::from(Span::styled(
            hint.to_string(),
            ansi::style_dim(),
        )));
    }
    out
}

/// Terminal column within the search row of a caret sitting
/// `caret_char` chars into `query` (None = no caret). No padding: the
/// column is also the text cell the caret overlays.
pub fn search_caret_col(query: &str, caret_char: Option<usize>) -> Option<usize> {
    let caret_char = caret_char?;
    Some(
        query
            .chars()
            .take(caret_char)
            .map(unicode_width::UnicodeWidthChar::width)
            .map(|w| w.unwrap_or(0))
            .sum::<usize>(),
    )
}

/// Inverse of [`search_caret_col`]: value-relative char boundary at
/// (or left of) the given row column, for click-to-place the caret.
pub fn search_caret_char_at(query: &str, row_col: usize) -> usize {
    let col = row_col;
    let mut used = 0usize;
    for (i, c) in query.chars().enumerate() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if col < used + w {
            return i;
        }
        used += w;
    }
    query.chars().count()
}

fn header_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), ansi::style_dim()))
}

fn render_search(
    query: &str,
    placeholder: &str,
    selected: bool,
    style: SearchStyle,
    width: usize,
) -> Vec<Line<'static>> {
    match style {
        SearchStyle::Bordered => render_search_bordered(query, placeholder, selected, width),
        SearchStyle::Inline => render_search_inline(query, placeholder, selected, width),
    }
}

/// Legacy 3-row bordered search box. Kept for `/sessions`, `/model`,
/// etc., so they keep their existing chrome.
fn render_search_bordered(
    query: &str,
    placeholder: &str,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let (left_pad, inner_w) = search_border_x(width);
    let right_pad = width.saturating_sub(left_pad + inner_w + 2);
    let value = if query.is_empty() { placeholder } else { query };
    let text = format!(" {value} ");
    let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
    let inner_pad = inner_w.saturating_sub(text_width);
    let left = " ".repeat(left_pad);
    let right = " ".repeat(right_pad);
    let value_style = if query.is_empty() {
        ansi::style_dim()
    } else if selected {
        ansi::style_query_sel()
    } else {
        Style::default()
    };
    let border_style = ansi::style_prompt_border();
    let top = format!("{left}┌{}┐{right}", "─".repeat(inner_w));
    let bot = format!("{left}└{}┘{right}", "─".repeat(inner_w));
    vec![
        Line::from(Span::styled(top, border_style)),
        Line::from(vec![
            Span::styled("│".to_string(), border_style),
            Span::styled(text, value_style),
            Span::styled(
                " ".repeat(inner_pad),
                if query.is_empty() || selected {
                    value_style
                } else {
                    Style::default()
                },
            ),
            Span::styled("│".to_string(), border_style),
        ]),
        Line::from(Span::styled(bot, border_style)),
    ]
}

/// Borderless inline search row for `/fork`: muted-extra background
/// across the full content width, the placeholder dimmed so it
/// visually disappears once the user starts typing. The caret is the
/// terminal's own (blinking) cursor — positioned by the caller via
/// [`search_caret_col`] — so insertion, arrow-key motion and
/// click-to-place behave like a normal input.
fn render_search_inline(
    query: &str,
    placeholder: &str,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let bg_color = ansi::c_muted_deepest();
    let bg = Style::default().bg(bg_color);
    // Dim placeholder text on the muted-deepest wash — fg = muted
    // color, so the row reads as "recessed row with faint text".
    let dim_fg = ansi::c_muted();
    let dim = Style::default().fg(dim_fg).bg(bg_color);
    let plain = Style::default().bg(bg_color);

    let value = if query.is_empty() { placeholder } else { query };

    // Selected (Cmd+A): whole value is highlighted as a single block,
    // and the caret is suppressed (the caller hides the terminal cursor
    // when a selection is active).
    if selected {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if value.is_empty() {
            spans.push(Span::styled(" ".to_string(), ansi::style_query_sel()));
        } else {
            spans.push(Span::styled(value.to_string(), ansi::style_query_sel()));
        }
        let used: usize = spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), bg));
        }
        return vec![Line::from(spans)];
    }

    let style = if query.is_empty() { dim } else { plain };
    let mut spans: Vec<Span<'static>> = Vec::new();
    // The value renders unpadded, starting on the row's first cell —
    // the native caret overlays text cells directly.
    spans.push(Span::styled(value.to_string(), style));

    let used: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), bg));
    }
    vec![Line::from(spans)]
}

/// Pads the search-row width so the borders sit flush against the
/// terminal width. `2` columns of left padding keep the box off the
/// terminal edge.
pub fn search_border_x(width: usize) -> (usize, usize) {
    let x = 2usize.min(width.saturating_sub(2));
    let w = width.saturating_sub(2 * x).max(1);
    (x, w)
}

fn render_rows(
    rows: &[ViewRow],
    selected: usize,
    width: usize,
    scroll: usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(scroll) {
        match row {
            ViewRow::Header(label) => {
                // One blank line between sections (none before the
                // first visible header) — e.g. between "Entire
                // Session" and "From Message".
                if i > scroll && i > 0 {
                    out.push(Line::from(""));
                }
                out.push(header_line(label));
            }
            ViewRow::Item(item) => {
                let indicator = if i == selected {
                    "▸ ".to_string()
                } else if item.marker.is_empty() {
                    "  ".to_string()
                } else {
                    item.marker.clone()
                };
                let style = if i == selected {
                    ansi::style_selected()
                } else {
                    ansi::style_inactive()
                };
                let swatch_w = item.swatch.len() * SWATCH_CELL_W;
                let trailing_w = unicode_width::UnicodeWidthStr::width(item.trailing.as_str());
                let suffix_w = swatch_w + trailing_w;
                let avail = width.saturating_sub(suffix_w + 1);
                let label_with_marker = format!("{indicator}{}", item.label);
                let wrapped = wrap_plain(&label_with_marker, avail.max(1));
                let total_rows = wrapped.len();
                for (li, line) in wrapped.into_iter().enumerate() {
                    if li == 0 && (suffix_w > 0 && !item.trailing.is_empty() || swatch_w > 0) {
                        // Chips (if any) hug the label; the trailing tag
                        // is right-aligned with one cell of clearance.
                        let mut spans = vec![Span::styled(line, style)];
                        for hex in &item.swatch {
                            spans.extend(swatch_spans(hex));
                        }
                        if !item.trailing.is_empty() {
                            let used = unicode_width::UnicodeWidthStr::width(
                                spans
                                    .iter()
                                    .map(|s| s.content.as_ref())
                                    .collect::<String>()
                                    .as_str(),
                            );
                            let gap = width.saturating_sub(used + trailing_w);
                            spans.push(Span::styled(" ".repeat(gap), style));
                            spans.push(Span::styled(item.trailing.clone(), ansi::style_dim()));
                        }
                        out.push(Line::from(spans));
                    } else {
                        out.push(Line::from(Span::styled(line, style)));
                    }
                }
                if !item.meta.is_empty() && total_rows == 1 {
                    // Short labels share a row; meta goes underneath.
                    out.push(Line::from(Span::styled(
                        format!("    {}", item.meta),
                        ansi::style_dim(),
                    )));
                } else if !item.meta.is_empty() && total_rows > 1 {
                    // Multi-line labels don't need a separate meta row —
                    // the trailing already carries the secondary info.
                }
            }
            ViewRow::Raw(spans) => {
                out.push(Line::from(spans.clone()));
            }
        }
    }
    out
}

/// Cells per swatch chip: 2 colored cells + 1 gap.
const SWATCH_CELL_W: usize = 3;

fn swatch_spans(hex: &str) -> Vec<Span<'static>> {
    let (r, g, b) = atom_core::render::colors::hex_to_rgb(hex);
    vec![
        Span::styled("  ".to_string(), Style::default().bg(Color::Rgb(r, g, b))),
        Span::raw(" "),
    ]
}

/// Per-row visual line counts, mirroring `render_rows` (including the
/// header separator blanks). Shared by the scroll sync math so
/// wrapping can never drift between render and navigation.
pub fn row_line_counts(rows: &[ViewRow], width: usize) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| match row {
            // Non-first headers get a blank separator line before them
            // (mirrors render_rows).
            ViewRow::Header(label) => {
                let gap = usize::from(i > 0);
                gap + wrap_plain(label, width.max(1)).len().max(1)
            }
            ViewRow::Item(item) => {
                let swatch_w = item.swatch.len() * SWATCH_CELL_W;
                let trailing_w = unicode_width::UnicodeWidthStr::width(item.trailing.as_str());
                let avail = width.saturating_sub(swatch_w + trailing_w + 1).max(1);
                let n = wrap_plain(&format!("▸ {}", item.label), avail).len();
                let meta_extra = if item.meta.is_empty() { 0 } else { 1 };
                n.max(1) + meta_extra
            }
            ViewRow::Raw(_) => 1,
        })
        .collect()
}

/// Rows rendered above the search row: title row (+ wrapped extra),
/// description, and the gap lines. Matches `render_view`'s layout
/// exactly — used to place the terminal caret on (and hit-test clicks
/// against) the search input.
pub fn search_row_top(spec: &ViewSpec<'_>, width: usize) -> usize {
    let mut n = 0usize;
    let title_w = unicode_width::UnicodeWidthStr::width(spec.title.as_str());
    let hint_w = unicode_width::UnicodeWidthStr::width("esc");
    if title_w + hint_w < width {
        n += 1; // title + esc hint share one row
    } else {
        n += wrap_plain(&spec.title, width.max(1)).len().max(1);
        if hint_w <= width {
            n += 2; // hint pad row + hint row
        }
    }
    if !spec.description.is_empty() {
        n += wrap_plain(&spec.description, width.max(1)).len().max(1);
    }
    if !spec.search_placeholder.is_empty() {
        n += 2; // blank-gap rows before the search bar
    }
    n
}

fn header_rows(spec: &ViewSpec<'_>, width: usize) -> usize {
    let mut n = search_row_top(spec, width);
    let has_search = !spec.search_placeholder.is_empty();
    if has_search {
        // N-row search row(s). Inline adds a wash-only pad row above
        // and below the value row; Bordered adds top/middle/bottom.
        n += search_row_count(spec.search_style);
    }
    if spec.loading.is_some() {
        // blank + 1 loading line.
        return n + 1 + 1;
    }
    n + 1 // blank before list
}

fn footer_rows(spec: &ViewSpec<'_>) -> usize {
    if spec.footer.is_empty() {
        0
    } else {
        1 + 1 // blank + footer
    }
}

/// Number of terminal rows the search input occupies for `style`
/// (Inline: value row + vertical wash pad rows; Bordered: 3 border
/// rows). Callers use it for caret placement and click hit-testing.
pub fn search_row_count(style: SearchStyle) -> usize {
    match style {
        SearchStyle::Bordered => 3,
        SearchStyle::Inline => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> ViewRow {
        ViewRow::Item(ViewItem {
            id: Some(label.into()),
            label: label.into(),
            trailing: String::new(),
            meta: String::new(),
            marker: String::new(),
            swatch: Vec::new(),
        })
    }

    fn header(label: &str) -> ViewRow {
        ViewRow::Header(label.into())
    }

    #[test]
    fn renders_title_description_search_rows_footer() {
        let rows = vec![header("Session"), row("hello")];
        let spec = ViewSpec {
            title: "Fork session".to_string(),
            description: "type to filter, Enter to fork".to_string(),
            search_placeholder: "Search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 1,
            scroll: 0,
            footer: "1/1 user messages",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 80);
        let plain: Vec<String> = lines.iter().map(|l| crate::ansi::line_plain(l)).collect();
        assert!(plain[0].contains("Fork session"), "title row: {plain:?}");
        assert!(
            plain.iter().any(|l| l.contains("type to filter")),
            "description row"
        );
        assert!(plain.iter().any(|l| l.contains("Session")), "header row");
        assert!(plain.iter().any(|l| l.contains("▸ hello")), "selected row");
        assert!(plain.iter().any(|l| l.contains("user messages")), "footer");
    }

    #[test]
    fn search_placeholder_appears_in_search_row() {
        let rows: Vec<ViewRow> = vec![];
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        let joined: String = lines
            .iter()
            .map(|l| crate::ansi::line_plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("search"));
    }

    #[test]
    fn move_selection_skips_header_rows() {
        let rows = vec![header("A"), row("x"), header("B"), row("y")];
        assert_eq!(move_selection(&rows, 0, 1), 1);
        assert_eq!(move_selection(&rows, 1, 1), 3);
        assert_eq!(move_selection(&rows, 3, -1), 1);
        assert_eq!(move_selection(&rows, 3, 1), 3);
    }

    #[test]
    fn hit_test_maps_screen_y_to_row_index() {
        let rows = vec![header("H"), row("a"), row("b")];
        let spec = ViewSpec {
            title: "title".to_string(),
            description: "".to_string(),
            search_placeholder: "".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 1,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let width = 80;
        let top = list_top(&spec, width);
        // y = top → header "H" → Some(0).
        assert_eq!(hit_test(&spec, top, width, 24), Some(0));
        // y = top + 1 → row "a" → Some(1).
        assert_eq!(hit_test(&spec, top + 1, width, 24), Some(1));
        // y = top + 2 → row "b" → Some(2).
        assert_eq!(hit_test(&spec, top + 2, width, 24), Some(2));
    }

    #[test]
    fn hit_test_returns_none_for_chrome_rows() {
        let rows = vec![row("only")];
        let spec = ViewSpec {
            title: "title".to_string(),
            description: "".to_string(),
            search_placeholder: "".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        // y=0 lands on the title.
        assert!(hit_test(&spec, 0, 80, 24).is_none());
    }

    #[test]
    fn loading_state_replaces_list_with_spinner() {
        let rows = vec![row("x")];
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "1/1",
            loading: Some("loading session..."),
            spinner_frame: 2,
        };
        let plain: Vec<String> = render_view(&spec, 60)
            .into_iter()
            .map(|l| crate::ansi::line_plain(&l))
            .collect();
        assert!(plain.iter().any(|l| l.contains("loading session")));
        assert!(!plain.iter().any(|l| l.contains("1/1")));
    }

    #[test]
    fn footer_hidden_when_empty() {
        let rows: Vec<ViewRow> = vec![];
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 60);
        let plain: Vec<String> = lines.iter().map(|l| crate::ansi::line_plain(l)).collect();
        assert!(plain.iter().any(|l| l.contains("nothing here yet")));
    }

    #[test]
    fn render_search_box_borders_align_with_width() {
        let rows: Vec<ViewRow> = vec![];
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        // The top border row must not exceed the width (40 columns).
        for line in &lines {
            assert!(
                crate::ansi::line_width(line) <= 40,
                "over-wide line: {:?}",
                crate::ansi::line_plain(line)
            );
        }
    }

    #[test]
    fn short_label_meta_renders_on_its_own_line() {
        let rows = vec![ViewRow::Item(ViewItem {
            id: Some("1".into()),
            label: "Convert the loader".into(),
            trailing: String::new(),
            meta: "14:02".into(),
            marker: String::new(),
            swatch: Vec::new(),
        })];
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Bordered,
            rows: &rows,
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        let plain: Vec<String> = lines.iter().map(|l| crate::ansi::line_plain(l)).collect();
        assert!(plain.iter().any(|l| l.contains("14:02")));
    }

    #[test]
    fn inline_search_renders_unpadded_input_with_placeholder() {
        // Empty query + Inline: placeholder shown inline on one row,
        // no border characters, no padding, no in-buffer block cursor
        // — the caret is the terminal's own cursor (search_caret_col).
        let spec = ViewSpec {
            title: "Fork session".to_string(),
            description: "".to_string(),
            search_placeholder: "Search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Inline,
            rows: &[],
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        // Layout: title(+esc), blank, blank, search row, blank,
        // "nothing here yet".
        assert!(
            lines.len() >= 4,
            "expected at least title + gap + search: got {:?}",
            lines
        );
        let search = crate::ansi::line_plain(&lines[3]);
        assert!(
            !search.contains('┌') && !search.contains('└'),
            "no border characters: {search:?}"
        );
        assert!(
            search.contains('S') && search.contains("earch"),
            "placeholder rendered inline: {search:?}"
        );
        // The whole value renders as one dim span — unpadded, starting
        // on the row's first cell — on the muted-deepest wash.
        let s_cell = &lines[3].spans[0];
        assert_eq!(s_cell.content.as_ref(), "Search");
        assert_eq!(
            s_cell.style.bg,
            Some(crate::ansi::c_muted_deepest()),
            "placeholder sits on the muted-deepest wash: {search:?}"
        );
        // The caret position is exposed for the terminal cursor. No
        // padding: the column is the text cell the caret overlays.
        assert_eq!(search_caret_col("", Some(0)), Some(0));
        assert_eq!(search_caret_col("abc", Some(1)), Some(1));
        assert_eq!(search_caret_col("abc", Some(3)), Some(3));
        assert_eq!(search_caret_col("abc", None), None);
        assert_eq!(search_caret_char_at("abc", 2), 2);
        assert_eq!(search_caret_char_at("abc", 0), 0);
        assert_eq!(search_caret_char_at("abc", 99), 3);
        // The input is a single row.
        assert_eq!(search_row_count(SearchStyle::Inline), 1);
    }

    #[test]
    fn inline_search_displaces_placeholder_when_typing() {
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "Search".to_string(),
            search_query: "abc",
            search_selected: false,
            search_style: SearchStyle::Inline,
            rows: &[],
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        let search = crate::ansi::line_plain(&lines[3]);
        assert!(search.contains("abc"), "typed query appears: {search:?}");
        assert!(
            !search.contains("Search"),
            "placeholder is hidden when typing: {search:?}"
        );
    }

    #[test]
    fn inline_search_selection_highlights_whole_query() {
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "Search".to_string(),
            search_query: "abc",
            search_selected: true,
            search_style: SearchStyle::Inline,
            rows: &[],
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        let search_line = &lines[3];
        let mut highlighted = 0usize;
        for span in &search_line.spans {
            if span.style.bg == Some(crate::ansi::c_primary()) {
                highlighted += unicode_width::UnicodeWidthStr::width(span.content.as_ref());
            }
        }
        assert!(
            highlighted >= 3,
            "selected query highlights >= 3 cells (abc): got {highlighted}"
        );
    }

    #[test]
    fn inline_search_padding_fills_full_width() {
        let spec = ViewSpec {
            title: "t".to_string(),
            description: "".to_string(),
            search_placeholder: "Search".to_string(),
            search_query: "",
            search_selected: false,
            search_style: SearchStyle::Inline,
            rows: &[],
            selected: 0,
            scroll: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        assert!(
            crate::ansi::line_width(&lines[3]) == 40,
            "search row fills terminal width"
        );
    }
}
