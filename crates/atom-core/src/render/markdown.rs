//! Ported from markdown.go: renders CommonMark for assistant and
//! compaction text. The Go version drives glamour with atomMarkdownStyle
//! (DarkStyleConfig recolored: foreground body, primary headings/emphasis,
//! secondary links, warm inline code, margin 0 everywhere,
//! chroma-highlighted fences). This module reproduces those styling
//! decisions directly: bullets ("• "), two-column list indents, clean
//! headings, flattened quotes, and fenced code delegated to the highlighter
//! (crate::render::highlight). On any
//! unexpected failure it falls back to wrapLinked so the TUI never
//! blanks, mirroring renderMarkdown's error path.

use super::colors::{ansi_fg, COLOR_FOREGROUND, COLOR_MUTED, COLOR_SECONDARY, COLOR_SYNTAX_STRING};
use super::highlight::highlight_code;
use super::links::{ansi_wrap, osc8_close, osc8_open};
use super::math_inline;

const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const CROSSED_OUT: &str = "\x1b[9m";
const UNDERLINE: &str = "\x1b[4m";
const RESET: &str = "\x1b[0m";

/// renderMarkdown renders CommonMark for assistant and compaction text,
/// trimmed of trailing whitespace like the Go version.
pub fn render_markdown(text: &str, width: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let width = width.max(1);
    let lines: Vec<String> = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect();
    let blocks = parse_blocks(&lines);
    let out = render_blocks(&blocks, width);
    out.trim_end_matches([' ', '\t', '\n']).to_string()
}

// ---------------------------------------------------------------------------
// Block model + parsing.
// ---------------------------------------------------------------------------

enum Block {
    Para(Vec<String>),
    Heading(u8, Vec<String>),
    Code { lang: String, body: Vec<String> },
    Quote(Vec<Block>),
    List(ListBlock),
    Rule,
}

struct ListBlock {
    ordered: bool,
    start: u64,
    items: Vec<Vec<Block>>,
    loose: bool,
}

#[derive(Clone, Copy)]
struct Fence<'a> {
    ch: char,
    len: usize,
    info: &'a str,
}

fn fence_open(line: &str) -> Option<Fence<'_>> {
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
    Some(Fence { ch: c, len, info })
}

fn fence_close(line: &str, f: Fence<'_>) -> bool {
    let t = line.trim();
    t.chars().all(|c| c == f.ch) && t.chars().count() >= f.len
}

fn is_hr(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 3 {
        return false;
    }
    let c = compact.chars().next().unwrap();
    (c == '-' || c == '*' || c == '_') && compact.chars().all(|x| x == c)
}

fn atx_heading(line: &str) -> Option<(u8, Vec<String>)> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let mut title = rest.trim().to_string();
    // Strip an optional closing sequence of #'s.
    let trimmed_end = title.trim_end_matches('#');
    if trimmed_end.len() != title.len()
        && (trimmed_end.is_empty() || trimmed_end.ends_with(' ') || trimmed_end.ends_with('\t'))
    {
        title = trimmed_end.trim_end().to_string();
    }
    Some((
        hashes as u8,
        if title.is_empty() {
            Vec::new()
        } else {
            vec![title]
        },
    ))
}

struct ItemMarker {
    indent: usize,
    ordered: bool,
    number: u64,
    /// byte offset of the item content on the marker line
    content_col_byte: usize,
}

fn item_marker(line: &str) -> Option<ItemMarker> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let t = &line[indent.min(line.len())..];
    let mut chars = t.char_indices();
    let (_, c) = chars.next()?;
    let (ordered, marker_len) = match c {
        '-' | '*' | '+' => (false, 1),
        '0'..='9' => {
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 9 {
                return None;
            }
            let after = t.as_bytes().get(digits).copied();
            if after != Some(b'.') && after != Some(b')') {
                return None;
            }
            (true, digits + 1)
        }
        _ => return None,
    };
    let rest = &t[marker_len..];
    let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    if spaces == 0 && !rest.is_empty() {
        // "-foo" is not a list item; "1.foo" neither.
        return None;
    }
    let number: u64 = if ordered {
        t[..marker_len - 1].parse().unwrap_or(1)
    } else {
        0
    };
    Some(ItemMarker {
        indent,
        ordered,
        number,
        content_col_byte: indent + marker_len + spaces.min(999),
    })
}

