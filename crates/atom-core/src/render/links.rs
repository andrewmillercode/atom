//! Ported from links.go: detects URLs and filesystem paths in
//! conversation text and renders them as secondary-colored OSC 8
//! hyperlinks so a supporting terminal can open them (usually click or
//! cmd-click). Also carries a faithful port of charmbracelet/x/ansi.Wrap,
//! the ANSI-aware word wrapper used across the renderer.

use once_cell::sync::Lazy;
use regex::Regex;
use unicode_width::UnicodeWidthChar;

use super::colors::{ansi_bg, ansi_fg, COLOR_SECONDARY};

/// linkRe matches http(s) and file URLs, home-relative paths, and
/// absolute Unix paths with at least two segments (so /thinking and
/// other slash-commands stay plain text). A backticked or double-quoted
/// span that starts with a path prefix links in full, so paths
/// containing spaces (`~/Library/Application Support/...` or
/// "/Users/me/My Docs/...") aren't truncated at the space.
static LINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
    r#"(?i)`(?:~/|/)[^`\n]+`|"(?:~/|/)[^"\n]+"|\bhttps?://[^\s<>\[\]"'`]+|file://[^\s<>\[\]"'`]+|~/(?:[A-Za-z0-9._+-]+/)*[A-Za-z0-9._+-]+|/(?:[A-Za-z0-9._+-]+/){1,}[A-Za-z0-9._+-]+"#,
).unwrap()
});

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

/// wrapLinked wraps text to width after turning detected links into
/// styled hyperlinks. restoreFg/restoreBg are theme hex colors written
/// after each link so a parent renderer's colors survive; empty
/// restoreFg returns to the default foreground.
pub fn wrap_linked(text: &str, width: usize, restore_fg: &str, restore_bg: &str) -> String {
    ansi_wrap(&linkify(text, restore_fg, restore_bg), width)
}

pub fn linkify(text: &str, restore_fg: &str, restore_bg: &str) -> String {
    if !LINK_RE.is_match(text) {
        return text.to_string();
    }
    let mut sb = String::with_capacity(text.len() + 64);
    let mut last = 0usize;
    for m in LINK_RE.find_iter(text) {
        let raw = m.as_str();
        // Backticked or double-quoted paths: the delimiter characters
        // stay as plain text and the path inside — spaces included —
        // becomes the hyperlink.
        if let Some(inner) = raw
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .or_else(|| raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        {
            let delim = raw.chars().next().expect("delimited match is non-empty");
            sb.push_str(&text[last..m.start()]);
            sb.push(delim);
            sb.push_str(&render_link(
                inner,
                &link_uri(inner),
                restore_fg,
                restore_bg,
            ));
            sb.push(delim);
            last = m.end();
            continue;
        }
        let display = trim_link(raw);
        if display.is_empty() {
            continue;
        }
        sb.push_str(&text[last..m.start()]);
        sb.push_str(&render_link(
            display,
            &link_uri(display),
            restore_fg,
            restore_bg,
        ));
        last = m.start() + display.len();
    }
    sb.push_str(&text[last..]);
    sb
}

/// linkifyPath turns a filesystem path into an OSC 8 file:// hyperlink,
/// including relative paths like tui.go or src/main.go. Display text stays
/// as given; the URI is the absolute path.
pub fn linkify_path(display: &str, restore_fg: &str, restore_bg: &str) -> String {
    render_link(display, &path_file_uri(display), restore_fg, restore_bg)
}

pub fn path_file_uri(display: &str) -> String {
    let mut p = display.to_string();
    if p.len() >= 7 && p[..7].eq_ignore_ascii_case("file://") {
        return p;
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            p = format!("{}/{}", home.display(), rest);
        }
    }
    p = lexical_abs(&p);
    file_uri(&p)
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

pub fn trim_link(s: &str) -> &str {
    let mut s = s.trim_end_matches(['.', ',', ';', ':', '!', '?', ']', '}', '\'', '"']);
    while s.ends_with(')') && s.matches('(').count() < s.matches(')').count() {
        s = &s[..s.len() - 1];
    }
    s
}

pub fn link_uri(display: &str) -> String {
    let lower_prefix =
        |p: &str| display.len() >= p.len() && display[..p.len()].eq_ignore_ascii_case(p);
    if lower_prefix("http://") || lower_prefix("https://") || lower_prefix("file://") {
        display.to_string()
    } else if let Some(rest) = display.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => file_uri(&format!("{}/{}", home.display(), rest)),
            None => file_uri(display),
        }
    } else {
        file_uri(display)
    }
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
    match ch {
        '-' | '_' | '.' | '~' => false,
        '$' | '&' | '+' | ',' | '/' | ':' | ';' | '=' | '@' => false,
        _ => true,
    }
}

