//! Renders markdown-style `[label](url)` and `<https://…>` autolinks as
//! OSC 8 hyperlinks (handled structurally by the markdown layer in
//! `render::markdown`), plus an explicit `linkify_path` helper for
//! clickable file paths in tool headers. Also carries a faithful port
//! of charmbracelet/x/ansi.Wrap, the ANSI-aware word wrapper used
//! across the renderer.
//!
//! Prose-level URL/path detection has been removed: bare URLs in
//! prose, `~/path`, `/abs/path`, repo-relative `crates/foo.rs`, and
//! backticked/quoted paths are intentionally **not** turned into
//! clickable links. The only paths to an OSC 8 region are markdown
//! syntax (`[label](url)`, `<url>`) and explicit per-element helpers
//! like `linkify_path`.

use unicode_width::UnicodeWidthChar;

use super::colors::{ansi_bg, ansi_fg, COLOR_SECONDARY};

/// split_line_anchor peels a trailing `:N[-M|,M]` off a path-shaped
/// match so the URI builder can preserve it. Returns (path, Some(line))
/// when present, else (raw, None). The path part is the display text
/// the user sees; the line part goes into the OSC 8 URI as
/// `?line=N` (or `?line=N-M`, `?line=N,M`).
pub(crate) fn split_line_anchor(raw: &str) -> (&str, Option<&str>) {
    let bytes = raw.as_bytes();
    let mut i = raw.len();
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'-' || b == b',' {
            continue;
        }
        if b == b':' {
            // Reject Windows drive letters (`C:\` / `C:/` / `C:foo`):
            // the byte immediately before is an ASCII letter AND
            // there's no `/` or `\` between it and the start of the
            // raw match.
            if i > 0 && bytes[i - 1].is_ascii_alphabetic() && !raw[..i].contains(['/', '\\']) {
                break;
            }
            // Must be followed by a digit to count as an anchor.
            if !bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
                break;
            }
            return (&raw[..i], Some(&raw[i + 1..]));
        }
        // Anything else (`.`, `_`, `)`, `]`, `;`, …) ends the scan.
        break;
    }
    (raw, None)
}

pub const ANSI_UNDERLINE: &str = "\x1b[4m";
pub const ANSI_NO_UNDERLINE: &str = "\x1b[24m";
pub const ANSI_DEFAULT_FG: &str = "\x1b[39m";

/// SetHyperlink opens an OSC 8 hyperlink (charmbracelet/x/ansi shape:
/// params empty, BEL terminator).
pub fn osc8_open(uri: &str) -> String {
    format!("\x1b]8;;{uri}\x07")
}

/// ResetHyperlink closes the current OSC 8 hyperlink.
pub fn osc8_close() -> &'static str {
    "\x1b]8;;\x07"
}

/// wrapLinked wraps text to `width` cells with escape sequences
/// preserved verbatim and wide characters counted correctly. The
/// optional `restore_fg`/`restore_bg` are prepended as SGR and re-
/// emitted at the end so a parent renderer's colors survive; empty
/// `restore_fg` falls back to the default foreground.
///
/// No hyperlink detection happens here — markdown `[label](url)`
/// and `<url>` autolinks are produced by the markdown layer, and
/// explicit per-element paths go through `linkify_path`. Plain text
/// round-trips unchanged (modulo wrap + color reset).
pub fn wrap_linked(text: &str, width: usize, restore_fg: &str, restore_bg: &str) -> String {
    let mut sb = String::with_capacity(text.len() + 16);
    if !restore_bg.is_empty() {
        sb.push_str(&ansi_bg(restore_bg));
    }
    if !restore_fg.is_empty() {
        sb.push_str(&ansi_fg(restore_fg));
    }
    sb.push_str(&ansi_wrap(text, width));
    if !restore_bg.is_empty() {
        sb.push_str(&ansi_bg(restore_bg));
    }
    if !restore_fg.is_empty() {
        sb.push_str(&ansi_fg(restore_fg));
    } else {
        sb.push_str(ANSI_DEFAULT_FG);
    }
    sb
}

/// linkifyPath turns a filesystem path into an OSC 8 file:// hyperlink,
/// including relative paths like tui.go or src/main.go. Display text stays
/// as given; the URI is the absolute path. A trailing `:N[-M|,M]` line
/// anchor is preserved in the URI as `?line=N`.
///
/// `cwd` is the base for resolving relative paths; empty falls back to
/// the process cwd.
pub fn linkify_path(display: &str, restore_fg: &str, restore_bg: &str, cwd: &str) -> String {
    render_link(
        display,
        &path_file_uri(display, cwd),
        restore_fg,
        restore_bg,
    )
}