fn parse_blocks(lines: &[String]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = &lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(f) = fence_open(line) {
            let ind = line.len() - line.trim_start().len();
            let mut body: Vec<String> = Vec::new();
            i += 1;
            while i < lines.len() && !fence_close(&lines[i], f) {
                body.push(dedent_line(&lines[i], ind));
                i += 1;
            }
            if i < lines.len() {
                i += 1; // consume closing fence
            }
            let lang = f
                .info
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            blocks.push(Block::Code { lang, body });
            continue;
        }
        if let Some((level, src)) = atx_heading(line) {
            blocks.push(Block::Heading(level, src));
            i += 1;
            continue;
        }
        if is_hr(line) {
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }
        let t = line.trim_start();
        if t.starts_with('>') {
            let mut inner: Vec<String> = Vec::new();
            while i < lines.len() {
                match lines[i].trim_start().strip_prefix('>') {
                    Some(rest) => {
                        inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                        i += 1;
                    }
                    None => break,
                }
            }
            blocks.push(Block::Quote(parse_blocks(&inner)));
            continue;
        }
        if item_marker(line).is_some() {
            let (list, consumed) = parse_list(lines, i);
            blocks.push(Block::List(list));
            i = consumed;
            continue;
        }
        // Paragraph: gather until blank line or an interrupting block.
        let mut para: Vec<String> = vec![line.clone()];
        i += 1;
        while i < lines.len() {
            let l = &lines[i];
            if l.trim().is_empty()
                || fence_open(l).is_some()
                || atx_heading(l).is_some()
                || is_hr(l)
                || l.trim_start().starts_with('>')
                || item_marker(l).map(|m| m.indent <= 3).unwrap_or(false)
            {
                break;
            }
            para.push(l.clone());
            i += 1;
        }
        blocks.push(Block::Para(para));
    }
    blocks
}

fn parse_list(lines: &[String], start: usize) -> (ListBlock, usize) {
    let first = item_marker(&lines[start]).unwrap();
    let ordered = first.ordered;
    let num = first.number;
    let base = first.indent;
    let content_off = base + m_min_content();
    let mut items: Vec<Vec<String>> = Vec::new();
    let mut loose = false;
    let mut i = start;
    let mut pending_blank = false;

    while i < lines.len() {
        let line = &lines[i];
        if line.trim().is_empty() {
            pending_blank = true;
            i += 1;
            continue;
        }
        if let Some(m) = item_marker(line) {
            if m.indent <= base {
                if pending_blank && !items.is_empty() {
                    loose = true;
                }
                pending_blank = false;
                items.push(vec![take_item_content(line, &m)]);
                i += 1;
                continue;
            }
            // Deeper-indented marker: continuation of the current item
            // (nested list content), dedented for recursive parsing.
            if pending_blank {
                loose = true;
                pending_blank = false;
            }
            if let Some(cur) = items.last_mut() {
                cur.push(dedent_line(line, content_off));
            } else {
                items.push(vec![take_item_content(line, &m)]);
            }
            i += 1;
            continue;
        }
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let block_start = fence_open(line).is_some()
            || atx_heading(line).is_some()
            || is_hr(line)
            || line.trim_start().starts_with('>');
        if items.is_empty() {
            break;
        }
        if indent <= base && (block_start || !continues_paragraph(items.last())) {
            break;
        }
        // Unindented text right after an item line lazily continues the
        // item's paragraph (CommonMark lazy continuation).
        if pending_blank {
            loose = true;
            pending_blank = false;
        }
        if let Some(cur) = items.last_mut() {
            cur.push(dedent_line(line, content_off));
        }
        i += 1;
    }

    let parsed_items: Vec<Vec<Block>> = items.into_iter().map(|ls| parse_blocks(&ls)).collect();

    (
        ListBlock {
            ordered,
            start: num,
            items: parsed_items,
            loose,
        },
        i,
    )
}

/// Two-column hang per nesting level (glamour LevelIndent).
const fn m_min_content() -> usize {
    2
}

/// True when the item's last content line keeps a paragraph open, so an
/// unindented follow-on line lazily continues it.
fn continues_paragraph(item: Option<&Vec<String>>) -> bool {
    match item {
        Some(lines) => lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false),
        None => false,
    }
}

fn take_item_content(line: &str, m: &ItemMarker) -> String {
    let b = line.as_bytes();
    let mut idx = m.content_col_byte.min(b.len());
    // content_col_byte may land mid-multibyte; walk to a boundary.
    while idx < b.len() && (b[idx] & 0xC0) == 0x80 {
        idx += 1;
    }
    line[idx..].to_string()
}