/// lexical_abs stands in for Go's filepath.Abs: pure lexical join with
/// the process cwd plus dot/dotdot cleanup, no filesystem access.
fn lexical_abs(p: &str) -> String {
    let joined = if p.starts_with('/') {
        p.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| "/".to_string());
        format!("{}/{}", cwd.trim_end_matches('/'), p)
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
    format!("/{}", parts.join("/"))
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
    use crate::render::colors::{
        ansi_bg, ansi_fg, COLOR_CARD_DARK, COLOR_FOREGROUND, COLOR_SECONDARY,
    };

    #[test]
    fn slash_commands_stay_plain() {
        let in_text = "run /thinking or /compact now";
        assert_eq!(linkify(in_text, "", ""), in_text);
    }

    #[test]
    fn linkify_styles_and_hyperlinks() {
        let in_text =
            "see https://ness-health.com and /Users/andrewmiller/.config/atom/AGENTS.md please";
        let out = linkify(in_text, COLOR_FOREGROUND, "");
        assert!(out.contains(&osc8_open("https://ness-health.com")));
        assert!(out.contains(&osc8_open(&file_uri(
            "/Users/andrewmiller/.config/atom/AGENTS.md"
        ))));
        assert!(out.contains(&ansi_fg(COLOR_SECONDARY)));
        assert!(out.contains(ANSI_UNDERLINE));
        assert_eq!(
            visible_width(&out),
            visible_width(in_text),
            "styled width must equal plain width"
        );
    }

    #[test]
    fn trim_link_punct_and_quotes() {
        let out = linkify("try 'https://ness-health.com'.", "", "");
        assert!(
            out.contains(&osc8_open("https://ness-health.com")),
            "{}",
            out
        );
        assert!(!out.contains(&osc8_open("https://ness-health.com'.")));
    }

    #[test]
    fn linkify_home_path_keeps_display() {
        let home = dirs::home_dir().unwrap().display().to_string();
        let out = linkify("open ~/.config/atom/AGENTS.md", "", "");
        assert!(out.contains(&osc8_open(&file_uri(&format!(
            "{}/.config/atom/AGENTS.md",
            home
        )))));
        assert!(out.contains("~/.config/atom/AGENTS.md"));
    }

    #[test]
    fn linkify_backticked_path_with_space() {
        let in_text = "`~/Library/Application Support/atom/diagrams/`";
        let home = dirs::home_dir().unwrap().display().to_string();
        let out = linkify(in_text, "", "");
        // The full path — space included — becomes the URI, escaped.
        assert!(out.contains(&osc8_open(&file_uri(&format!(
            "{home}/Library/Application Support/atom/diagrams/"
        )))));
        // Backticks remain visible and no text is lost.
        assert_eq!(visible_width(&out), visible_width(in_text));
        // Absolute backticked paths behave the same.
        let out = linkify("`/Users/a/My Docs/x.md`", "", "");
        assert!(out.contains(&osc8_open(&file_uri("/Users/a/My Docs/x.md"))));
    }

    #[test]
    fn linkify_double_quoted_path_with_space() {
        // The visualize tool marker quotes artifact paths; a
        // space-containing path must not truncate at the space.
        let in_text = "png=\"/Users/a/My Docs/x.png\" html=\"/Users/a/My Docs/x.html\"";
        let out = linkify(in_text, "", "");
        assert!(out.contains(&osc8_open(&file_uri("/Users/a/My Docs/x.png"))));
        assert!(out.contains(&osc8_open(&file_uri("/Users/a/My Docs/x.html"))));
        // The full display text survives (no truncation at the space).
        assert!(out.contains("/Users/a/My Docs/x.png"));
        assert!(out.contains("/Users/a/My Docs/x.html"));
        // The double quotes stay visible as plain text.
        assert!(out.contains("png=\""));
        assert!(out.contains("\" html=\""));
        assert!(out.ends_with('"'));
        assert_eq!(visible_width(&out), visible_width(in_text));
    }

    #[test]
    fn linkify_path_relative_makes_absolute_uri() {
        for rel in ["tui.go", "src/main.go"] {
            let out = linkify_path(rel, COLOR_FOREGROUND, COLOR_CARD_DARK);
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

    #[test]
    fn file_uri_escapes_like_go_url() {
        assert_eq!(file_uri("/a b/c.md"), "file:///a%20b/c.md");
        assert_eq!(file_uri("/plain/path.txt"), "file:///plain/path.txt");
    }

    /// Golden captured from charmbracelet/x/ansi v0.11.8 via the real
    /// renderLink + Wrap pipeline in the Go build.
    #[test]
    fn wrap_linked_golden_matches_go() {
        let url = "https://ness-health.com/a/very/long/path/that/will/wrap";
        let got = wrap_linked(&format!("prefix {url} suffix"), 20, COLOR_FOREGROUND, "");
        let want = "prefix \x1b]8;;https://ness-health.com/a/very/long/path/that/will/wrap\x07\
\x1b[38;2;180;145;176m\x1b[4mhttps://ness-\nhealth.com/a/very/lo\nng/path/that/will/wr\nap\
\x1b[24m\x1b[38;2;222;227;232m\x1b]8;;\x07 suffix";
        assert_eq!(got, want, "\ngot:  {:?}\nwant: {:?}", got, want);
    }

    #[test]
    fn wrap_breaks_at_hyphens_and_words() {
        assert_eq!(ansi_wrap("aa-bbbb-cc", 6), "aa-\nbbbb-\ncc");
        assert_eq!(ansi_wrap("ab cdefgh", 4), "ab\ncdef\ngh");
        assert_eq!(ansi_wrap("hello", 10), "hello");
        assert_eq!(ansi_wrap("", 5), "");
    }
}
