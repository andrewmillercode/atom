use std::{borrow::Cow, fmt::Write as _, ops::Range};

/// Delimiter that introduced a display-math region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayMathDelimiter {
    /// `$$ ... $$`.
    Dollars,
    /// `\[ ... \]`.
    Brackets,
    /// A complete top-level display environment such as `align`.
    Environment,
}

/// One closed Markdown display-math region.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayMath<'a> {
    source: &'a str,
    full_source: &'a str,
    range: Range<usize>,
    delimiter: DisplayMathDelimiter,
    environment: Option<&'a str>,
}

impl<'a> DisplayMath<'a> {
    /// TeX passed to the renderer. Delimiters are removed except for complete
    /// display environments, which are already self-delimiting.
    pub const fn source(&self) -> &'a str {
        self.source
    }

    /// Exact source including its Markdown delimiters.
    pub const fn full_source(&self) -> &'a str {
        self.full_source
    }

    /// Byte range of [`Self::full_source`] in the original Markdown.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Kind of display delimiter.
    pub const fn delimiter(&self) -> DisplayMathDelimiter {
        self.delimiter
    }

    /// Environment name for [`DisplayMathDelimiter::Environment`].
    pub const fn environment(&self) -> Option<&'a str> {
        self.environment
    }
}

/// Alternating prose and display-math regions from a Markdown document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkdownSegment<'a> {
    /// Markdown that the application's existing renderer should handle.
    Text(&'a str),
    /// A complete display formula.
    DisplayMath(DisplayMath<'a>),
}

/// Finds completed display math while ignoring fenced and inline code.
///
/// Incomplete delimiters are deliberately returned as ordinary Markdown so a
/// streaming TUI never hides model output that has not closed yet.
pub fn display_math(source: &str) -> Vec<DisplayMath<'_>> {
    let protected = code_ranges(source);
    let mut regions = Vec::new();
    let mut cursor = 0;
    let mut protected_index = 0;

    while cursor < source.len() {
        if let Some(range) = protected.get(protected_index) {
            if cursor >= range.end {
                protected_index += 1;
                continue;
            }
            if range.contains(&cursor) {
                cursor = range.end;
                protected_index += 1;
                continue;
            }
        }

        if source[cursor..].starts_with("$$") && !is_escaped(source, cursor) {
            if let Some(end_start) = find_unescaped(source, cursor + 2, "$$", &protected) {
                let end = end_start + 2;
                let body = &source[cursor + 2..end_start];
                if !body.trim().is_empty() {
                    regions.push(DisplayMath {
                        source: body.trim(),
                        full_source: &source[cursor..end],
                        range: cursor..end,
                        delimiter: DisplayMathDelimiter::Dollars,
                        environment: None,
                    });
                    cursor = end;
                    continue;
                }
            }
        } else if source[cursor..].starts_with(r"\[") && !is_escaped(source, cursor) {
            if let Some(end_start) = find_unescaped(source, cursor + 2, r"\]", &protected) {
                let end = end_start + 2;
                let body = &source[cursor + 2..end_start];
                if !body.trim().is_empty() {
                    regions.push(DisplayMath {
                        source: body.trim(),
                        full_source: &source[cursor..end],
                        range: cursor..end,
                        delimiter: DisplayMathDelimiter::Brackets,
                        environment: None,
                    });
                    cursor = end;
                    continue;
                }
            }
        } else if source[cursor..].starts_with(r"\begin{") && !is_escaped(source, cursor) {
            if let Some((environment, opener_end)) =
                parse_environment(source, cursor).filter(|(name, _)| is_display_environment(name))
            {
                let closing = format!(r"\end{{{environment}}}");
                if let Some(end_start) = find_unescaped(source, opener_end, &closing, &protected) {
                    let end = end_start + closing.len();
                    regions.push(DisplayMath {
                        source: &source[cursor..end],
                        full_source: &source[cursor..end],
                        range: cursor..end,
                        delimiter: DisplayMathDelimiter::Environment,
                        environment: Some(environment),
                    });
                    cursor = end;
                    continue;
                }
            }
        }

        cursor += source[cursor..].chars().next().map_or(1, char::len_utf8);
    }
    regions
}

