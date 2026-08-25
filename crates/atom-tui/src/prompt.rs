//! prompt.rs is the multi-line input field: a small textarea port with
//! word-wrapped display lines, an internal scroll that pins the cursor
//! into view, a minimal selection model, and image-chip substitution
//! for [IMG n] markers.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
enum TokenKind {
    Space,
    Word,
}

fn kind_of(ch: char) -> TokenKind {
    if ch == ' ' || ch == '\t' {
        TokenKind::Space
    } else {
        TokenKind::Word
    }
}

fn display_width(ch: char) -> usize {
    if ch == '\t' {
        TAB_WIDTH
    } else if ch.is_control() {
        0
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
struct WrappedUnit {
    text: String,
    source_start: usize,
    source_end: usize,
    cells: usize,
    kind: TokenKind,
}

#[derive(Debug, Clone)]
struct WrappedRow {
    text: String,
    source_start: usize,
    source_end: usize,
    units: Vec<WrappedUnit>,
    cells: usize,
}

impl WrappedRow {
    fn empty(source_offset: usize) -> Self {
        Self {
            text: String::new(),
            source_start: source_offset,
            source_end: source_offset,
            units: Vec::new(),
            cells: 0,
        }
    }
}

#[derive(Debug)]
struct WrappedLayout {
    rows: Vec<WrappedRow>,
    cursor_positions: Vec<(usize, usize, usize)>,
    width: usize,
}

impl WrappedLayout {
    fn cursor_position(&self, offset: usize) -> (usize, usize) {
        let index = self
            .cursor_positions
            .binary_search_by_key(&offset, |&(source, _, _)| source)
            .unwrap_or_else(|index| index.saturating_sub(1));
        let (_, row, col) = self.cursor_positions[index];
        (row, col.min(self.width))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Prompt {
    /// value may contain '\n'
    pub value: String,
    /// cursor as a byte offset into value (always on a char boundary)
    pub cursor: usize,
    /// selection anchors (byte offsets); Some when one exists
    pub sel: Option<(usize, usize)>,
    /// internal vertical scroll in wrapped display lines
    pub scroll_y: usize,
}

impl Prompt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_value(&mut self, v: &str) {
        self.value = v.to_string();
        self.cursor = self.value.len();
        self.sel = None;
        self.scroll_y = 0;
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.sel = None;
        self.scroll_y = 0;
    }

    // -- cursor plumbing ---------------------------------------------------

    fn snap_back(&mut self) {
        while self.cursor > 0 && !self.value.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    fn prev_boundary(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.snap_back();
        }
    }

    fn next_boundary(&mut self) {
        let start = self.cursor.min(self.value.len());
        if let Some(ch) = self.value[start..].chars().next() {
            self.cursor = start + ch.len_utf8();
        }
    }

    // -- editing -----------------------------------------------------------

    /// Deletes the selection if any; returns whether it did.
    fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.normalized_selection() {
            if a != b {
                self.value.replace_range(a..b, "");
                self.cursor = a;
                self.sel = None;
                return true;
            }
            self.sel = None;
        }
        false
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        self.snap_back();
        self.value.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn newline(&mut self) {
        self.insert_str("\n");
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.snap_back();
        if self.cursor == 0 {
            return;
        }
        let start = self.cursor;
        self.prev_boundary();
        self.value.replace_range(self.cursor..start, "");
    }

    pub fn delete_fwd(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.snap_back();
        let start = self.cursor;
        self.next_boundary();
        if self.cursor > start {
            self.value.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    // -- movement ----------------------------------------------------------

    pub fn left(&mut self) {
        self.clear_selection();
        self.snap_back();
        self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.clear_selection();
        self.next_boundary();
    }

    pub fn up(&mut self) {
        let (line, col) = self.line_col_cells();
        if line == 0 {
            return;
        }
        self.move_to_line_col(line - 1, col);
    }

    pub fn down(&mut self) {
        let (line, col) = self.line_col_cells();
        if line + 1 >= self.logical_lines().len().max(1) {
            return;
        }
        self.move_to_line_col(line + 1, col);
    }

    pub fn home(&mut self) {
        let (line, _) = self.line_col_cells();
        self.move_to_line_col(line, 0);
    }

    pub fn end(&mut self) {
        let (line, _) = self.line_col_cells();
        let len = self
            .logical_lines()
            .get(line)
            .map(|l| l.chars().map(display_width).sum())
            .unwrap_or(0);
        self.move_to_line_col(line, len);
    }

    // -- selection ---------------------------------------------------------

    pub fn select_all(&mut self) {
        if !self.value.is_empty() {
            self.sel = Some((0, self.value.len()));
        }
    }

    pub fn has_selection(&self) -> bool {
        matches!(self.sel, Some((a, b)) if a != b)
    }

    pub fn selected_text(&self) -> String {
        match self.normalized_selection() {
            Some((a, b)) => self.value[a..b].to_string(),
            None => String::new(),
        }
    }

    pub fn clear_selection(&mut self) {
        self.sel = None;
    }

    fn normalized_selection(&self) -> Option<(usize, usize)> {
        let (a, b) = self.sel?;
        let a = self.char_boundary_at_or_before(a);
        let b = self.char_boundary_at_or_before(b);
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    fn char_boundary_at_or_before(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.value.len());
        while !self.value.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    // -- geometry ----------------------------------------------------------

    /// Logical (newline-split) lines.
    pub fn logical_lines(&self) -> Vec<&str> {
        self.value.split('\n').collect()
    }

    /// Cursor as (logical line index, cell column).
    pub fn line_col_cells(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.value.char_indices() {
            if i >= self.cursor {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += display_width(ch);
            }
        }
        (line, col)
    }

    fn move_to_line_col(&mut self, target_line: usize, target_col: usize) {
        let mut offset = 0usize;
        for (i, l) in self.logical_lines().iter().enumerate() {
            if i == target_line {
                let mut cells = 0usize;
                let mut cut = l.len();
                for (ci, ch) in l.char_indices() {
                    if cells >= target_col {
                        cut = ci;
                        break;
                    }
                    cells += display_width(ch);
                }
                self.cursor = offset + cut;
                return;
            }
            offset += l.len() + 1;
        }
        self.cursor = self.value.len();
    }

    /// Word-wrap the value to width cells.
    pub fn wrapped(&self, width: usize) -> Vec<String> {
        self.wrapped_layout(width)
            .rows
            .into_iter()
            .map(|row| row.text)
            .collect()
    }

    /// Display-line count at width (at least 1).
    pub fn content_lines(&self, width: usize) -> usize {
        self.wrapped_layout(width).rows.len()
    }

    /// Cursor position over wrapped rows: (display line, cell column).
    pub fn cursor_display_pos(&self, width: usize) -> (usize, usize) {
        let layout = self.wrapped_layout(width);
        layout.cursor_position(self.char_boundary_at_or_before(self.cursor))
    }

    fn wrapped_layout(&self, width: usize) -> WrappedLayout {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut cursor_positions: Vec<(usize, usize, usize)> = Vec::new();
        let mut byte_pos = 0usize;

        for logical in self.logical_lines() {
            let line_rows = wrap_line(logical, byte_pos, width);
            let first_row = rows.len();
            rows.extend(line_rows);

            for (relative_row, row) in rows[first_row..].iter().enumerate() {
                let mut col = 0;
                for unit in &row.units {
                    if cursor_positions.last().map(|point| point.0) != Some(unit.source_start) {
                        cursor_positions.push((unit.source_start, first_row + relative_row, col));
                    }
                    col += unit.cells;
                }
            }

            let last_row = rows.len() - 1;
            let line_end = byte_pos + logical.len();
            if cursor_positions.last().map(|point| point.0) != Some(line_end) {
                cursor_positions.push((line_end, last_row, rows[last_row].cells));
            }
            byte_pos = line_end + 1;
        }

        WrappedLayout {
            rows,
            cursor_positions,
            width,
        }
    }

    /// Renders the field to `height` rows at `width`, scrolling
    /// internally to keep the cursor visible. Returns the rows plus the
    /// cursor (cell column, row within returned rows). Selection is
    /// rendered with reverse video.
    pub fn view(
        &mut self,
        width: usize,
        height: usize,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let height = height.max(1);
        let layout = self.wrapped_layout(width);
        let total = layout.rows.len();
        let (dl, dc) = layout.cursor_position(self.char_boundary_at_or_before(self.cursor));

        // Pin the cursor into view (textarea repositionView analog).
        if dl < self.scroll_y {
            self.scroll_y = dl;
        } else if dl >= self.scroll_y + height {
            self.scroll_y = dl + 1 - height;
        }
        self.scroll_y = self.scroll_y.min(total.saturating_sub(1));

        let sel = self.normalized_selection();
        let sel_style = Style::new().add_modifier(Modifier::REVERSED);

        let mut rows = Vec::with_capacity(height);
        for y in 0..height {
            let spans = layout
                .rows
                .get(self.scroll_y + y)
                .map(|row| selection_spans(row, sel, sel_style))
                .unwrap_or_else(|| vec![Span::raw(String::new())]);
            rows.push(Line::from(spans));
        }
        let cur = if dl >= self.scroll_y && dl < self.scroll_y + height {
            Some((dc, dl - self.scroll_y))
        } else {
            None
        };
        (rows, cur)
    }
}

fn selection_spans(
    row: &WrappedRow,
    selection: Option<(usize, usize)>,
    selection_style: Style,
) -> Vec<Span<'static>> {
    let Some((selection_start, selection_end)) = selection else {
        return vec![Span::raw(row.text.clone())];
    };
    if selection_end <= row.source_start || selection_start >= row.source_end {
        return vec![Span::raw(row.text.clone())];
    }

    let mut spans = Vec::new();
    let mut text = String::new();
    let mut styled = None;
    for unit in &row.units {
        if unit.text.is_empty() {
            continue;
        }
        let unit_styled = selection_start < unit.source_end && selection_end > unit.source_start;
        if styled.is_some_and(|current| current != unit_styled) {
            let content = std::mem::take(&mut text);
            if styled == Some(true) {
                spans.push(Span::styled(content, selection_style));
            } else {
                spans.push(Span::raw(content));
            }
        }
        styled = Some(unit_styled);
        text.push_str(&unit.text);
    }
    if !text.is_empty() {
        if styled == Some(true) {
            spans.push(Span::styled(text, selection_style));
        } else {
            spans.push(Span::raw(text));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(row.text.clone()));
    }
    spans
}

/// Greedy cell-width word wrap for plain text. A hyphen is always a
/// breakpoint, matching links::ansi_wrap semantics used elsewhere.
pub fn wrap_plain(s: &str, limit: usize) -> Vec<String> {
    wrap_line(s, 0, limit.max(1))
        .into_iter()
        .map(|row| row.text)
        .collect()
}

fn wrap_line(s: &str, source_offset: usize, limit: usize) -> Vec<WrappedRow> {
    if s.is_empty() {
        return vec![WrappedRow::empty(source_offset)];
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;

    for mut token in split_units(s, source_offset) {
        let token_kind = token[0].kind;
        let token_width = units_width(&token);
        if current_width + token_width > limit && current_width > 0 {
            push_wrapped_row(&mut rows, &mut current, true);
            current_width = 0;
        }

        match token_kind {
            TokenKind::Space => {
                // Space runs can be split, including a tab's normalized spaces.
                while units_width(&token) > limit {
                    let mut width = current_width;
                    let mut take = 0;
                    for unit in &token {
                        if width + unit.cells > limit && width > 0 {
                            break;
                        }
                        width += unit.cells;
                        take += 1;
                    }
                    if take == 0 {
                        take = 1;
                    }
                    current.extend(token.drain(..take));
                    push_wrapped_row(&mut rows, &mut current, false);
                    current_width = 0;
                }
                current_width += units_width(&token);
                current.extend(token);
            }
            TokenKind::Word if token_width > limit => {
                for unit in token {
                    if current_width + unit.cells > limit && current_width > 0 {
                        push_wrapped_row(&mut rows, &mut current, true);
                        current_width = 0;
                    }
                    current_width += unit.cells;
                    current.push(unit);
                }
            }
            TokenKind::Word => {
                current_width += token_width;
                current.extend(token);
            }
        }
    }
    if !current.is_empty() || rows.is_empty() {
        push_wrapped_row(&mut rows, &mut current, false);
    }
    rows
}

fn split_units(s: &str, source_offset: usize) -> Vec<Vec<WrappedUnit>> {
    let mut tokens = Vec::new();
    let mut current = Vec::new();
    let mut current_kind = TokenKind::Word;

    for (index, ch) in s.char_indices() {
        let kind = kind_of(ch);
        if !current.is_empty() && kind != current_kind {
            tokens.push(std::mem::take(&mut current));
        }
        current_kind = kind;
        let source_start = source_offset + index;
        let source_end = source_start + ch.len_utf8();
        if ch == '\t' {
            for _ in 0..TAB_WIDTH {
                current.push(WrappedUnit {
                    text: " ".to_string(),
                    source_start,
                    source_end,
                    cells: 1,
                    kind,
                });
            }
        } else {
            let cells = display_width(ch);
            current.push(WrappedUnit {
                text: if ch.is_control() {
                    String::new()
                } else {
                    ch.to_string()
                },
                source_start,
                source_end,
                cells,
                kind,
            });
        }
        if kind == TokenKind::Word && ch == '-' {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn units_width(units: &[WrappedUnit]) -> usize {
    units.iter().map(|unit| unit.cells).sum()
}

fn push_wrapped_row(rows: &mut Vec<WrappedRow>, current: &mut Vec<WrappedUnit>, trim_end: bool) {
    let mut units = std::mem::take(current);
    if trim_end {
        for unit in units.iter_mut().rev() {
            if unit.text.is_empty() {
                continue;
            }
            if unit.kind != TokenKind::Space {
                break;
            }
            unit.text.clear();
            unit.cells = 0;
        }
    }
    let source_start = units.first().map(|unit| unit.source_start).unwrap_or(0);
    let source_end = units
        .last()
        .map(|unit| unit.source_end)
        .unwrap_or(source_start);
    let text = units.iter().map(|unit| unit.text.as_str()).collect();
    let cells = units_width(&units);
    rows.push(WrappedRow {
        text,
        source_start,
        source_end,
        units,
        cells,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_cursor_roundtrip() {
        let mut p = Prompt::new();
        p.insert_str("hello");
        assert_eq!(p.value, "hello");
        assert_eq!(p.line_col_cells(), (0, 5));
        p.left();
        p.insert_str("p");
        assert_eq!(p.value, "hellpo");
        assert_eq!(p.line_col_cells(), (0, 5));
    }

    #[test]
    fn multiline_movement() {
        let mut p = Prompt::new();
        p.set_value("abc\ndefgh\nx"); // leaves the cursor at the very end
        p.up();
        p.up();
        p.home();
        assert_eq!(p.line_col_cells(), (0, 0));
        p.end();
        p.down();
        assert_eq!(p.line_col_cells(), (1, 3));
        p.up();
        assert_eq!(p.line_col_cells(), (0, 3));
        p.home();
        assert_eq!(p.line_col_cells(), (0, 0));
        p.end();
        p.down();
        p.end();
        assert_eq!(p.line_col_cells(), (1, 5));
    }

    #[test]
    fn wrapping_counts_display_lines() {
        let mut p = Prompt::new();
        p.set_value("aaaa bbbb cccc dddd");
        assert_eq!(p.content_lines(10), 2);
        p.set_value("one\ntwo\nthree");
        assert_eq!(p.content_lines(20), 3);
        assert_eq!(Prompt::new().content_lines(20), 1);
    }

    #[test]
    fn view_pins_cursor_into_view() {
        let mut p = Prompt::new();
        let long = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        p.set_value(&long);
        p.move_to_line_col(19, 0);
        let (rows, cur) = p.view(40, 4);
        assert_eq!(rows.len(), 4);
        let (_, dy) = cur.unwrap();
        assert_eq!(rows[dy].spans[0].content.as_ref(), "line19");
        // Scrolled internally so the last line is visible.
        assert!(p.scroll_y > 0);
    }

    #[test]
    fn selection_delete_and_text() {
        let mut p = Prompt::new();
        p.set_value("hello world");
        p.sel = Some((0, p.value.len()));
        assert_eq!(p.selected_text(), "hello world");
        p.backspace();
        assert_eq!(p.value, "");
        // reversed anchors normalize
        p.set_value("abcdef");
        p.sel = Some((4, 2));
        assert_eq!(p.selected_text(), "cd");
    }

    #[test]
    fn wrap_plain_breaks_on_spaces_and_hyphens() {
        let got = wrap_plain("alpha-beta gamma", 8);
        assert_eq!(got.join("\n"), "alpha-\nbeta\ngamma");
    }

    #[test]
    fn select_all_renders_reversed() {
        let mut p = Prompt::new();
        p.insert_str("hello world");
        p.select_all();
        assert!(p.has_selection());
        let (lines, _) = p.view(80, 1);
        // The single row should be one reversed span (all selected).
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert!(lines[0].spans[0].style.add_modifier == ratatui::style::Modifier::REVERSED);
    }

    #[test]
    fn partial_selection_splits_spans() {
        let mut p = Prompt::new();
        p.insert_str("hello world");
        p.sel = Some((0, 5)); // select "hello"
        let (lines, _) = p.view(80, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content, "hello");
        assert_eq!(lines[0].spans[1].content, " world");
    }

    #[test]
    fn multibyte_selection_uses_source_offsets_after_trimmed_wrap_space() {
        let reversed = Modifier::REVERSED;
        for (selection, selected_span) in [((4, 6), 0), ((6, 8), 1)] {
            let mut p = Prompt::new();
            p.set_value("aaa éé");
            p.sel = Some(selection);

            let (lines, _) = p.view(4, 2);
            assert_eq!(lines[0].spans[0].content, "aaa");
            assert_eq!(lines[1].spans.len(), 2);
            assert_eq!(lines[1].spans[0].content, "é");
            assert_eq!(lines[1].spans[1].content, "é");
            assert_eq!(lines[1].spans[selected_span].style.add_modifier, reversed);
            assert_eq!(
                lines[1].spans[1 - selected_span].style.add_modifier,
                Modifier::empty()
            );
        }
    }

    #[test]
    fn cursor_tracks_discarded_wrap_space() {
        let mut p = Prompt::new();
        p.set_value("aaa éé");

        p.cursor = 3;
        assert_eq!(p.cursor_display_pos(4), (0, 3));
        p.cursor = 4;
        assert_eq!(p.cursor_display_pos(4), (1, 0));
        p.cursor = 6;
        assert_eq!(p.cursor_display_pos(4), (1, 1));
    }

    #[test]
    fn combining_and_cjk_offsets_follow_display_cells() {
        let mut p = Prompt::new();
        p.set_value("e\u{301}界x");
        assert_eq!(p.wrapped(3), ["e\u{301}界", "x"]);

        p.cursor = 1;
        assert_eq!(p.cursor_display_pos(3), (0, 1));
        p.cursor = 3;
        assert_eq!(p.cursor_display_pos(3), (0, 1));
        p.cursor = 6;
        assert_eq!(p.cursor_display_pos(3), (1, 0));

        p.sel = Some((3, 6));
        let (lines, _) = p.view(3, 2);
        assert_eq!(lines[0].spans[0].content, "e\u{301}");
        assert_eq!(lines[0].spans[1].content, "界");
        assert_eq!(lines[0].spans[1].style.add_modifier, Modifier::REVERSED);
    }

    #[test]
    fn tabs_and_controls_cannot_produce_invalid_geometry_or_slices() {
        let mut p = Prompt::new();
        p.set_value("\t\u{1b}界é");
        assert!(p.wrapped(2).iter().all(|row| !row.contains('\u{1b}')));

        for cursor in [0, 1, 2, 5, 7] {
            p.cursor = cursor;
            let (_, col) = p.cursor_display_pos(2);
            assert!(col <= 2);
        }

        // Public offsets may be set by callers; snap invalid UTF-8 offsets safely.
        p.sel = Some((3, 6));
        let _ = p.selected_text();
        let _ = p.view(2, 8);
    }
}