pub fn path_file_uri(display: &str, cwd: &str) -> String {
    let (path, line) = split_line_anchor(display);
    let mut p = path.to_string();
    // get(..7) instead of [..7]: a leading multi-byte rune can make byte 7
    // a non-boundary; the ASCII check then simply fails, as it should.
    if p.get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("file://"))
    {
        return append_line_query(p, line);
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            p = format!("{}/{}", home.display(), rest);
        }
    }
    let mut uri = file_uri(&lexical_abs(&p, cwd));
    if let Some(line) = line {
        uri.push('?');
        uri.push_str("line=");
        uri.push_str(line);
    }
    uri
}

/// append_line_query tags `?line=N[-M|,M]` onto a `file://` URI when
/// the display text had a line anchor. Already-queried URIs use `;`
/// to keep the existing query intact.
fn append_line_query(mut uri: String, line: Option<&str>) -> String {
    if let Some(line) = line {
        if uri.contains('?') {
            uri.push_str(";line=");
        } else {
            uri.push('?');
            uri.push_str("line=");
        }
        uri.push_str(line);
    }
    uri
}

pub fn render_link(display: &str, uri: &str, restore_fg: &str, restore_bg: &str) -> String {
    let mut sb = String::new();
    sb.push_str(&osc8_open(uri));
    sb.push_str(&ansi_fg(COLOR_SECONDARY));
    sb.push_str(ANSI_UNDERLINE);
    sb.push_str(display);
    sb.push_str(ANSI_NO_UNDERLINE);
    if !restore_bg.is_empty() {
        sb.push_str(&ansi_bg(restore_bg));
    }
    if !restore_fg.is_empty() {
        sb.push_str(&ansi_fg(restore_fg));
    } else {
        sb.push_str(ANSI_DEFAULT_FG);
    }
    sb.push_str(osc8_close());
    sb
}

/// fileURI mirrors Go's (&url.URL{Scheme:"file", Path:path}).String():
/// percent-escape everything net/url escapes in encodePath mode.
pub fn file_uri(path: &str) -> String {
    format!("file://{}", escaped_path(path))
}

fn escaped_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut buf = [0u8; 4];
    for ch in path.chars() {
        if should_escape_path(ch) {
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn should_escape_path(ch: char) -> bool {
    if ch.is_ascii_alphanumeric() {
        return false;
    }
    !matches!(
        ch,
        '-' | '_' | '.' | '~' | '$' | '&' | '+' | ',' | '/' | ':' | ';' | '=' | '@'
    )
}

/// lexical_abs stands in for Go's filepath.Abs: pure lexical join with
/// `cwd` plus dot/dotdot cleanup, no filesystem access. An empty
/// `cwd` falls back to the process cwd so old single-argument
/// callers (e.g. tests that don't care about resolution) still work.
fn lexical_abs(p: &str, cwd: &str) -> String {
    let joined = if p.starts_with('/') {
        p.to_string()
    } else {
        let base = if cwd.is_empty() {
            std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "/".to_string())
        } else {
            cwd.to_string()
        };
        format!("{}/{}", base.trim_end_matches('/'), p)
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in joined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    // A trailing slash is a directory marker (e.g. "`~/Library/.../`");
    // lexical normalization must not eat it or the file:// URI silently
    // points at the parent path. Root "/" stays exactly "/".
    let trailing_slash = joined.ends_with('/') && joined.len() > 1;
    let mut out = format!("/{}", parts.join("/"));
    if trailing_slash && out.len() > 1 {
        out.push('/');
    }
    out
}

// ---------------------------------------------------------------------------
// ANSI-aware wrapping: faithful port of charmbracelet/x/ansi Wrap
// (word-wrapping with hard breaks, escape sequences kept verbatim and
// zero-width, '-' always a breakpoint). Like the original, SGR/hyperlink
// state is NOT re-emitted on continuation lines; open sequences simply
// stay active across the inserted newline.
// ---------------------------------------------------------------------------

/// A printable character or escape sequence, as scanned out of styled text.
#[derive(Debug, Clone, Copy)]
pub enum Tok<'a> {
    /// Escape sequence copied verbatim (CSI/OSC/2-char), zero width.
    Esc(&'a str),
    Ch(char),
}

/// Splits s into zero-width escape sequences and printable characters.
pub fn tokens(s: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                let start = i;
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    // CSI: parameter/intermediate bytes then final @-~
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    // OSC terminated by BEL or ST.
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            break;
                        }
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += if bytes[i] == 0x07 { 1 } else { 2 };
                    }
                } else if i + 1 < bytes.len() {
                    i += 2; // two-byte escape
                } else {
                    i += 1;
                }
                toks.push(Tok::Esc(&s[start..i]));
            }
            _ => {
                let ch = s[i..].chars().next().unwrap();
                i += ch.len_utf8();
                toks.push(Tok::Ch(ch));
            }
        }
    }
    toks
}