/// Splits Markdown into ordinary text and completed display formulas.
pub fn markdown_segments(source: &str) -> Vec<MarkdownSegment<'_>> {
    let formulas = display_math(source);
    let mut segments = Vec::with_capacity(formulas.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0;
    for formula in formulas {
        let range = formula.range();
        if cursor < range.start {
            segments.push(MarkdownSegment::Text(&source[cursor..range.start]));
        }
        cursor = range.end;
        segments.push(MarkdownSegment::DisplayMath(formula));
    }
    if cursor < source.len() {
        segments.push(MarkdownSegment::Text(&source[cursor..]));
    }
    if segments.is_empty() {
        segments.push(MarkdownSegment::Text(source));
    }
    segments
}

/// Completes a trailing, currently open display-math region for previewing a
/// streaming Markdown document.
///
/// The returned Markdown borrows `source` when every display region is
/// already closed. Otherwise it appends only synthetic closing tokens for
/// unmatched TeX groups, `\left` delimiters, environments, and the outer
/// display delimiter. Existing source bytes are never changed, so callers can
/// continue to retain the original stream for selection and copying.
///
/// Feed this temporary view to [`display_math`] on each streaming update. A
/// renderer may still reject a prefix that ends in the middle of a command;
/// applications should keep their most recent successfully rendered formula
/// visible until a newer prefix becomes ready.
pub fn heal_streaming_display_math(source: &str) -> Cow<'_, str> {
    let protected = code_ranges(source);
    let mut protected_index = 0;
    let mut cursor = 0;
    let mut closers = Vec::new();

    while cursor < source.len() {
        if let Some(range) = protected.get(protected_index) {
            if cursor >= range.end {
                protected_index += 1;
                continue;
            }
            if range.contains(&cursor) {
                cursor = range.end;
                protected_index += 1;
                continue;
            }
        }

        if let Some(next) = advance_math_scanner(source, cursor, &mut closers) {
            cursor = next;
            continue;
        }

        cursor += source[cursor..].chars().next().map_or(1, char::len_utf8);
    }

    if closers.is_empty() {
        return Cow::Borrowed(source);
    }
    let mut healed = String::with_capacity(
        source
            .len()
            .saturating_add(closers.len().saturating_mul(16))
            .saturating_add(1),
    );
    healed.push_str(source);
    healed.push('\n');
    for closer in closers.iter().rev() {
        closer.write_to(&mut healed);
    }
    Cow::Owned(healed)
}

fn advance_math_scanner(
    source: &str,
    cursor: usize,
    closers: &mut Vec<MathCloser>,
) -> Option<usize> {
    if closers.is_empty() {
        if source[cursor..].starts_with("$$") && !is_escaped(source, cursor) {
            closers.push(MathCloser::Dollars);
            return Some(cursor + 2);
        }
        if source[cursor..].starts_with(r"\[") && !is_escaped(source, cursor) {
            closers.push(MathCloser::Brackets);
            return Some(cursor + 2);
        }
        if source[cursor..].starts_with(r"\begin{") && !is_escaped(source, cursor) {
            if let Some((environment, opener_end)) = parse_environment(source, cursor) {
                if is_display_environment(environment) {
                    closers.push(MathCloser::Environment(environment.to_owned()));
                    return Some(opener_end);
                }
            }
        }
        return None;
    }
    if matches!(closers.first(), Some(MathCloser::Dollars))
        && source[cursor..].starts_with("$$")
        && !is_escaped(source, cursor)
        && closers.len() == 1
    {
        closers.clear();
        return Some(cursor + 2);
    }
    if matches!(closers.first(), Some(MathCloser::Brackets))
        && source[cursor..].starts_with(r"\]")
        && !is_escaped(source, cursor)
        && closers.len() == 1
    {
        closers.clear();
        return Some(cursor + 2);
    }
    if source[cursor..].starts_with(r"\begin{") && !is_escaped(source, cursor) {
        if let Some((environment, opener_end)) = parse_environment(source, cursor) {
            closers.push(MathCloser::Environment(environment.to_owned()));
            return Some(opener_end);
        }
    }
    if source[cursor..].starts_with(r"\end{") && !is_escaped(source, cursor) {
        if let Some((environment, closer_end)) = parse_end_environment(source, cursor) {
            if closers
                .last()
                .is_some_and(|closer| closer.closes_environment(environment))
            {
                let _ = closers.pop();
                return Some(closer_end);
            }
        }
    }
    if source[cursor..].starts_with(r"\left")
        && command_ends_at(source, cursor + r"\left".len())
        && !is_escaped(source, cursor)
    {
        closers.push(MathCloser::Left);
        return Some(cursor + r"\left".len());
    }
    if source[cursor..].starts_with(r"\right")
        && command_ends_at(source, cursor + r"\right".len())
        && !is_escaped(source, cursor)
        && matches!(closers.last(), Some(MathCloser::Left))
    {
        let _ = closers.pop();
        return Some(cursor + r"\right".len());
    }
    if source.as_bytes()[cursor] == b'{' && !is_escaped(source, cursor) {
        closers.push(MathCloser::Group);
        return Some(cursor + 1);
    }
    if source.as_bytes()[cursor] == b'}'
        && !is_escaped(source, cursor)
        && matches!(closers.last(), Some(MathCloser::Group))
    {
        let _ = closers.pop();
        return Some(cursor + 1);
    }
    None
}

