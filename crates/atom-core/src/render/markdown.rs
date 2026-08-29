//! Ported from markdown.go: renders CommonMark for assistant and
//! compaction text. The Go version drove glamour (atomMarkdownStyle:
//! DarkStyleConfig recolored — foreground body, primary headings,
//! secondary links, warm inline code, margin 0 everywhere,
//! chroma-highlighted fences); a hand-rolled parser reproduced those
//! styling decisions until it was replaced by the pulldown-cmark
//! event stream in this module. The visual contract is unchanged:
//! bullets ("• "), two-column list indents, clean headings, flattened
//! quotes, fenced code delegated to the highlighter
//! (crate::render::highlight), plus GFM tables rendered as muted
//! box-drawing grids that honor column alignment and wrap cell text.
//!
//! Inline math: pulldown emits InlineMath events for `$…$`, which
//! crate::render::math_inline converts to styled Unicode; currency
//! ("$5 vs $10") stays prose because pulldown rejects a closer
//! preceded by whitespace. Display math is owned by the Kitty engine,
//! and render_markdown only ever sees it for the engine's raw-source
//! fallback, so `$$…$$` passes through verbatim. The pandoc `\(…\)`
//! spelling has no pulldown extension, so it is rewritten to `$…$`
//! before parsing — outside code fences and code spans only.
//! On any unexpected failure the caller falls back to wrapLinked so
//! the TUI never blanks, mirroring renderMarkdown's error path.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::colors::{ansi_fg, COLOR_FOREGROUND, COLOR_MUTED, COLOR_SECONDARY, COLOR_SYNTAX_STRING};
use super::highlight::highlight_code;
use super::links::{ansi_wrap, osc8_close, osc8_open, visible_width};
use super::math_inline;

const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const CROSSED_OUT: &str = "\x1b[9m";
const UNDERLINE: &str = "\x1b[4m";
const RESET: &str = "\x1b[0m";

/// renderMarkdown renders CommonMark (plus GFM tables) for assistant
/// and compaction text, trimmed of trailing whitespace like the Go
/// version.
pub fn render_markdown(text: &str, width: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let width = width.max(1);
    let source = text.replace("\r\n", "\n").replace('\r', "\n");
    let prepared = convert_paren_math(&source);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_MATH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut r = Renderer::new(&prepared, width);
    for (event, span) in Parser::new_ext(&prepared, opts).into_offset_iter() {
        r.event(event, span);
    }
    r.finish()
}

// ---------------------------------------------------------------------------
// `\(…\)` → `$…$` preprocessing.
//
// pulldown-cmark has no LaTeX-parenthesis math extension and treats
// `\(` as an escaped paren, so paired spans would degrade to literal
// text. Rewriting them to `$…$` before parsing preserves the old
// renderer's behavior. Fenced code and inline code spans are skipped —
// their backslashes are literal. Pairing is line-local, so the rewrite
// never changes the document's line structure (block ranges stay
// meaningful for looseness checks below).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Fence {
    ch: char,
    len: usize,
}

fn fence_open(line: &str) -> Option<Fence> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = t.chars().take_while(|&x| x == c).count();
    if len < 3 {
        return None;
    }
    let info = t[len..].trim();
    if c == '`' && info.contains('`') {
        return None;
    }
    Some(Fence { ch: c, len })
}

fn fence_close(line: &str, f: Fence) -> bool {
    let t = line.trim();
    t.chars().all(|c| c == f.ch) && t.chars().count() >= f.len
}

fn convert_paren_math(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<Fence> = None;
    for line in text.split('\n') {
        if let Some(f) = fence {
            if fence_close(line, f) {
                fence = None;
            }
            out.push(line.to_string());
            continue;
        }
        if let Some(f) = fence_open(line) {
            fence = Some(f);
            out.push(line.to_string());
            continue;
        }
        out.push(convert_line_paren_math(line));
    }
    out.join("\n")
}

