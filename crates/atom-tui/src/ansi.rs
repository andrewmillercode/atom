//! ansi.rs converts the ANSI output of atom_core::render (markdown,
//! highlight, diff, links all emit SGR strings) into ratatui styled
//! Lines, and holds the theme styles shared by every widget.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---------------------------------------------------------------------------
// Theme: colors.rs hex values as ratatui types.
// ---------------------------------------------------------------------------

fn hex_color(hex: &str) -> Color {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return Color::Reset;
    }
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
    Color::Rgb(r, g, b)
}

pub fn c_primary() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Primary)
}
pub fn c_primary_dark() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::PrimaryDark)
}
pub fn c_secondary() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Secondary)
}
pub fn c_muted() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Muted)
}
pub fn c_muted_extra() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::MutedExtra)
}
pub fn c_muted_deepest() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::MutedDeepest)
}
pub fn c_foreground() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Foreground)
}
pub fn c_background() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Background)
}
pub fn c_border() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Border)
}
pub fn c_card_dark() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::CardDark)
}
pub fn c_card_light() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::CardLight)
}
pub fn c_select() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::Select)
}
pub fn c_syntax_type() -> Color {
    theme_color(atom_core::render::colors::ThemeColor::SyntaxType)
}

fn theme_color(role: atom_core::render::colors::ThemeColor) -> Color {
    hex_color(&atom_core::render::colors::theme_color(role))
}

/// Mirrors tui.go's style block.
pub fn style_user() -> Style {
    Style::new().fg(c_foreground()).bg(c_card_light())
}
/// Default terminal foreground (no fg set), for text that should stand
/// out from dim chrome like the status bar.
pub fn style_foreground() -> Style {
    Style::new()
}
/// Bold title style for fullscreen overlay headers.
pub fn style_title() -> Style {
    Style::new().fg(c_foreground()).add_modifier(Modifier::BOLD)
}
pub fn style_primary() -> Style {
    Style::new().fg(c_primary())
}
pub fn style_reasoning() -> Style {
    Style::new().fg(c_muted())
}
pub fn style_tool() -> Style {
    Style::new().fg(c_foreground()).bg(c_card_dark())
}
pub fn style_tool_name() -> Style {
    Style::new().fg(c_muted()).bg(c_card_dark())
}
pub fn style_tool_hint() -> Style {
    Style::new().fg(c_muted()).bg(c_card_dark())
}
pub fn style_error() -> Style {
    Style::new().fg(c_secondary())
}
pub fn style_cursor() -> Style {
    Style::new().fg(c_primary()).add_modifier(Modifier::BOLD)
}
pub fn style_dim() -> Style {
    Style::new().fg(c_muted())
}
pub fn style_inactive() -> Style {
    Style::new().fg(c_primary_dark())
}
pub fn style_selected() -> Style {
    Style::new().fg(c_primary()).add_modifier(Modifier::BOLD)
}
pub fn style_prompt_border() -> Style {
    Style::new().fg(c_border())
}
pub fn style_img_chip() -> Style {
    Style::new().fg(c_background()).bg(c_primary())
}
pub fn style_file_chip() -> Style {
    Style::new()
        .fg(c_syntax_type())
        .add_modifier(Modifier::BOLD)
}
pub fn style_query_sel() -> Style {
    Style::new().fg(c_background()).bg(c_primary())
}
/// Mouse-dragged selection wash in the conversation viewport.
pub fn style_select() -> Style {
    Style::new().fg(c_foreground()).bg(c_select())
}

/// The base frame style: app foreground/background on every cell.
pub fn frame_style() -> Style {
    Style::new().fg(c_foreground()).bg(c_background())
}

// ---------------------------------------------------------------------------
// ANSI parsing.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Default)]
struct Attrs {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    italic: bool,
    underline: bool,
    crossed_out: bool,
    dim: bool,
}

impl Attrs {
    fn style(self) -> Style {
        let mut s = Style::new();
        if let Some(fg) = self.fg {
            s = s.fg(fg);
        }
        if let Some(bg) = self.bg {
            s = s.bg(bg);
        }
        let mut m = Modifier::empty();
        if self.bold {
            m |= Modifier::BOLD;
        }
        if self.italic {
            m |= Modifier::ITALIC;
        }
        if self.underline {
            m |= Modifier::UNDERLINED;
        }
        if self.crossed_out {
            m |= Modifier::CROSSED_OUT;
        }
        if self.dim {
            m |= Modifier::DIM;
        }
        if !m.is_empty() {
            s = s.add_modifier(m);
        }
        s
    }
}