fn dedent_line(line: &str, cols: usize) -> String {
    let mut removed = 0usize;
    let mut idx = 0usize;
    for c in line.chars() {
        if removed >= cols {
            break;
        }
        if c == ' ' {
            removed += 1;
            idx += 1;
        } else if c == '\t' {
            removed += 4;
            idx += 1;
        } else {
            break;
        }
    }
    line[idx..].to_string()
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

fn render_blocks(blocks: &[Block], width: usize) -> String {
    let outs: Vec<String> = blocks
        .iter()
        .filter_map(|b| render_block(b, width))
        .collect();
    outs.join("\n\n")
}

fn render_block(b: &Block, width: usize) -> Option<String> {
    match b {
        Block::Para(lines) => {
            let src = join_paragraph(lines);
            Some(wrap_colored(&render_inlines(&src), width, COLOR_FOREGROUND))
        }
        Block::Heading(level, src) => {
            let color = if *level >= 6 {
                COLOR_MUTED
            } else {
                COLOR_FOREGROUND
            };
            let inline = render_inlines(&join_paragraph(src));
            let styled = format!("{}{}{}{}", ansi_fg(color), BOLD, inline.trim(), RESET);
            Some(wrap_colored(&styled, width, COLOR_FOREGROUND))
        }
        Block::Code { lang, body } => {
            let mut code = body.join("\n");
            code.push('\n');
            Some(highlight_code(&code, lang, width))
        }
        Block::Quote(inner) => {
            let body = render_blocks(inner, width);
            if body.is_empty() {
                None
            } else {
                Some(body)
            }
        }
        Block::List(list) => Some(render_list(list, width)),
        Block::Rule => Some(format!("{}--------{}", ansi_fg(COLOR_MUTED), RESET)),
    }
}

fn render_list(list: &ListBlock, width: usize) -> String {
    let inner_w = width.saturating_sub(2).max(1);
    let mut n = list.start;
    let parts: Vec<String> = list
        .items
        .iter()
        .map(|blocks| {
            let marker = if list.ordered {
                format!("{}. ", n)
            } else {
                "• ".to_string()
            };
            n += 1;
            let body = render_blocks(blocks, inner_w);
            if body.is_empty() {
                return marker.trim_end().to_string();
            }
            let mut lines = body.split('\n');
            let first = lines.next().unwrap_or("");
            let mut out = vec![format!("{}{}", marker, first)];
            for l in lines {
                out.push(format!("  {}", l));
            }
            out.join("\n")
        })
        .collect();
    parts.join(if list.loose { "\n\n" } else { "\n" })
}

/// Joins paragraph source lines, turning soft wraps into spaces and
/// keeping hard breaks (two trailing spaces or trailing backslash).
fn join_paragraph(lines: &[String]) -> String {
    if lines.len() == 1 {
        return lines[0].clone();
    }
    let mut src = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            let prev = &lines[i - 1];
            if prev.ends_with("  ") || prev.ends_with('\\') {
                src.push('\n');
            } else {
                src.push(' ');
            }
        }
        src.push_str(l.trim_end());
    }
    src
}

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
// Inline rendering.
// ---------------------------------------------------------------------------