fn convert_line_paren_math(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                // Code span: copy verbatim through the matching run.
                let n = run_len(&chars, i, '`');
                match find_run(&chars, i + n, '`', n) {
                    Some(close) => {
                        out.extend(&chars[i..close + n]);
                        i = close + n;
                    }
                    None => {
                        out.extend(&chars[i..i + n]);
                        i += n;
                    }
                }
            }
            '\\' if chars.get(i + 1) == Some(&'(') => {
                if let Some(close) = find_sub(&chars, i + 2, &['\\', ')']) {
                    out.push('$');
                    out.extend(chars[i + 2..close].iter().copied());
                    out.push('$');
                    i = close + 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn run_len(chars: &[char], from: usize, c: char) -> usize {
    chars[from..].iter().take_while(|&&x| x == c).count()
}

fn find_run(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == c {
            let r = run_len(chars, i, c);
            if r == n {
                return Some(i);
            }
            i += r;
        } else {
            i += 1;
        }
    }
    None
}

fn find_sub(chars: &[char], from: usize, pat: &[char]) -> Option<usize> {
    if pat.is_empty() || chars.len() < pat.len() || from > chars.len() - pat.len() {
        return None;
    }
    (from..=chars.len() - pat.len()).find(|&i| chars[i..i + pat.len()] == *pat)
}

// ---------------------------------------------------------------------------
// Event rendering.
// ---------------------------------------------------------------------------

/// A single currently-open inline span. Closing one resets SGR and
/// re-opens the enclosing spans, so nesting degrades to the same
/// look the old recursion produced.
enum Active {
    /// Hyperlinked span: keeps an OSC 8 region alive.
    Link(String),
    /// A plain color (image alt text).
    Color(&'static str),
    /// A literal SGR prefix (emphasis, strong, strike).
    Sgr(String),
}

impl Active {
    fn open_repr(&self) -> String {
        match self {
            Active::Link(url) => format!(
                "{}{}{}",
                osc8_open(url),
                ansi_fg(COLOR_SECONDARY),
                UNDERLINE
            ),
            Active::Color(c) => ansi_fg(c),
            Active::Sgr(s) => s.clone(),
        }
    }
}

enum Frame {
    Quote,
    List {
        /// Next number for an ordered list; None for bullets.
        next: Option<u64>,
        /// True when blank lines separate items or blocks inside items.
        loose: bool,
        /// Byte offset just past the last completed item, for
        /// loose-list gap detection.
        last_end: Option<usize>,
    },
    Item {
        marker: String,
        /// Set until the item's first output line carries the marker.
        pending: bool,
        /// Set once any output line was produced inside the item.
        emitted: bool,
        /// Byte offset just past the last completed child, for gap
        /// detection between blocks of one item.
        last_end: Option<usize>,
    },
}

struct CodeAcc {
    buf: String,
    lang: String,
}

struct TableAcc {
    aligns: Vec<Alignment>,
    in_head: bool,
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    in_cell: bool,
}

struct Renderer<'a> {
    width: usize,
    /// Prepared source, for blank-line gap detection via event ranges.
    src: &'a str,
    out: Vec<String>,
    frames: Vec<Frame>,
    spans: Vec<Active>,
    inline: String,
    heading: Option<u8>,
    code: Option<CodeAcc>,
    table: Option<TableAcc>,
}

impl<'a> Renderer<'a> {
    fn new(src: &'a str, width: usize) -> Self {
        Renderer {
            width,
            src,
            out: Vec::new(),
            frames: Vec::new(),
            spans: Vec::new(),
            inline: String::new(),
            heading: None,
            code: None,
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>, span: std::ops::Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, span.start),
            Event::End(tag) => self.end(tag, span.end),
            Event::Text(t) => {
                if let Some(code) = self.code.as_mut() {
                    code.buf.push_str(&t);
                } else {
                    self.sink().push_str(&t);
                }
            }
            Event::Code(t) => {
                let sink = self.sink();
                sink.push_str(&ansi_fg(COLOR_SYNTAX_STRING));
                sink.push_str(&t);
                sink.push_str(RESET);
                let reopened: Vec<String> = self.spans.iter().map(Active::open_repr).collect();
                self.sink().push_str(&reopened.concat());
            }
            Event::InlineMath(t) => {
                self.sink().push_str(&math_inline::styled(&t));
            }
            Event::DisplayMath(t) => {
                // The Kitty math engine owns display math and calls
                // render_markdown only for its raw-source fallback
                // (pending/failed renders), so pass the delimiters
                // through verbatim — exactly the appearance before
                // any formula is ready.
                self.sink().push_str("$$");
                self.sink().push_str(&t);
                self.sink().push_str("$$");
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                if let Some(code) = self.code.as_mut() {
                    code.buf.push_str(&t);
                } else {
                    self.sink().push_str(&t);
                }
            }
            Event::SoftBreak => {
                if let Some(code) = self.code.as_mut() {
                    code.buf.push('\n');
                } else {
                    self.sink().push(' ');
                }
            }
            Event::HardBreak => {
                if let Some(code) = self.code.as_mut() {
                    code.buf.push('\n');
                } else {
                    self.sink().push('\n');
                }
            }
            Event::TaskListMarker(checked) => {
                let sink = self.sink();
                sink.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::Rule => {
                self.flush_inline();
                self.emit_block_lines(vec![format!("{}--------{}", ansi_fg(COLOR_MUTED), RESET)]);
            }
            Event::FootnoteReference(_) => {}
        }
    }

    // -- block structure ---------------------------------------------------

    fn start(&mut self, tag: Tag<'_>, start: usize) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                self.gap_mark(start);
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.gap_mark(start);
                self.heading = Some(heading_num(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.gap_mark(start);
                self.frames.push(Frame::Quote);
            }
            Tag::List(ordered) => {
                self.flush_inline();
                self.gap_mark(start);
                self.frames.push(Frame::List {
                    next: ordered,
                    loose: false,
                    last_end: None,
                });
            }
            Tag::Item => {
                self.flush_inline();
                self.gap_mark(start);
                // A loose list puts a blank line before each item
                // except the first.
                if let Some(Frame::List { loose, .. }) = self.frames.last_mut() {
                    if *loose
                        && !self.out.is_empty()
                        && !self.out.last().is_some_and(String::is_empty)
                    {
                        self.out.push(String::new());
                    }
                }
                let marker;
                if let Some(Frame::List { next, .. }) = self.frames.last_mut() {
                    marker = match *next {
                        Some(n) => {
                            *next = Some(n.checked_add(1).unwrap_or(n));
                            format!("{}. ", n)
                        }
                        None => "• ".to_string(),
                    };
                } else {
                    marker = "• ".to_string();
                }
                self.frames.push(Frame::Item {
                    marker,
                    pending: true,
                    emitted: false,
                    last_end: None,
                });
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.gap_mark(start);
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_lowercase()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeAcc {
                    buf: String::new(),
                    lang,
                });
            }
            Tag::HtmlBlock => {
                self.flush_inline();
                self.gap_mark(start);
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.gap_mark(start);
                self.table = Some(TableAcc {
                    aligns,
                    in_head: false,
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: String::new(),
                    in_cell: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.in_cell = true;
                    t.cell.clear();
                }
            }
            Tag::Emphasis => {
                self.open_span(Active::Sgr(format!(
                    "{}{}",
                    ansi_fg(COLOR_FOREGROUND),
                    ITALIC
                )));
            }
            Tag::Strong => {
                self.open_span(Active::Sgr(format!(
                    "{}{}",
                    ansi_fg(COLOR_FOREGROUND),
                    BOLD
                )));
            }
            Tag::Strikethrough => {
                self.open_span(Active::Sgr(CROSSED_OUT.to_string()));
            }
            Tag::Link { dest_url, .. } => {
                self.open_span(Active::Link(dest_url.to_string()));
            }
            Tag::Image { .. } => {
                self.open_span(Active::Color(COLOR_MUTED));
            }
            // Footnote definitions, definition lists, metadata blocks,
            // super/subscripts: disabled by our Options; treat any
            // unknown container as transparent.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, end: usize) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
            }
            TagEnd::Heading(_) => {
                self.flush_inline();
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    let mut buf = code.buf;
                    if !buf.ends_with('\n') {
                        buf.push('\n');
                    }
                    let rendered = highlight_code(&buf, &code.lang, self.content_width());
                    let mut lines: Vec<String> = rendered.split('\n').map(str::to_string).collect();
                    if lines.last().map(String::is_empty).unwrap_or(false) {
                        lines.pop();
                    }
                    self.emit_block_lines(lines);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    let w = self.content_width();
                    let lines = render_table(&t.head, &t.rows, &t.aligns, w);
                    self.emit_block_lines(lines);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    // In 0.13.4 the head holds TableCells directly; the
                    // body's rows arrive via TableRow tags.
                    t.head = std::mem::take(&mut t.row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.in_cell = false;
                    let mut row = std::mem::take(&mut t.row);
                    while row.len() < t.aligns.len() {
                        row.push(String::new());
                    }
                    if t.in_head {
                        t.head = row;
                    } else {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cell = t.cell.trim_end().to_string();
                    t.row.push(std::mem::take(&mut t.cell));
                    t.in_cell = false;
                }
            }
            TagEnd::Item => {
                // Tight list items hold their paragraphs directly (no
                // Paragraph tag), so unwritten inline content ends here.
                self.flush_inline();
                if let Some(Frame::Item {
                    marker, emitted, ..
                }) = self.frames.pop()
                {
                    if !emitted {
                        // Empty list item: just the marker.
                        let d = self.item_depth();
                        self.out
                            .push(format!("{}{}", "  ".repeat(d), marker.trim_end()));
                    }
                }
            }
            TagEnd::List(_) => {
                self.frames.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.frames.pop();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.close_span();
            }
            TagEnd::HtmlBlock => {
                self.flush_inline();
            }
            _ => {
                self.flush_inline();
            }
        }
        // Bookkeep completed-block offsets for loose-list detection.
        match self.frames.last_mut() {
            Some(Frame::Item { last_end, .. }) | Some(Frame::List { last_end, .. }) => {
                *last_end = Some(end);
            }
            _ => {}
        }
    }

    // -- inline spans ------------------------------------------------------

    fn sink(&mut self) -> &mut String {
        if let Some(t) = self.table.as_mut() {
            if t.in_cell {
                return &mut t.cell;
            }
        }
        &mut self.inline
    }

    fn open_span(&mut self, a: Active) {
        let repr = a.open_repr();
        self.sink().push_str(&repr);
        self.spans.push(a);
    }

    fn close_span(&mut self) {
        let popped = self.spans.pop();
        let reopened: String = self
            .spans
            .iter()
            .map(Active::open_repr)
            .collect::<Vec<_>>()
            .concat();
        let sink = self.sink();
        sink.push_str(RESET);
        if matches!(popped, Some(Active::Link(_))) {
            sink.push_str(osc8_close());
        }
        sink.push_str(&reopened);
    }

    // -- layout helpers ----------------------------------------------------

    /// Columns of list indentation currently open.
    fn item_depth(&self) -> usize {
        self.frames
            .iter()
            .filter(|f| matches!(f, Frame::Item { .. }))
            .count()
    }

    fn content_width(&self) -> usize {
        self.width.saturating_sub(2 * self.item_depth()).max(1)
    }

    /// Emits one block's lines, applying list indentation and the
    /// innermost pending item marker, with a blank line between
    /// consecutive blocks (tight items join directly).
    fn emit_block_lines(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        // Find the innermost item (skipping transparent quote frames):
        // its marker goes on the first line while pending, and it
        // decides whether a blank separator is needed.
        let mut marker: Option<String> = None;
        let mut sep = !self.out.is_empty();
        for frame in self.frames.iter().rev() {
            if let Frame::Item {
                marker: m,
                pending,
                emitted,
                ..
            } = frame
            {
                marker = pending.then(|| m.clone());
                sep = !*pending && *emitted;
                break;
            }
        }
        for frame in self.frames.iter_mut() {
            if let Frame::Item {
                pending, emitted, ..
            } = frame
            {
                *pending = false;
                *emitted = true;
            }
        }
        if sep && !self.out.last().is_some_and(String::is_empty) {
            self.out.push(String::new());
        }
        let d = self.item_depth();
        let indent = "  ".repeat(d);
        for (i, line) in lines.into_iter().enumerate() {
            let prefixed = if i == 0 {
                match marker.take() {
                    Some(m) => format!("{}{}{}", "  ".repeat(d.saturating_sub(1)), m, line),
                    None => format!("{}{}", indent, line),
                }
            } else {
                format!("{}{}", indent, line)
            };
            self.out.push(prefixed);
        }
    }

    /// Flushes pending inline content as a paragraph (or styled
    /// heading when inside one).
    fn flush_inline(&mut self) {
        let body = std::mem::take(&mut self.inline);
        if body.trim().is_empty() {
            self.heading = None;
            return;
        }
        let w = self.content_width();
        match self.heading.take() {
            Some(level) => {
                let color = if level >= 6 {
                    COLOR_MUTED
                } else {
                    COLOR_FOREGROUND
                };
                let styled = format!("{}{}{}{}", ansi_fg(color), BOLD, body.trim(), RESET);
                self.emit_block_lines(
                    wrap_colored(&styled, w, COLOR_FOREGROUND)
                        .split('\n')
                        .map(str::to_string)
                        .collect(),
                );
            }
            None => {
                self.emit_block_lines(
                    wrap_colored(&body, w, COLOR_FOREGROUND)
                        .split('\n')
                        .map(str::to_string)
                        .collect(),
                );
            }
        }
    }

    /// Marks the ancestor list loose when a blank line immediately
    /// precedes a direct-child block start (CommonMark's loose-list
    /// rule — event byte ranges absorb trailing newlines, so we look
    /// at the previous source line instead).
    fn gap_mark(&mut self, start: usize) {
        // Only direct children of a list item (or the list itself)
        // count; blank lines inside nested quotes belong to the quote.
        // The container must already have a completed child — the blank
        // line before a list's first item is ordinary block separation,
        // not looseness.
        let eligible = match self.frames.last() {
            Some(Frame::List {
                last_end: Some(_), ..
            })
            | Some(Frame::Item {
                last_end: Some(_), ..
            }) => true,
            _ => false,
        };
        if !eligible {
            return;
        }
        let s = &self.src[..start.min(self.src.len())];
        let line_start = s.rfind('\n').map(|i| i + 1).unwrap_or(0);
        if line_start == 0 {
            return;
        }
        let prev_line = match s[..line_start - 1].rfind('\n') {
            Some(i) => &s[i + 1..line_start - 1],
            None => &s[..line_start - 1],
        };
        if !prev_line.trim().is_empty() {
            return;
        }
        for frame in self.frames.iter_mut().rev() {
            if let Frame::List { loose, .. } = frame {
                *loose = true;
                break;
            }
        }
    }

    fn finish(mut self) -> String {
        self.flush_inline();
        self.out
            .join("\n")
            .trim_end_matches([' ', '\t', '\n'])
            .to_string()
    }
}

fn heading_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Wraps styled text and paints each line with the base foreground the
/// way the Go renderer did.
fn wrap_colored(styled: &str, width: usize, base_fg: &str) -> String {
    ansi_wrap(styled, width)
        .split('\n')
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{}{}{}", ansi_fg(base_fg), l, RESET)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

/// Renders one GFM table as a box-drawing grid: muted borders, bold
/// header, alignment honored, cells wrapped to fit `width`.
fn render_table(
    head: &[String],
    rows: &[Vec<String>],
    aligns: &[Alignment],
    width: usize,
) -> Vec<String> {
    let ncol = head.len();
    if ncol == 0 {
        return Vec::new();
    }
    let align_of = |i: usize| aligns.get(i).copied().unwrap_or(Alignment::None);

    // Logical rows: header first, padded to the column count.
    let mut logical: Vec<Vec<String>> = vec![head.to_vec()];
    logical.extend(rows.iter().cloned());
    for r in &mut logical {
        r.truncate(ncol);
        while r.len() < ncol {
            r.push(String::new());
        }
    }

    // Natural column widths (longest cell line, header included).
    let mut nat: Vec<usize> = vec![1; ncol];
    for r in &logical {
        for (i, cell) in r.iter().enumerate() {
            let w = cell
                .split('\n')
                .map(visible_width)
                .max()
                .unwrap_or(0)
                .max(1);
            nat[i] = nat[i].max(w);
        }
    }
    // Shrink to fit: largest cap such that all columns ≤ cap fit.
    let overhead = 3 * ncol + 1; // two padding spaces + two borders per col, plus closing border
    if nat.iter().sum::<usize>() + overhead > width {
        let budget = width.saturating_sub(overhead).max(ncol);
        let mut lo = 1usize;
        let mut hi = nat.iter().copied().max().unwrap_or(1);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let capped: usize = nat.iter().map(|w| (*w).min(mid)).sum();
            if capped <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        for w in &mut nat {
            *w = (*w).min(lo);
        }
    }

    // Wrap cell text to its column width.
    let wrapped: Vec<Vec<Vec<String>>> = logical
        .iter()
        .map(|r| {
            r.iter()
                .enumerate()
                .map(|(i, cell)| wrap_cell(cell, nat[i]))
                .collect()
        })
        .collect();

    let bar = |left: &str, joint: &str, right: &str| {
        let mut s = ansi_fg(COLOR_MUTED);
        s.push_str(left);
        for (i, w) in nat.iter().enumerate() {
            if i > 0 {
                s.push_str(RESET);
                s.push_str(&ansi_fg(COLOR_MUTED));
                s.push_str(joint);
            }
            s.push_str(&"─".repeat(w + 2));
        }
        s.push_str(&ansi_fg(COLOR_MUTED));
        s.push_str(right);
        s.push_str(RESET);
        s
    };

    let mut out = Vec::new();
    out.push(bar("┌", "┬", "┐"));
    for (li, row) in wrapped.iter().enumerate() {
        let is_head = li == 0;
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        for line_i in 0..height {
            let mut s = String::new();
            s.push_str(&ansi_fg(COLOR_MUTED));
            s.push('│');
            s.push_str(RESET);
            for (i, cell) in row.iter().enumerate() {
                let mut content = cell.get(line_i).cloned().unwrap_or_default();
                if is_head {
                    content = format!("{}{}{}{}", ansi_fg(COLOR_FOREGROUND), BOLD, content, RESET);
                }
                let used = visible_width(&content);
                let pad = nat[i].saturating_sub(used);
                let (left, right) = match align_of(i) {
                    Alignment::Right => (pad, 0),
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                s.push_str(RESET);
                s.push(' ');
                s.push_str(&" ".repeat(left));
                s.push_str(&content);
                s.push_str(&" ".repeat(right));
                s.push(' ');
                s.push_str(&ansi_fg(COLOR_MUTED));
                s.push('│');
                s.push_str(RESET);
            }
            out.push(s);
        }
        // Close with ├ after the header, └ after the last body row;
        // body rows do not get separators between them.
        let closer = if li + 1 == wrapped.len() {
            "└"
        } else if li == 0 {
            "├"
        } else {
            continue;
        };
        let joint = if closer == "└" { "┴" } else { "┼" };
        out.push(bar(closer, joint, closer));
    }
    out
}

/// Wraps one cell's text (ANSI-styled) to the given column width,
/// preserving hard breaks as separate logical lines.
fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for part in cell.split('\n') {
        for l in ansi_wrap(part, width).split('\n') {
            out.push(l.to_string());
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::colors::{ansi_fg, COLOR_FOREGROUND, COLOR_PRIMARY};
    use crate::render::links::visible_width;

    fn strip(ansi: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in ansi.chars() {
            if c == '\x1b' {
                esc = true;
                continue;
            }
            if esc {
                if c.is_ascii_alphabetic() {
                    esc = false;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn empty_text_renders_empty() {
        assert_eq!(render_markdown("", 80), "");
    }

    #[test]
    fn heading_contains_title() {
        let rendered = render_markdown("# Title", 80);
        assert_eq!(strip(&rendered), "Title");
        assert!(!rendered.contains(&ansi_fg(COLOR_PRIMARY)));
    }

    #[test]
    fn bold_keeps_word() {
        let rendered = render_markdown("this is **bold** text", 80);
        let got = strip(&rendered);
        assert!(got.contains("bold"), "{}", got);
        assert!(!rendered.contains(&ansi_fg(COLOR_PRIMARY)));
    }

    #[test]
    fn code_fence_keeps_code() {
        let got = strip(&render_markdown("```go\nfmt.Println(\"hi\")\n```", 80));
        assert!(got.contains("fmt.Println"), "{}", got);
    }

    #[test]
    fn fence_delegates_to_highlighter_colors() {
        let got = render_markdown("```go\nfunc main() {}\n```", 80);
        assert!(strip(&got).contains("func main"), "{}", got);
        assert!(
            got.contains(&ansi_fg(crate::render::colors::COLOR_PRIMARY)),
            "fenced code should use the chroma keyword color, got {:?}",
            got
        );
    }

    #[test]
    fn link_label_rendered() {
        let got = strip(&render_markdown("[label](https://example.com)", 80));
        assert!(got.contains("label"), "{}", got);
    }

    #[test]
    fn links_carry_osc8_targets() {
        let got = render_markdown("[label](https://example.com)", 80);
        assert!(
            got.contains("\x1b]8;;https://example.com\x07"),
            "missing OSC 8 open: {got:?}"
        );
        assert!(got.contains(crate::render::links::osc8_close()));

        let got = render_markdown("<https://example.com/a>", 80);
        assert!(
            got.contains("\x1b]8;;https://example.com/a\x07"),
            "missing OSC 8 open for autolink: {got:?}"
        );

        // Plain text never gains hyperlink wrappers.
        let got = render_markdown("no links here", 80);
        assert!(!got.contains("\x1b]8;;"), "{got:?}");
    }

    #[test]
    fn paragraph_stays_visible() {
        let got = strip(&render_markdown("plain words stay visible", 80));
        assert!(got.contains("plain words stay visible"), "{}", got);
    }

    #[test]
    fn wrapping_respects_width() {
        const WIDTH: usize = 40;
        let src = format!(
            "{}\n\n```\n{}\n```",
            "word ".repeat(40),
            "x".repeat(WIDTH * 3 + 7)
        );
        let got = strip(&render_markdown(&src, WIDTH));
        for (i, line) in got.split('\n').enumerate() {
            let w = visible_width(line);
            assert!(
                w <= WIDTH,
                "line {} width {} exceeds wrap {}: {:?}",
                i,
                w,
                WIDTH,
                line
            );
        }
    }

    #[test]
    fn fenced_code_preserves_blank_lines_when_wrapped() {
        let got = strip(&render_markdown("```\nabcdefghijk\n\nsecond line\n```", 6));

        assert_eq!(got, "abcdef\nghijk\n\nsecond\nline");
    }

    #[test]
    fn inline_code_has_warm_foreground_without_background() {
        let got = render_markdown("run `make all` now", 80);
        assert!(got.contains(&ansi_fg(COLOR_SYNTAX_STRING)));
        assert!(!got.contains("\x1b[48;"), "{got:?}");
        assert!(got.contains("make all"));
    }

    #[test]
    fn lists_use_bullets_and_hanging_indent() {
        let got = strip(&render_markdown("- alpha\n- beta\ndelta\n- third\n", 80));
        assert!(got.contains("• alpha"), "{}", got);
        // Lazy continuation folds the unindented line into the item.
        assert!(got.contains("• beta delta"), "{}", got);
        assert!(got.contains("• third"), "{}", got);
    }

    #[test]
    fn long_list_items_wrap_with_hanging_indent() {
        let got = strip(&render_markdown(
            "- one two three four five six seven eight nine ten eleven twelve\n",
            20,
        ));
        assert!(
            got.lines().any(|l| l.starts_with("  ")),
            "continuation lines should hang two columns: {:?}",
            got
        );
    }

    #[test]
    fn ordered_lists_number_from_start() {
        let got = strip(&render_markdown("3. three\n4. four\n", 80));
        assert!(got.contains("3. three"), "{}", got);
        assert!(got.contains("4. four"), "{}", got);
    }

    #[test]
    fn blockquotes_render_as_normal_content() {
        let got = strip(&render_markdown("> quoted line\n> more\n", 80));
        // Consecutive "> " lines form one paragraph.
        assert_eq!(got, "quoted line more");
        assert!(!got.contains('|'));
    }

    #[test]
    fn fenced_code_inside_a_quote_keeps_syntax_colors() {
        let got = render_markdown("> ```rust\n> fn main() {}\n> ```\n", 80);

        assert!(strip(&got).contains("fn main"), "{got:?}");
        assert!(got.contains(&ansi_fg(COLOR_PRIMARY)), "{got:?}");
        assert!(!strip(&got).contains('|'));
    }

    #[test]
    fn horizontal_rule_dashes_muted() {
        let got = render_markdown("---\n", 80);
        assert!(got.contains(&ansi_fg(COLOR_MUTED)), "{:?}", got);
        assert!(got.contains("--------"));
    }

    #[test]
    fn images_show_alt_muted() {
        let got = render_markdown("![alt text](https://x/y.png)", 80);
        assert!(got.contains(&ansi_fg(COLOR_MUTED)), "{:?}", got);
        assert!(got.contains("alt text"));
    }

    #[test]
    fn hard_break_survives() {
        let got = strip(&render_markdown("one  \ntwo", 80));
        assert!(got.contains("one\ntwo"), "{:?}", got);
    }

    #[test]
    fn fallback_foreground_on_plain_text() {
        let got = render_markdown("just words here", 80);
        assert!(got.contains(&ansi_fg(COLOR_FOREGROUND)), "{:?}", got);
    }

    // -- tables ----------------------------------------------------------

    #[test]
    fn table_renders_box_grid() {
        let md = "| Method | Size |\n|---|---|\n| GET | 212 |\n| POST | 1024 |\n";
        let got = strip(&render_markdown(md, 80));
        assert!(got.contains('┌') && got.contains('┐'), "{got:?}");
        assert!(got.contains('├') && got.contains('┼'), "{got:?}");
        assert!(got.contains('└') && got.contains('┴'), "{got:?}");
        for cell in ["Method", "Size", "GET", "212", "POST", "1024"] {
            assert!(got.contains(cell), "missing {cell}: {got:?}");
        }
        // Borders are muted.
        assert!(render_markdown(md, 80).contains(&ansi_fg(COLOR_MUTED)));
    }

    #[test]
    fn table_honors_alignment() {
        let md = "| left | center | right |\n|:-----|:------:|------:|\n| a | bb | ccc |\n";
        let got = strip(&render_markdown(md, 80));
        let line = got.lines().find(|l| l.contains("ccc")).expect("row line");
        let cells: Vec<&str> = line.split('│').filter(|c| !c.trim().is_empty()).collect();
        assert_eq!(cells.len(), 3, "{line:?}");
        assert_eq!(cells[0].trim(), "a");
        assert_eq!(cells[1].trim(), "bb");
        assert_eq!(cells[2].trim(), "ccc");
        // Left: flush against the cell's left padding.
        assert!(cells[0].starts_with(" a"), "cell0={:?}", cells[0]);
        // Center: symmetric padding on both sides.
        assert!(
            cells[1].starts_with(' ') && cells[1].ends_with(' '),
            "cell1={:?}",
            cells[1]
        );
        // Right: flush against the cell's right edge.
        assert!(cells[2].ends_with("ccc "), "cell2={:?}", cells[2]);
    }

    #[test]
    fn table_escaped_pipe_stays_in_cell() {
        let md = "| expr | ok |\n|---|---|\n| `a\\|b` | yes |\n";
        let got = strip(&render_markdown(md, 80));
        assert!(got.contains("a|b"), "escaped pipe lost: {got:?}");
        let widths: Vec<usize> = got
            .lines()
            .filter(|l| l.contains('│'))
            .map(visible_width)
            .collect();
        // All content rows share one grid width (3 borders, cells padded).
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn table_wraps_narrow_cells_instead_of_overflowing() {
        let md = "| col |\n|---|\n| one two three four five six |\n";
        let got = strip(&render_markdown(md, 20));
        for line in got.lines() {
            assert!(visible_width(line) <= 20, "too wide: {line:?}");
        }
        assert!(got.contains("one two"), "{got:?}");
        assert!(got.contains("six"), "{got:?}");
    }

    #[test]
    fn table_inline_styling_survives() {
        let md = "| name | note |\n|---|---|\n| **big** | `code` |\n";
        let got = render_markdown(md, 80);
        assert!(got.contains(&ansi_fg(COLOR_SYNTAX_STRING)), "{got:?}");
        assert!(got.contains(&BOLD), "{got:?}");
    }

    #[test]
    fn table_inside_list_indents() {
        let md = "- item\n\n  | a |\n  |---|\n  | b |\n";
        let got = strip(&render_markdown(md, 40));
        let row = got.lines().find(|l| l.contains('┌')).unwrap();
        assert!(
            row.starts_with("  "),
            "table should hang under marker: {row:?}"
        );
        assert!(row.contains('┌'), "{row:?}");
    }

    #[test]
    fn loose_list_gets_blank_lines_between_items() {
        let got = strip(&render_markdown("- one\n\n- two\n", 80));
        assert!(
            got.contains("• one\n\n• two"),
            "loose list should separate items: {got:?}"
        );
        let tight = strip(&render_markdown("- one\n- two\n", 80));
        assert!(tight.contains("• one\n• two"), "{tight:?}");
    }
}