fn sgr_color(params: &[u16]) -> Option<(Color, usize)> {
    match params.first()? {
        5 => {
            if params.len() < 2 {
                return None;
            }
            Some((Color::Indexed(params[1].min(255) as u8), 2))
        }
        2 => {
            if params.len() < 4 {
                return None;
            }
            Some((
                Color::Rgb(params[1] as u8, params[2] as u8, params[3] as u8),
                4,
            ))
        }
        _ => None,
    }
}

fn indexed_color(n: u16) -> Color {
    const BASE: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xcd, 0x00, 0x00],
        [0x00, 0xcd, 0x00],
        [0xcd, 0xcd, 0x00],
        [0x00, 0x00, 0xee],
        [0xcd, 0x00, 0xcd],
        [0x00, 0xcd, 0xcd],
        [0xe5, 0xe5, 0xe5],
        [0x7f, 0x7f, 0x7f],
        [0xff, 0x00, 0x00],
        [0x00, 0xff, 0x00],
        [0xff, 0xff, 0x00],
        [0x5c, 0x5c, 0xff],
        [0xff, 0x00, 0xff],
        [0x00, 0xff, 0xff],
        [0xff, 0xff, 0xff],
    ];
    let [r, g, b] = BASE[(n as usize).min(15)];
    Color::Rgb(r, g, b)
}

enum Tok {
    Text(String),
    /// OSC 8 link open, carrying the target URI.
    LinkStart(String),
    LinkEnd,
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut plain = String::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI ... final byte.
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    break;
                }
                if !plain.is_empty() {
                    toks.push(Tok::Text(std::mem::take(&mut plain)));
                }
                if bytes[j] == b'm' {
                    toks.push(Tok::Text(
                        // Keep the SGR so the shared parser below handles it.
                        s[i..=j].to_string(),
                    ));
                    plain = String::new();
                }
                i = j + 1;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                // OSC ... BEL or ST.
                let rest = &s[i + 2..];
                let end_bel = rest.find('\x07');
                let end_st = rest.find("\x1b\\");
                let (end_len, matched) = match (end_bel, end_st) {
                    (Some(a), Some(b)) => {
                        if a <= b {
                            (a + 1, false)
                        } else {
                            (b + 2, true)
                        }
                    }
                    (Some(a), None) => (a + 1, false),
                    (None, Some(b)) => (b + 2, true),
                    (None, None) => {
                        break;
                    }
                };
                let body = &rest[..end_len.saturating_sub(if matched { 2 } else { 1 })];
                if !plain.is_empty() {
                    toks.push(Tok::Text(std::mem::take(&mut plain)));
                }
                if let Some(payload) = body.strip_prefix("8;") {
                    if let Some((_, uri)) = payload.split_once(';') {
                        toks.push(if uri.is_empty() {
                            Tok::LinkEnd
                        } else {
                            Tok::LinkStart(uri.to_string())
                        });
                    }
                }
                i = i + 2 + end_len;
                continue;
            }
            // Unknown escape: drop the ESC byte pair.
            i += 2;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    if !plain.is_empty() {
        toks.push(Tok::Text(plain));
    }
    toks
}

fn apply_sgr(attrs: &mut Attrs, params: &[u16]) {
    let mut k = 0usize;
    while k < params.len() {
        match params[k] {
            0 => *attrs = Attrs::default(),
            1 => attrs.bold = true,
            2 => attrs.dim = true,
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            9 => attrs.crossed_out = true,
            22 => {
                attrs.bold = false;
                attrs.dim = false;
            }
            23 => attrs.italic = false,
            24 => attrs.underline = false,
            29 => attrs.crossed_out = false,
            30..=37 => attrs.fg = Some(indexed_color(params[k] - 30)),
            39 => attrs.fg = None,
            40..=47 => attrs.bg = Some(indexed_color(params[k] - 40)),
            49 => attrs.bg = None,
            90..=97 => attrs.fg = Some(indexed_color(params[k] - 90 + 8)),
            100..=107 => attrs.bg = Some(indexed_color(params[k] - 100 + 8)),
            38 | 48 => {
                // Color spec follows the 38/48 marker byte.
                if let Some((c, used)) = sgr_color(&params[k + 1..]) {
                    if params[k] == 38 {
                        attrs.fg = Some(c);
                    } else {
                        attrs.bg = Some(c);
                    }
                    // `used` covers the spec bytes after the marker; the
                    // loop tail advances past the marker itself.
                    k += used;
                }
            }
            _ => {}
        }
        k += 1;
    }
}

