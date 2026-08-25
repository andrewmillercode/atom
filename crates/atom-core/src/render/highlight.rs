//! Ported from highlight.go: syntax highlighting. The Go version drives
//! chroma with atomTokenColor; here syntect does the lexing and a small
//! custom theme (built below from the atom palette) reproduces the same
//! token→color decisions: comments muted, keywords/functions primary,
//! builtins/types/tags syntax-type, strings/other literals syntax-string,
//! numbers primary, everything else foreground. Only 24-bit foreground
//! is ever emitted — backgrounds come from the caller's washes.

use std::io::Cursor;
use std::path::Path;

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static ATOM_THEME: Lazy<Theme> = Lazy::new(|| {
    ThemeSet::load_from_reader(&mut Cursor::new(ATOME_TMTHEME)).expect("built-in atom tmTheme")
});

/// atomTokenColor maps a chroma token to an atom theme hex (foreground
/// only). Encoded here as a TextMate theme: chroma categories become
/// scope selectors, resolved by syntect's longest-match scoring.
const ATOME_TMTHEME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>name</key><string>atom</string>
	<key>settings</key>
	<array>
		<dict>
			<key>settings</key>
			<dict>
				<key>foreground</key><string>#DEE3E8</string>
				<key>background</key><string>#00000000</string>
			</dict>
		</dict>
		<dict><key>scope</key><string>comment</string>
			<key>settings</key><dict><key>foreground</key><string>#6B6E77</string></dict></dict>
		<dict><key>scope</key><string>keyword.operator</string>
			<key>settings</key><dict><key>foreground</key><string>#DEE3E8</string></dict></dict>
		<dict><key>scope</key><string>keyword</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>storage.type.function, storage.modifier, keyword.declaration</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>storage.type</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>storage</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>support.function.builtin</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>support.function</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>support</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>entity.name.function</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.interface, entity.name.trait, entity.name.impl</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>entity.name.tag</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>entity.name.exception</string>
			<key>settings</key><dict><key>foreground</key><string>#E8A07A</string></dict></dict>
		<dict><key>scope</key><string>string</string>
			<key>settings</key><dict><key>foreground</key><string>#D8C9B0</string></dict></dict>
		<dict><key>scope</key><string>punctuation.definition.string</string>
			<key>settings</key><dict><key>foreground</key><string>#D8C9B0</string></dict></dict>
		<dict><key>scope</key><string>constant.character.escape</string>
			<key>settings</key><dict><key>foreground</key><string>#D8C9B0</string></dict></dict>
		<dict><key>scope</key><string>constant.numeric</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>constant.language</string>
			<key>settings</key><dict><key>foreground</key><string>#82ACD9</string></dict></dict>
		<dict><key>scope</key><string>constant</string>
			<key>settings</key><dict><key>foreground</key><string>#D8C9B0</string></dict></dict>
	</array>
</dict>
</plist>
"#;

fn find_syntax<'a>(ss: &'a SyntaxSet, filename: &str) -> &'a SyntaxReference {
    let lower = filename.to_lowercase();
    if let Some(s) = ss.find_syntax_by_token(&lower) {
        return s;
    }
    if let Some(ext) = Path::new(filename).extension().and_then(|e| e.to_str()) {
        if let Some(s) = ss.find_syntax_by_extension(&ext.to_lowercase()) {
            return s;
        }
    }
    ss.find_syntax_plain_text()
}

fn style_fg(style: Style) -> String {
    let c = style.foreground;
    use super::colors::{ansi_fg, theme_color, ThemeColor};

    let role = match (c.r, c.g, c.b) {
        (0xde, 0xe3, 0xe8) => Some(ThemeColor::Foreground),
        (0x6b, 0x6e, 0x77) => Some(ThemeColor::Muted),
        (0x82, 0xac, 0xd9) => Some(ThemeColor::Primary),
        (0xe8, 0xa0, 0x7a) => Some(ThemeColor::SyntaxType),
        (0xd8, 0xc9, 0xb0) => Some(ThemeColor::SyntaxString),
        _ => None,
    };
    match role {
        Some(role) => ansi_fg(&theme_color(role)),
        None => format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b),
    }
}

/// highlightDocument syntax-highlights src as a whole file and returns
/// one ANSI-colored line per source line (24-bit foreground only). On
/// any lexing trouble it degrades to plain lines like the Go fallback.
pub fn highlight_document(filename: &str, src: &str) -> Vec<String> {
    if src.is_empty() {
        return Vec::new();
    }
    let ss = &*SYNTAX_SET;
    let syntax = find_syntax(ss, filename);
    let theme = &*ATOM_THEME;
    let mut hl = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(src) {
        let ranges = match hl.highlight_line(line, ss) {
            Ok(r) => r,
            Err(_) => return plain_lines(src),
        };
        // One ANSI-colored output line per source line: concatenate the
        // styled chunks, dropping the line's trailing newline. Like the
        // Go version, colors switch inline with no resets.
        let mut cur = String::new();
        for (style, text) in ranges {
            let text = text.strip_suffix('\n').unwrap_or(text);
            if !text.is_empty() {
                cur.push_str(&style_fg(style));
                cur.push_str(text);
            }
        }
        lines.push(cur);
    }
    lines
}