#[derive(Debug)]
enum MathCloser {
    Dollars,
    Brackets,
    Environment(String),
    Group,
    Left,
}

impl MathCloser {
    fn closes_environment(&self, environment: &str) -> bool {
        matches!(self, Self::Environment(open) if open == environment)
    }

    fn write_to(&self, output: &mut String) {
        match self {
            Self::Dollars => output.push_str("$$"),
            Self::Brackets => output.push_str(r"\]"),
            Self::Environment(environment) => {
                let _ = writeln!(output, "\\end{{{environment}}}");
            }
            Self::Group => output.push('}'),
            Self::Left => output.push_str(r"\right."),
        }
    }
}

fn parse_environment(source: &str, start: usize) -> Option<(&str, usize)> {
    let name_start = start.checked_add(r"\begin{".len())?;
    let close = source[name_start..].find('}')? + name_start;
    let name = &source[name_start..close];
    (!name.is_empty()).then_some((name, close + 1))
}

fn parse_end_environment(source: &str, start: usize) -> Option<(&str, usize)> {
    let name_start = start.checked_add(r"\end{".len())?;
    let close = source[name_start..].find('}')? + name_start;
    let name = &source[name_start..close];
    (!name.is_empty()).then_some((name, close + 1))
}

fn command_ends_at(source: &str, end: usize) -> bool {
    source
        .get(end..)
        .and_then(|remaining| remaining.chars().next())
        .is_none_or(|character| !character.is_ascii_alphabetic())
}

fn is_display_environment(environment: &str) -> bool {
    matches!(
        environment,
        "align"
            | "align*"
            | "alignat"
            | "alignat*"
            | "displaymath"
            | "equation"
            | "equation*"
            | "flalign"
            | "flalign*"
            | "gather"
            | "gather*"
            | "multline"
            | "multline*"
    )
}

fn find_unescaped(
    source: &str,
    mut cursor: usize,
    needle: &str,
    protected: &[Range<usize>],
) -> Option<usize> {
    let mut protected_index = protected.partition_point(|range| range.end <= cursor);
    while cursor <= source.len().saturating_sub(needle.len()) {
        if let Some(range) = protected.get(protected_index) {
            if cursor >= range.end {
                protected_index += 1;
                continue;
            }
            if range.contains(&cursor) {
                cursor = range.end;
                protected_index += 1;
                continue;
            }
        }
        if source[cursor..].starts_with(needle) && !is_escaped(source, cursor) {
            return Some(cursor);
        }
        cursor += source[cursor..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn is_escaped(source: &str, index: usize) -> bool {
    source[..index]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        % 2
        == 1
}

fn code_ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = fenced_code_ranges(source);
    ranges.extend(inline_code_ranges(source, &ranges));
    ranges.sort_unstable_by_key(|range| range.start);
    ranges
}

fn fenced_code_ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(u8, usize, usize)> = None;
    let mut offset = 0;
    for line_with_ending in source.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
        let candidate = if indentation <= 3 {
            fence_run(&line[indentation..])
        } else {
            None
        };
        match (open, candidate) {
            (None, Some((marker, length))) if length >= 3 => {
                open = Some((marker, length, offset));
            }
            (Some((marker, length, start)), Some((closing, closing_length)))
                if marker == closing && closing_length >= length =>
            {
                ranges.push(start..offset + line_with_ending.len());
                open = None;
            }
            _ => {}
        }
        offset += line_with_ending.len();
    }
    if let Some((_, _, start)) = open {
        ranges.push(start..source.len());
    }
    ranges
}