/// Cell width of a character the way the wrapper counts it: controls are
/// zero, tabs count as 4, everything else via unicode-width.
pub fn char_width(ch: char) -> usize {
    match ch {
        '\n' | '\r' => 0,
        c if c.is_control() => 0,
        c => UnicodeWidthChar::width(c).unwrap_or(0),
    }
}

/// Visible cell width of s, ignoring ANSI escape sequences (lipgloss.Width).
pub fn visible_width(s: &str) -> usize {
    tokens(s)
        .into_iter()
        .map(|t| match t {
            Tok::Ch(c) => char_width(c),
            Tok::Esc(_) => 0,
        })
        .sum()
}

/// ansi_wrap wraps s or a block of text to limit cells, breaking word
/// boundaries if necessary, preserving ANSI escape codes and accounting
/// for wide characters. A hyphen (-) is always considered a breakpoint.
pub fn ansi_wrap(s: &str, limit: usize) -> String {
    if limit < 1 {
        return s.to_string();
    }

    let mut st = WrapState {
        out: String::with_capacity(s.len() + 16),
        ..Default::default()
    };

    for tok in tokens(s) {
        match tok {
            Tok::Esc(_) => {
                st.word.push(tok);
            }
            Tok::Ch('\n') => {
                if st.word_len == 0 {
                    if st.col + st.space_w > limit {
                        st.col = 0;
                    } else {
                        st.out.push_str(&st.space);
                    }
                    st.space.clear();
                    st.space_w = 0;
                }
                flush_word(&mut st, limit);
                st.out.push('\n');
                st.col = 0;
                st.space.clear();
                st.space_w = 0;
            }
            Tok::Ch(' ') => {
                flush_word(&mut st, limit);
                st.space.push(' ');
                st.space_w += 1;
            }
            Tok::Ch('-') => {
                if !(st.space.is_empty() && st.space_w == 0) {
                    st.out.push_str(&st.space);
                    st.col += st.space_w;
                    st.space.clear();
                    st.space_w = 0;
                }
                if st.col + st.word_len >= limit {
                    st.word.push(Tok::Ch('-'));
                    st.word_len += 1;
                } else {
                    flush_word(&mut st, limit);
                    st.out.push('-');
                    st.col += 1;
                }
            }
            Tok::Ch(c) => {
                if st.col == limit {
                    st.out.push('\n');
                    st.col = 0;
                    st.space.clear();
                    st.space_w = 0;
                }
                st.word.push(Tok::Ch(c));
                st.word_len += char_width(c);
                if st.word_len == limit {
                    flush_word(&mut st, limit);
                }
                if st.col + st.word_len + st.space_w > limit {
                    st.out.push('\n');
                    st.col = 0;
                    st.space.clear();
                    st.space_w = 0;
                }
            }
        }
    }

    if st.word_len == 0 {
        if st.col + st.space_w > limit {
            st.col = 0;
        } else {
            st.out.push_str(&st.space);
        }
        st.space.clear();
        st.space_w = 0;
    }
    flush_word(&mut st, limit);
    st.out
}

#[derive(Default)]
struct WrapState<'a> {
    out: String,
    word: Vec<Tok<'a>>,
    word_len: usize,
    space: String,
    space_w: usize,
    col: usize,
}