/// Renders one line's worth of inline markdown (code spans, emphasis,
/// links, images, autolinks, escapes) with atom styling.
pub fn render_inlines(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                // `\(…\)` is inline math, same treatment as `$…$`.
                let close = if chars[i + 1] == '(' {
                    find_sub(&chars, i + 2, &['\\', ')'])
                } else {
                    None
                };
                if let Some(close) = close {
                    let src: String = chars[i + 2..close].iter().collect();
                    out.push_str(&math_inline::styled(&src));
                    i = close + 2;
                } else {
                    out.push(chars[i + 1]);
                    i += 2;
                }
            }
            '`' => {
                let n = run_len(&chars, i, '`');
                if let Some(close) = find_run(&chars, i + n, '`', n) {
                    let content: String = chars[i + n..close].iter().collect();
                    let content = strip_code_pad(&content);
                    out.push_str(&format!(
                        "{}{}{}",
                        ansi_fg(COLOR_SYNTAX_STRING),
                        content,
                        RESET
                    ));
                    i = close + n;
                } else {
                    out.push_str(&"`".repeat(n));
                    i += n;
                }
            }
            '*' | '_' => {
                let n = run_len(&chars, i, c);
                let prev_ok = i == 0 || !chars[i - 1].is_alphanumeric();
                if c == '_' && !prev_ok {
                    out.push_str(&c.to_string().repeat(n));
                    i += n;
                    continue;
                }
                let (used, rendered) = try_emphasis(&chars, i, c, n);
                if used > 0 {
                    out.push_str(&rendered);
                    i += used;
                } else {
                    out.push_str(&c.to_string().repeat(n));
                    i += n;
                }
            }
            '~' if run_len(&chars, i, '~') >= 2 => {
                if let Some(close) = find_sub(&chars, i + 2, &['~', '~']) {
                    let inner = render_inlines(&chars[i + 2..close].iter().collect::<String>());
                    out.push_str(&format!("{}{}{}", CROSSED_OUT, inner, RESET));
                    i = close + 2;
                } else {
                    out.push_str("~~");
                    i += 2;
                }
            }
            '[' => match parse_link(&chars, i, false) {
                Some((end, label_src, url)) => {
                    let body = if label_src.trim().is_empty() {
                        url.clone()
                    } else {
                        render_inlines(&label_src)
                    };
                    // OSC 8 wrapper so terminals (and the TUI click
                    // handler) can open the target; the label body keeps
                    // its nested inline styling.
                    out.push_str(&format!(
                        "{}{}{}{}{}{}",
                        osc8_open(&url),
                        ansi_fg(COLOR_SECONDARY),
                        UNDERLINE,
                        body,
                        RESET,
                        osc8_close()
                    ));
                    i = end;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            '!' if chars.get(i + 1) == Some(&'[') => match parse_link(&chars, i + 1, true) {
                Some((end, label_src, _url)) => {
                    out.push_str(&format!(
                        "{}{}{}",
                        ansi_fg(COLOR_MUTED),
                        label_src.trim(),
                        RESET
                    ));
                    i = end;
                }
                None => {
                    out.push('!');
                    i += 1;
                }
            },
            '<' => {
                if let Some(end) = autolink_end(&chars, i) {
                    let url: String = chars[i + 1..end].iter().collect();
                    out.push_str(&format!(
                        "{}{}{}{}{}{}",
                        osc8_open(&url),
                        ansi_fg(COLOR_SECONDARY),
                        UNDERLINE,
                        url,
                        RESET,
                        osc8_close()
                    ));
                    i = end + 1;
                } else {
                    out.push('<');
                    i += 1;
                }
            }
            '$' => match math_inline::inline_math_span(&chars, i) {
                Some((end, rendered)) => {
                    out.push_str(&rendered);
                    i = end;
                }
                None => {
                    // Not math (currency, stray dollar): pass through.
                    out.push('$');
                    i += 1;
                }
            },
            '\n' => {
                out.push('\n');
                i += 1;
            }
            _ => {
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
    if pat.is_empty() || chars.len() < pat.len() {
        return None;
    }
    (from..=chars.len() - pat.len()).find(|&i| chars[i..i + pat.len()] == *pat)
}

fn strip_code_pad(content: &str) -> String {
    if content.len() >= 2
        && content.starts_with(' ')
        && content.ends_with(' ')
        && content.trim() == content.trim_matches(' ')
        && !content.trim().is_empty()
    {
        content[1..content.len() - 1].to_string()
    } else {
        content.to_string()
    }
}

/// Attempts emphasis at `i`; returns (bytes consumed, rendered span).
fn try_emphasis(chars: &[char], i: usize, c: char, n: usize) -> (usize, String) {
    if n >= 2 {
        if let Some(close) = find_sub(chars, i + 2, &[c, c]) {
            let inner = render_inlines(&chars[i + 2..close].iter().collect::<String>());
            return (
                close + 2 - i,
                format!("{}{}{}{}", ansi_fg(COLOR_FOREGROUND), BOLD, inner, RESET),
            );
        }
    }
    if let Some(close) = find_single_closer(chars, i + 1, c) {
        let inner = render_inlines(&chars[i + 1..close].iter().collect::<String>());
        (
            close + 1 - i,
            format!("{}{}{}{}", ansi_fg(COLOR_FOREGROUND), ITALIC, inner, RESET),
        )
    } else {
        (0, String::new())
    }
}

fn find_single_closer(chars: &[char], from: usize, c: char) -> Option<usize> {
    for j in from..chars.len() {
        if chars[j] == c {
            let next_ok = j + 1 >= chars.len() || !chars[j + 1].is_alphanumeric();
            if next_ok {
                return Some(j);
            }
        }
    }
    None
}

/// Parses [label](url) starting at the '[' (or image starting at the '[').
/// Returns (index just past ')', label source, url).
fn parse_link(chars: &[char], open: usize, _img: bool) -> Option<(usize, String, String)> {
    let mut depth = 0usize;
    let mut close = None;
    let mut j = open;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    let close = close?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let mut pdepth = 0usize;
    let mut k = close + 2;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '(' => pdepth += 1,
            ')' => {
                if pdepth == 0 {
                    break;
                }
                pdepth -= 1;
            }
            _ => {}
        }
        k += 1;
    }
    if k >= chars.len() {
        return None;
    }
    let label: String = chars[open + 1..close].iter().collect();
    let mut url: String = chars[close + 2..k].iter().collect();
    // Drop an optional "title" suffix.
    if let Some(pos) = url.find(" \"") {
        url.truncate(pos);
    }
    Some((k + 1, label, url.trim().to_string()))
}

fn autolink_end(chars: &[char], open: usize) -> Option<usize> {
    let head: String = chars
        .iter()
        .skip(open + 1)
        .take(8)
        .collect::<String>()
        .to_lowercase();
    if !(head.starts_with("http://") || head.starts_with("https://")) {
        return None;
    }
    (open + 1..chars.len()).find(|&j| chars[j] == '>')
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
}