fn plain_lines(src: &str) -> Vec<String> {
    let mut v: Vec<String> = src.split('\n').map(str::to_string).collect();
    // "x\n" splits to ["x", ""]; the final empty piece is not a line.
    if src.ends_with('\n') {
        v.pop();
    }
    v
}

/// HighlightCode highlights code as a block: one ANSI-colored string,
/// lines hard-wrapped to width when width > 0 (colors are fg-only, so
/// the wrapper can split safely).
pub fn highlight_code(code: &str, lang: &str, width: usize) -> String {
    let lines = highlight_document(lang, code);
    if width == 0 {
        return lines.join("\n");
    }
    lines
        .iter()
        .map(|l| super::links::ansi_wrap(l, width))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn pad_highlight_lines(mut lines: Vec<String>, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    if lines.len() > n {
        lines.truncate(n);
    }
    while lines.len() < n {
        lines.push(String::new());
    }
    lines
}

/// truncateWidth shortens s (ANSI-aware) to width cells, appending an
/// ellipsis when anything was cut.
pub fn truncate_width(s: &str, width: usize) -> String {
    if width < 1 {
        return String::new();
    }
    let total = super::links::visible_width(s);
    if total <= width {
        return s.to_string();
    }
    let ellipsis = "…";
    let budget = width.saturating_sub(super::links::visible_width(ellipsis));
    if budget < 1 {
        return ellipsis.to_string();
    }
    let mut b = String::new();
    let mut w = 0usize;
    for tok in super::links::tokens(s) {
        match tok {
            super::links::Tok::Ch(c) => {
                let rw = super::links::char_width(c);
                if w + rw > budget {
                    break;
                }
                b.push(c);
                w += rw;
            }
            super::links::Tok::Esc(seq) => b.push_str(seq),
        }
    }
    b + ellipsis
}

#[cfg(test)]
mod tests {
    use super::super::colors::{
        ansi_fg, COLOR_FOREGROUND, COLOR_MUTED, COLOR_PRIMARY, COLOR_SYNTAX_STRING,
        COLOR_SYNTAX_TYPE,
    };
    use super::*;

    #[test]
    fn comments_are_muted() {
        let lines = highlight_document("a.go", "// hi\nx := 1\n");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(&ansi_fg(COLOR_MUTED)), "{}", lines[0]);
    }

    #[test]
    fn go_keywords_primary_strings_typed() {
        let lines = highlight_document("a.go", "func main() {\n\tfmt.Println(\"hi\")\n}\n");
        assert!(lines[0].contains(&ansi_fg(COLOR_PRIMARY)), "{}", lines[0]);
        assert!(
            lines[1].contains(&ansi_fg(COLOR_SYNTAX_STRING)),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn numbers_are_primary_types_colored() {
        let lines = highlight_document("a.go", "var x int = 42\n");
        assert!(lines[0].contains(&ansi_fg(COLOR_PRIMARY)), "{}", lines[0]);
    }

    #[test]
    fn rust_keywords_and_types() {
        let lines = highlight_document("m.rs", "let s: String = \"x\".into();\nfn main() {}\n");
        // Method calls/functions primary, strings syntax-string, types
        // and declarations syntax-type (chroma parity; note the Rust
        // grammar scopes `let` as storage.type, so it takes the type
        // color rather than keyword primary — see module docs).
        assert!(
            lines[0].contains(&ansi_fg(COLOR_SYNTAX_STRING)),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains(&ansi_fg(COLOR_PRIMARY)), "{}", lines[0]);
        assert!(lines[0].contains(&ansi_fg(COLOR_SYNTAX_TYPE)));
        assert!(!lines[1].is_empty());
    }

    #[test]
    fn builtin_type_color_used_for_int() {
        let lines = highlight_document("a.c", "int x;\n");
        assert!(
            lines[0].contains(&ansi_fg(COLOR_SYNTAX_TYPE)),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn unknown_language_falls_back_to_foreground() {
        let src = "just text\n";
        let lines = highlight_document("x.definitelynotalang", src);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains(&ansi_fg(COLOR_FOREGROUND)),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn empty_source_is_empty() {
        assert!(highlight_document("a.go", "").is_empty());
    }

    #[test]
    fn line_count_matches_source() {
        let src = "a\nbb\nccc\n";
        let lines = highlight_document("a.py", src);
        assert_eq!(lines.len(), 3);
        assert!(!lines.iter().any(|l| l.ends_with('\n')));
    }

    #[test]
    fn pad_lines_pads_and_truncates() {
        assert_eq!(
            pad_highlight_lines(vec!["a".into()], 3),
            vec!["a".to_string(), String::new(), String::new()]
        );
        assert_eq!(
            pad_highlight_lines(vec!["a".into(), "b".into(), "c".into()], 2),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(pad_highlight_lines(vec![], 0).is_empty());
    }

    #[test]
    fn truncate_respects_width_and_ellipsis() {
        assert_eq!(truncate_width("hello", 5), "hello");
        assert_eq!(truncate_width("hello!", 5), "hell…");
        assert_eq!(truncate_width("hello", 0), "");
        assert_eq!(truncate_width("日本語", 5), "日本…");
    }

    #[test]
    fn highlight_code_block_joins_lines() {
        let out = highlight_code("x := 1\ny := 2\n", "go", 0);
        assert_eq!(out.matches('\n').count(), 1);
        let wrapped = highlight_code("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "go", 0);
        assert!(wrapped.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }
}