fn flush_word(st: &mut WrapState<'_>, _limit: usize) {
    if st.word.is_empty() {
        return;
    }
    if !(st.space.is_empty() && st.space_w == 0) {
        st.out.push_str(&st.space);
        st.col += st.space_w;
        st.space.clear();
        st.space_w = 0;
    }
    for tok in &st.word {
        match *tok {
            Tok::Esc(seq) => st.out.push_str(seq),
            Tok::Ch(c) => st.out.push(c),
        }
    }
    st.col += st.word_len;
    st.word.clear();
    st.word_len = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::colors::{ansi_fg, COLOR_CARD_DARK, COLOR_FOREGROUND};

    /// Plain prose must round-trip without gaining OSC 8 wrappers,
    /// even when it looks like a path or URL. The policy is that
    /// only markdown `[label](url)` syntax and explicit
    /// `linkify_path` produce clickable regions.
    #[test]
    fn wrap_linked_emits_no_osc8_for_plain_text() {
        for in_text in [
            "say yes/no/maybe",
            "and/or logic",
            "visit https://example.com today",
            "see crates/foo.rs for details",
            "open ~/.config/atom/AGENTS.md",
            "try /thinking or /compact now",
            "use bar:5 for the config",
        ] {
            let out = wrap_linked(in_text, 80, "", "");
            assert!(
                !out.contains("\x1b]8;;"),
                "wrap_linked({in_text:?}) leaked OSC 8: {out:?}"
            );
            // Visible characters survive; the input is a substring of
            // the visible output (modulo the embedded color resets
            // emitted by wrap_linked with empty fg/bg).
            let visible: String = out
                .split('\x1b')
                .enumerate()
                .filter_map(|(i, chunk)| if i % 2 == 0 { Some(chunk) } else { None })
                .collect();
            assert!(
                visible.contains(in_text),
                "input dropped from visible output: visible={visible:?} in={in_text:?}"
            );
        }
    }

    /// Wrap + color restore must not mangle embedded SGR sequences.
    #[test]
    fn wrap_linked_passes_through_ansi_escapes() {
        let in_text = "foo\x1b[31mred\x1b[0m bar";
        let out = wrap_linked(in_text, 80, "", "");
        assert!(out.contains("\x1b[31m"));
        assert!(out.contains("\x1b[0m"));
        assert!(out.contains("foo"));
        assert!(out.contains("red"));
        assert!(out.contains("bar"));
        // Default-fg reset at the end (empty restore_fg).
        assert!(out.ends_with(ANSI_DEFAULT_FG), "missing fg reset: {out:?}");
    }

    /// restore_fg is reapplied at end so a parent renderer's color
    /// survives the wrap.
    #[test]
    fn wrap_linked_restores_fg_when_provided() {
        let in_text = "hello world";
        let out = wrap_linked(in_text, 80, COLOR_FOREGROUND, "");
        assert!(
            out.starts_with(&ansi_fg(COLOR_FOREGROUND)),
            "missing fg prefix: {out:?}"
        );
        assert!(
            out.ends_with(&ansi_fg(COLOR_FOREGROUND)),
            "missing fg restore at end: {out:?}"
        );
    }

    #[test]
    fn linkify_path_relative_makes_absolute_uri() {
        for rel in ["tui.go", "src/main.go"] {
            let out = linkify_path(rel, COLOR_FOREGROUND, COLOR_CARD_DARK, "");
            let cwd = std::env::current_dir().unwrap();
            let abs = format!("{}/{}", cwd.display(), rel);
            assert!(
                out.contains(&osc8_open(&file_uri(&abs))),
                "{}: {}",
                rel,
                out
            );
            assert!(out.contains(rel), "{} display should stay relative", rel);
        }
    }

    /// Line anchors survive path → file:// URI conversion.
    #[test]
    fn linkify_path_preserves_line_anchor() {
        let out = linkify_path("crates/foo.rs:42", COLOR_FOREGROUND, "", "");
        assert!(
            out.contains("crates/foo.rs:42"),
            "anchor dropped from display: {out:?}"
        );
        let uri = path_file_uri("crates/foo.rs:42", "");
        assert!(uri.contains("?line=42"), "URI missing line query: {uri:?}");
    }

    #[test]
    fn file_uri_escapes_like_go_url() {
        assert_eq!(file_uri("/a b/c.md"), "file:///a%20b/c.md");
        assert_eq!(file_uri("/plain/path.txt"), "file:///plain/path.txt");
    }

    #[test]
    fn wrap_breaks_at_hyphens_and_words() {
        assert_eq!(ansi_wrap("aa-bbbb-cc", 6), "aa-\nbbbb-\ncc");
        assert_eq!(ansi_wrap("ab cdefgh", 4), "ab\ncdef\ngh");
        assert_eq!(ansi_wrap("hello", 10), "hello");
        assert_eq!(ansi_wrap("", 5), "");
    }
}