/// A clickable OSC 8 hyperlink region on one rendered line: visible
/// cells [c0, c1) open `uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRegion {
    pub c0: usize,
    pub c1: usize,
    pub uri: String,
}

/// Parser state threaded across the lines of one ANSI block so
/// multi-line links keep their URI (the wrapper never re-emits an open
/// sequence on continuation lines).
#[derive(Default)]
struct ParseState {
    attrs: Attrs,
    link: Option<String>,
}

/// Accumulates one line's spans while tracking visible columns, so
/// link text can be recorded as cell ranges.
#[derive(Default)]
struct LineBuilder {
    spans: Vec<Span<'static>>,
    regions: Vec<LinkRegion>,
    buf: String,
    /// visible column where `buf` starts
    buf_col: usize,
    /// visible column just past `buf`
    col: usize,
}

impl LineBuilder {
    fn push_text(&mut self, t: &str) {
        use unicode_width::UnicodeWidthStr;
        self.buf.push_str(t);
        self.col += t.width();
    }

    fn flush(&mut self, st: &ParseState, link_override: Option<Style>) {
        if self.buf.is_empty() {
            return;
        }
        let style = if st.link.is_some() {
            merge_link_style(st.attrs.style(), link_override)
        } else {
            st.attrs.style()
        };
        let content = std::mem::take(&mut self.buf);
        self.spans.push(Span::styled(content, style));
        if let Some(uri) = st.link.as_ref() {
            let (c0, c1) = (self.buf_col, self.col);
            match self.regions.last_mut() {
                Some(last) if last.uri == *uri && last.c1 == c0 => last.c1 = c1,
                _ => self.regions.push(LinkRegion {
                    c0,
                    c1,
                    uri: uri.clone(),
                }),
            }
        }
        self.buf_col = self.col;
    }
}

fn parse_line_linked(
    s: &str,
    link_override: Option<Style>,
    st: &mut ParseState,
) -> (Line<'static>, Vec<LinkRegion>) {
    let mut lb = LineBuilder::default();
    for tok in tokenize(s) {
        match tok {
            Tok::Text(t) => {
                if t.starts_with('\x1b') && t.ends_with('m') && t.len() > 2 {
                    lb.flush(st, link_override);
                    let inner = &t[2..t.len() - 1];
                    let params: Vec<u16> = inner
                        .split(';')
                        .map(|p| p.parse::<u16>().unwrap_or(0))
                        .collect();
                    apply_sgr(&mut st.attrs, &params);
                } else {
                    lb.push_text(&t);
                }
            }
            Tok::LinkStart(uri) => {
                lb.flush(st, link_override);
                st.link = Some(uri);
            }
            Tok::LinkEnd => {
                lb.flush(st, link_override);
                st.link = None;
            }
        }
    }
    lb.flush(st, link_override);
    (Line::from(lb.spans), lb.regions)
}

/// Converts an ANSI-styled string into one ratatui Line. OSC 8 wrappers
/// are stripped; link text keeps secondary+underline styling per spec.
pub fn ansi_to_line(s: &str) -> Line<'static> {
    ansi_to_line_with(s, None)
}

/// Like [`ansi_to_line`] but `link_style` overrides the default link look.
pub fn ansi_to_line_with(s: &str, link_override: Option<Style>) -> Line<'static> {
    let mut st = ParseState::default();
    parse_line_linked(s, link_override, &mut st).0
}

/// Like [`ansi_to_line`] but also returns the clickable link regions
/// (visible-cell ranges plus URI) of the single line.
pub fn ansi_to_line_linked(s: &str) -> (Line<'static>, Vec<LinkRegion>) {
    let mut st = ParseState::default();
    parse_line_linked(s, None, &mut st)
}

/// Styled lines plus, per line, the OSC 8 clickable regions.
pub struct LinkedLines {
    pub lines: Vec<Line<'static>>,
    /// Same length as `lines`; regions in visible-column coordinates.
    pub links: Vec<Vec<LinkRegion>>,
}

/// Like [`ansi_to_lines`] but also returns the clickable link regions,
/// threading link state across lines so a link wrapped onto several
/// rows is clickable on each of them.
pub fn ansi_to_lines_linked(s: &str) -> LinkedLines {
    let mut st = ParseState::default();
    let mut lines = Vec::new();
    let mut links = Vec::new();
    for part in s.split('\n') {
        let (line, regions) = parse_line_linked(part, None, &mut st);
        lines.push(line);
        links.push(regions);
    }
    LinkedLines { lines, links }
}