fn fence_run(source: &str) -> Option<(u8, usize)> {
    let marker = *source.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    Some((
        marker,
        source.bytes().take_while(|byte| *byte == marker).count(),
    ))
}

fn inline_code_ranges(source: &str, fenced: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        if let Some(range) = fenced.iter().find(|range| range.contains(&cursor)) {
            cursor = range.end;
            continue;
        }
        if source.as_bytes()[cursor] != b'`' {
            cursor += source[cursor..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let length = source.as_bytes()[cursor..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let delimiter = "`".repeat(length);
        let content_start = cursor + length;
        let line_end = source[content_start..]
            .find('\n')
            .map_or(source.len(), |relative| content_start + relative);
        if let Some(relative) = source[content_start..line_end].find(&delimiter) {
            let end = content_start + relative + length;
            ranges.push(cursor..end);
            cursor = end;
        } else {
            cursor = content_start;
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        DisplayMathDelimiter, MarkdownSegment, display_math, heal_streaming_display_math,
        markdown_segments,
    };

    #[test]
    fn finds_common_closed_display_math() {
        let markdown = "Before\n\n$$ x^2 $$\n\n\\[\\frac{a}{b}\\]\n\n\
                        \\begin{align*}a&=b\\\\c&=d\\end{align*}\n\nAfter";
        let regions = display_math(markdown);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0].source(), "x^2");
        assert_eq!(regions[0].delimiter(), DisplayMathDelimiter::Dollars);
        assert_eq!(regions[1].source(), r"\frac{a}{b}");
        assert_eq!(regions[1].delimiter(), DisplayMathDelimiter::Brackets);
        assert_eq!(regions[2].environment(), Some("align*"));
        assert!(regions[2].source().starts_with(r"\begin{align*}"));
    }

    #[test]
    fn ignores_code_and_incomplete_streaming_regions() {
        let markdown = "`$$ inline code $$`\n\n```tex\n\\[fenced\\]\n```\n\n$$still streaming";
        assert!(display_math(markdown).is_empty());
        assert_eq!(
            markdown_segments(markdown),
            vec![MarkdownSegment::Text(markdown)]
        );
    }

    #[test]
    fn heals_a_streaming_display_with_nested_tex_constructs() {
        let streaming = "\\[\n\\begin{aligned}\nx&=\\left(\\frac{1}{2";
        let healed = heal_streaming_display_math(streaming);
        assert_eq!(
            healed,
            "\\[\n\\begin{aligned}\nx&=\\left(\\frac{1}{2\n}\\right.\\end{aligned}\n\\]"
        );
        let regions = display_math(&healed);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].source(),
            "\\begin{aligned}\nx&=\\left(\\frac{1}{2\n}\\right.\\end{aligned}"
        );
    }

    #[test]
    fn heals_dollars_and_top_level_environments() {
        assert_eq!(
            heal_streaming_display_math("before\n$$x+1"),
            "before\n$$x+1\n$$"
        );
        assert_eq!(
            heal_streaming_display_math("\\begin{align*}a&=b"),
            "\\begin{align*}a&=b\n\\end{align*}\n"
        );
    }

    #[test]
    fn closed_math_and_code_remain_borrowed_and_unchanged() {
        for source in [
            "before\n\\[x\\]\nafter",
            "`\\[not math`\n\n```tex\n$$still code\n```",
        ] {
            let healed = heal_streaming_display_math(source);
            assert!(matches!(healed, Cow::Borrowed(_)));
            assert_eq!(healed, source);
        }
    }

    #[test]
    fn ignores_escaped_delimiters_and_non_display_environments() {
        let markdown = r"\$$not math$$ and \\[not math\] and \begin{pmatrix}a\end{pmatrix}";
        assert!(display_math(markdown).is_empty());
    }

    #[test]
    fn segments_preserve_every_source_byte() {
        let markdown = "A\n$$x$$\nB\n\\[y\\]\nC";
        let rebuilt = markdown_segments(markdown)
            .into_iter()
            .map(|segment| match segment {
                MarkdownSegment::Text(text) => text,
                MarkdownSegment::DisplayMath(math) => math.full_source(),
            })
            .collect::<String>();
        assert_eq!(rebuilt, markdown);
    }
}
