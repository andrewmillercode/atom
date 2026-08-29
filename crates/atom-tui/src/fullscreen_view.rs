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

use ratatui::style::Style;
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
}

/// State the renderer needs to paint one frame of a fullscreen view.
/// Build one in `view.rs` per overlay kind.
pub struct ViewSpec<'a> {
    /// Big bold title.
    pub title: &'a str,
    /// Smaller description line, drawn below the title. Empty string hides it.
    pub description: &'a str,
    /// Text shown in the search box when empty. Use the empty string to
    /// hide the search row (renders no border, no prompt).
    pub search_placeholder: &'a str,
    /// Current search query value (managed by the App via `overlay_q`).
    pub search_query: &'a str,
    /// True when the user has selected the search contents (Cmd+A) — the
    /// row renders with the inverted selection style.
    pub search_selected: bool,
    /// Already-filtered rows in display order.
    pub rows: &'a [ViewRow],
    /// Currently selected row index in `rows`. Skipped by navigation helpers.
    pub selected: usize,
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
    out.extend(wrap_lines(spec.title, ansi::style_title(), width));
    if !spec.description.is_empty() {
        out.push(Line::from(""));
        out.extend(wrap_lines(spec.description, ansi::style_dim(), width));
    }

    // --- search input ---------------------------------------------------
    let has_search = !spec.search_placeholder.is_empty();
    if has_search {
        out.push(Line::from(""));
        out.extend(render_search(
            spec.search_query,
            spec.search_placeholder,
            spec.search_selected,
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
        out.extend(render_rows(spec.rows, spec.selected, width));
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

/// Returns the row index at the given screen Y for a view rendered into a
/// terminal of the given width and height. Returns None when the Y lands
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
    let counts = row_line_counts(spec, width);
    let rel = y - top;
    let mut used = 0usize;
    for (i, count) in counts.iter().enumerate() {
        let h = (*count).max(1);
        if rel >= used && rel < used + h {
            return Some(i);
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

fn header_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), ansi::style_dim()))
}

fn render_search(
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

/// Pads the search-row width so the borders sit flush against the
/// terminal width. `2` columns of left padding keep the box off the
/// terminal edge.
pub fn search_border_x(width: usize) -> (usize, usize) {
    let x = 2usize.min(width.saturating_sub(2));
    let w = width.saturating_sub(2 * x).max(1);
    (x, w)
}

fn render_rows(rows: &[ViewRow], selected: usize, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match row {
            ViewRow::Header(label) => {
                out.push(header_line(label));
            }
            ViewRow::Item(item) => {
                let marker = if i == selected { "▸ " } else { "  " };
                let style = if i == selected {
                    ansi::style_selected()
                } else {
                    ansi::style_inactive()
                };
                let trailing_w = unicode_width::UnicodeWidthStr::width(item.trailing.as_str());
                let avail = width.saturating_sub(trailing_w + 1);
                let label_with_marker = format!("{marker}{}", item.label);
                let wrapped = wrap_plain(&label_with_marker, avail.max(1));
                let total_rows = wrapped.len();
                for (li, line) in wrapped.into_iter().enumerate() {
                    if li == 0 && !item.trailing.is_empty() {
                        let used = unicode_width::UnicodeWidthStr::width(line.as_str());
                        let gap = width.saturating_sub(used + trailing_w);
                        out.push(Line::from(vec![
                            Span::styled(line, style),
                            Span::styled(" ".repeat(gap), style),
                            Span::styled(item.trailing.clone(), ansi::style_dim()),
                        ]));
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
        }
    }
    out
}

fn row_line_counts(spec: &ViewSpec<'_>, width: usize) -> Vec<usize> {
    spec.rows
        .iter()
        .map(|row| match row {
            ViewRow::Header(label) => wrap_plain(label, width.max(1)).len().max(1),
            ViewRow::Item(item) => {
                let trailing_w = unicode_width::UnicodeWidthStr::width(item.trailing.as_str());
                let avail = width.saturating_sub(trailing_w + 1).max(1);
                let n = wrap_plain(&format!("▸ {}", item.label), avail).len();
                let meta_extra = if item.meta.is_empty() { 0 } else { 1 };
                n.max(1) + meta_extra
            }
        })
        .collect()
}

fn header_rows(spec: &ViewSpec<'_>, width: usize) -> usize {
    let mut n = 0usize;
    n += wrap_plain(spec.title, width.max(1)).len().max(1);
    if !spec.description.is_empty() {
        n += 1 + wrap_plain(spec.description, width.max(1)).len().max(1);
    }
    let has_search = !spec.search_placeholder.is_empty();
    if has_search {
        // blank + 3-row search box.
        n += 1 + 3;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> ViewRow {
        ViewRow::Item(ViewItem {
            id: Some(label.into()),
            label: label.into(),
            trailing: String::new(),
            meta: String::new(),
        })
    }

    fn header(label: &str) -> ViewRow {
        ViewRow::Header(label.into())
    }

    #[test]
    fn renders_title_description_search_rows_footer() {
        let rows = vec![header("Session"), row("hello")];
        let spec = ViewSpec {
            title: "Fork session",
            description: "type to filter, Enter to fork",
            search_placeholder: "Search",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 1,
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
            title: "t",
            description: "",
            search_placeholder: "search",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
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
            title: "title",
            description: "",
            search_placeholder: "",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 1,
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
            title: "title",
            description: "",
            search_placeholder: "",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
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
            title: "t",
            description: "",
            search_placeholder: "search",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
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
            title: "t",
            description: "",
            search_placeholder: "",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
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
            title: "t",
            description: "",
            search_placeholder: "search",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
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
        })];
        let spec = ViewSpec {
            title: "t",
            description: "",
            search_placeholder: "",
            search_query: "",
            search_selected: false,
            rows: &rows,
            selected: 0,
            footer: "",
            loading: None,
            spinner_frame: 0,
        };
        let lines = render_view(&spec, 40);
        let plain: Vec<String> = lines.iter().map(|l| crate::ansi::line_plain(l)).collect();
        assert!(plain.iter().any(|l| l.contains("14:02")));
    }
}