fn merge_link_style(base: Style, override_style: Option<Style>) -> Style {
    let mut s = base;
    s = s.add_modifier(Modifier::UNDERLINED);
    if s.fg.is_none() {
        s = s.fg(c_secondary());
    }
    if let Some(o) = override_style {
        s = o;
    }
    s
}

/// Splits an ANSI string into multiple styled Lines on '\n'.
pub fn ansi_to_lines(s: &str) -> Vec<Line<'static>> {
    ansi_to_lines_linked(s).lines
}

/// Plain-text content of a styled line.
pub fn line_plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Visible cell width of a styled line.
pub fn line_width(line: &Line<'_>) -> usize {
    use unicode_width::UnicodeWidthStr;
    line.spans.iter().map(|s| s.content.width()).sum()
}

/// Applies `style` to the visible cell range [c0, c1) of a line by
/// splitting spans at the boundaries. Out-of-range cells are ignored.
pub fn style_line_range(line: &Line<'static>, c0: usize, c1: usize, style: Style) -> Line<'static> {
    if c1 <= c0 || line.spans.is_empty() {
        return line.clone();
    }
    let mut out: Vec<Span> = Vec::with_capacity(line.spans.len() + 2);
    let mut col = 0usize;
    for span in &line.spans {
        use unicode_width::UnicodeWidthStr;
        let w = span.content.width();
        let s0 = col;
        let s1 = col + w;
        col = s1;
        if s1 <= c0 || s0 >= c1 {
            out.push(span.clone());
            continue;
        }
        // Split into up to three pieces.
        let chars: Vec<(usize, char)> = span
            .content
            .chars()
            .scan(0usize, |acc, ch| {
                let start = *acc;
                *acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                Some((start, ch))
            })
            .collect();
        let piece = |lo: usize, hi: usize| -> String {
            let mut t = String::new();
            for (off, ch) in &chars {
                if *off >= lo && *off < hi {
                    t.push(*ch);
                }
            }
            t
        };
        let lo = (c0.saturating_sub(s0)).min(w);
        let hi = (c1.saturating_sub(s0)).min(w);
        if lo > 0 {
            out.push(Span::raw(piece(0, lo)));
        }
        out.push(Span::styled(piece(lo, hi), span.style.patch(style)));
        if hi < w {
            out.push(Span::raw(piece(hi, w)));
        }
    }
    // Keep the line-level style: math placeholder rows carry the Kitty
    // image id there, and a selection wipe would blank the formula.
    let mut selected = Line::from(out);
    selected.style = line.style;
    selected
}

/// Cuts a line's visible cells to [c0, c1).
pub fn cut_line_range(line: &Line<'static>, c0: usize, c1: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut col = 0usize;
    for span in &line.spans {
        let w = span.content.width();
        let s0 = col;
        let s1 = col + w;
        col = s1;
        if s1 <= c0 {
            continue;
        }
        if s0 >= c1 {
            break;
        }
        let mut acc = 0usize;
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let start = s0 + acc;
            acc += cw;
            if start >= c0 && start < c1 {
                out.push(ch);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sgr_fg_bold() {
        let line = ansi_to_line("\x1b[38;2;130;172;217;1mhi\x1b[0m");
        assert_eq!(line_plain(&line), "hi");
        let sp = &line.spans[0];
        assert_eq!(sp.style.fg, Some(Color::Rgb(130, 172, 217)));
        assert!(sp.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn strips_osc8_keeps_text_styled() {
        let s = format!(
            "{}link{}\x1b[0m tail",
            atom_core::render::links::osc8_open("https://x.co"),
            atom_core::render::links::osc8_close()
        );
        let line = ansi_to_line(&s);
        let text = line_plain(&line);
        assert_eq!(text, "link tail");
        assert_eq!(line.spans[0].content.as_ref(), "link");
        assert!(line.spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn osc8_regions_capture_uri_and_cells() {
        let s = format!(
            "{}link{} tail",
            atom_core::render::links::osc8_open("https://x.co"),
            atom_core::render::links::osc8_close()
        );
        let (line, regions) = ansi_to_line_linked(&s);
        assert_eq!(line_plain(&line), "link tail");
        assert_eq!(
            regions,
            vec![LinkRegion {
                c0: 0,
                c1: 4,
                uri: "https://x.co".to_string(),
            }]
        );
        assert_eq!(cut_line_range(&line, 0, 4), "link");
    }

    #[test]
    fn osc8_regions_thread_across_lines() {
        let s = format!(
            "{}first\nsecond{}",
            atom_core::render::links::osc8_open("https://x.co"),
            atom_core::render::links::osc8_close()
        );
        let linked = ansi_to_lines_linked(&s);
        assert_eq!(linked.lines.len(), 2);
        assert_eq!(linked.links.len(), 2);
        assert_eq!(linked.links[0][0].c0, 0);
        assert_eq!(linked.links[0][0].c1, 5);
        assert_eq!(linked.links[1][0].c0, 0);
        assert_eq!(linked.links[1][0].c1, 6);
        assert!(linked.links.iter().all(|rs| rs[0].uri == "https://x.co"));
    }

    #[test]
    fn wrapped_link_is_clickable_on_every_row() {
        use atom_core::render::links::wrap_linked;
        let body = wrap_linked(
            "prose before and then https://example.com/very/long/url and some prose after",
            24,
            "",
            "",
            "",
        );
        let linked = ansi_to_lines_linked(&body);
        let hits: Vec<String> = linked
            .lines
            .iter()
            .zip(&linked.links)
            .filter_map(|(line, regions)| regions.first().map(|r| cut_line_range(line, r.c0, r.c1)))
            .collect();
        assert!(!hits.is_empty(), "wrapped link produced no regions");
        for frag in &hits {
            assert!(
                "https://example.com/very/long/url".contains(frag.as_str()),
                "fragment {frag:?} is not part of the link"
            );
        }
    }

    #[test]
    fn osc8_close_does_not_style_tail_or_next_line() {
        let s = format!(
            "{}link{} tail\nnext",
            atom_core::render::links::osc8_open("https://x.co"),
            atom_core::render::links::osc8_close()
        );
        let lines = ansi_to_lines(&s);

        assert_eq!(line_plain(&lines[0]), "link tail");
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(!lines[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert_eq!(lines[0].spans[1].style.fg, None);
        assert_eq!(line_plain(&lines[1]), "next");
        assert!(!lines[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert_eq!(lines[1].spans[0].style.fg, None);
    }

    #[test]
    fn osc8_st_close_ends_multiline_link() {
        let lines = ansi_to_lines("\x1b]8;;https://x.co\x1b\\first\nsecond\x1b]8;;\x1b\\ tail");

        assert!(lines[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(!lines[1].spans[1]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn parses_crossed_out_on_and_off() {
        let line = ansi_to_line("\x1b[9mremoved\x1b[29m kept");

        assert!(line.spans[0]
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
        assert!(!line.spans[1]
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn bg_colors_survive() {
        let line = ansi_to_line("\x1b[48;2;16;19;23mx\x1b[49m");
        assert_eq!(line.spans[0].style.bg, Some(Color::Rgb(16, 19, 23)));
    }

    #[test]
    fn multi_line_split() {
        let lines = ansi_to_lines("a\x1b[1mb\x1b[0m\ncd");
        assert_eq!(lines.len(), 2);
        assert_eq!(line_plain(&lines[1]), "cd");
    }

    #[test]
    fn multi_line_split_preserves_active_style() {
        let lines = ansi_to_lines("\x1b[38;2;107;110;119mfirst\nsecond\x1b[39m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(107, 110, 119)));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Rgb(107, 110, 119)));
    }

    #[test]
    fn range_styling_splits_spans() {
        let line = Line::from(vec![Span::raw("hello world")]);
        let styled = style_line_range(&line, 2, 7, Style::new().bg(Color::Red));
        assert_eq!(line_plain(&styled), "hello world");
        assert_eq!(styled.spans.len(), 3);
        assert_eq!(styled.spans[1].content.as_ref(), "llo w");
        assert_eq!(styled.spans[1].style.bg, Some(Color::Red));
    }

    #[test]
    fn range_styling_keeps_the_line_level_style() {
        // Math placeholder rows carry the Kitty image id as the Line-level
        // fg; selecting text over them must not strip it (blank formula).
        let line = Line::styled(
            "\u{10EEEE}\u{0305}\u{030D}",
            Style::new().fg(Color::Rgb(9, 8, 7)),
        );
        let styled = style_line_range(&line, 0, 1, Style::new().bg(Color::Red));
        assert_eq!(styled.style.fg, Some(Color::Rgb(9, 8, 7)));
        assert_eq!(styled.spans[0].style.bg, Some(Color::Red));
    }
}
