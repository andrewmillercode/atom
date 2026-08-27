//! Diff rendering: fileDiff (unified diff of a file change, from
//! main.go) and the colorizer renderDiff/parseUnifiedDiff/
//! paintWrappedDiffLine from highlight.go, plus the read_file line
//! window and image sniffing helpers that lived in main.go.

use similar::{ChangeTag, DiffTag, TextDiff};

use super::colors::{
    ansi_bg, ansi_fg, COLOR_CARD_DARK, COLOR_DIFF_ADD_BG, COLOR_DIFF_DEL_BG, COLOR_MUTED,
};
use super::highlight::highlight_document;
use super::highlight::pad_highlight_lines;
use super::links::ansi_wrap;

pub const DEFAULT_READ_FILE_LIMIT: i64 = 1000;

// ---------------------------------------------------------------------------
// fileDiff + unified formatting (stands in for go-udiff.Unified).
// ---------------------------------------------------------------------------

/// fileDiff returns a unified diff between old and new file content, or ""
/// when the content is unchanged. The diff headers carry the file path so
/// the client can label the change.
pub fn file_diff(path: &str, old: &[u8], new: &[u8]) -> String {
    if old == new {
        return String::new();
    }
    let o = String::from_utf8_lossy(old);
    let n = String::from_utf8_lossy(new);
    unified(path, path, &o, &n)
}

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Delete,
    Insert,
    Equal,
}

struct ULine<'a> {
    kind: OpKind,
    content: &'a str,
}

struct UHunk<'a> {
    from_line: i64,
    to_line: i64,
    lines: Vec<ULine<'a>>,
}

/// unified formats old→new exactly like go-udiff.Unified: "--- from\n+++
/// to\n" headers, hunks with 3 context lines merged within a 6-line gap,
/// counts omitted when 1, GNU "-0,0"/"+0,0" for empty sides, and "\ No
/// newline at end of file" markers.
pub fn unified(old_label: &str, new_label: &str, old: &str, new: &str) -> String {
    const CONTEXT: i64 = 3;
    let diff = TextDiff::from_lines(old, new);

    // Line-level edit regions: (old_start, old_end, inserted lines).
    struct Change<'a> {
        start: usize,
        end: usize,
        inserted: Vec<&'a str>,
    }
    let mut changes: Vec<Change> = Vec::new();
    for op in diff.ops() {
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Delete | DiffTag::Replace => {
                // Delete and Replace consume an old range; Replace's and
                // Insert's new lines ride along as inserts.
                let mut inserted: Vec<&str> = Vec::new();
                for change in diff.iter_changes(op) {
                    if change.tag() == ChangeTag::Insert {
                        inserted.push(change.value());
                    }
                }
                changes.push(Change {
                    start: op.old_range().start,
                    end: op.old_range().end,
                    inserted,
                });
            }
            DiffTag::Insert => {
                let mut inserted: Vec<&str> = Vec::new();
                for change in diff.iter_changes(op) {
                    if change.tag() == ChangeTag::Insert {
                        inserted.push(change.value());
                    }
                }
                changes.push(Change {
                    start: op.old_range().start,
                    end: op.old_range().end,
                    inserted,
                });
            }
        }
    }

    // Go counts the EOF as an implicit newline when computing spans; with
    // line-aligned ops from `similar` this only matters for trailing
    // inserts after an unterminated last line, which fall out naturally.

    let old_lines: Vec<&str> = split_keep_newlines(old);
    let gap = CONTEXT * 2;

    let mut hunks: Vec<UHunk> = Vec::new();
    let mut cur: Option<UHunk> = None;
    let mut last: i64 = 0;
    let mut to_line: i64 = 0;

    for ch in &changes {
        let start = ch.start as i64;
        let end = ch.end as i64;
        match cur.take() {
            Some(h) if start == last => cur = Some(h),
            Some(mut h) if start <= last + gap => {
                add_equal(&old_lines, &mut h, last, start);
                cur = Some(h);
            }
            Some(mut h) => {
                add_equal(&old_lines, &mut h, last, last + CONTEXT);
                hunks.push(h);
                to_line += start - last;
                let mut h = UHunk {
                    from_line: start + 1,
                    to_line: to_line + 1,
                    lines: Vec::new(),
                };
                let delta = add_equal(&old_lines, &mut h, start - CONTEXT, start);
                h.from_line -= delta;
                h.to_line -= delta;
                cur = Some(h);
            }
            None => {
                to_line += start - last;
                let mut h = UHunk {
                    from_line: start + 1,
                    to_line: to_line + 1,
                    lines: Vec::new(),
                };
                let delta = add_equal(&old_lines, &mut h, start - CONTEXT, start);
                h.from_line -= delta;
                h.to_line -= delta;
                cur = Some(h);
            }
        }
        let h = cur.as_mut().unwrap();
        last = start;
        for i in start..end {
            h.lines.push(ULine {
                kind: OpKind::Delete,
                content: old_lines[i as usize],
            });
            last += 1;
        }
        for content in &ch.inserted {
            h.lines.push(ULine {
                kind: OpKind::Insert,
                content,
            });
            to_line += 1;
        }
    }
    if let Some(mut h) = cur.take() {
        add_equal(&old_lines, &mut h, last, last + CONTEXT);
        hunks.push(h);
    }

    if hunks.is_empty() {
        return String::new();
    }

    use std::fmt::Write as _;
    let mut b = String::new();
    let _ = write!(b, "--- {}\n+++ {}\n", old_label, new_label);
    for hunk in &hunks {
        let mut from_count = 0i64;
        let mut to_count = 0i64;
        for l in &hunk.lines {
            match l.kind {
                OpKind::Delete => from_count += 1,
                OpKind::Insert => to_count += 1,
                OpKind::Equal => {
                    from_count += 1;
                    to_count += 1;
                }
            }
        }
        b.push_str("@@");
        if from_count > 1 {
            let _ = write!(b, " -{},{}", hunk.from_line, from_count);
        } else if hunk.from_line == 1 && from_count == 0 {
            // Match odd GNU diff -u behavior adding to empty file.
            b.push_str(" -0,0");
        } else {
            let _ = write!(b, " -{}", hunk.from_line);
        }
        if to_count > 1 {
            let _ = write!(b, " +{},{}", hunk.to_line, to_count);
        } else if hunk.to_line == 1 && to_count == 0 {
            b.push_str(" +0,0");
        } else {
            let _ = write!(b, " +{}", hunk.to_line);
        }
        b.push_str(" @@\n");
        for l in &hunk.lines {
            match l.kind {
                OpKind::Delete => b.push('-'),
                OpKind::Insert => b.push('+'),
                OpKind::Equal => b.push(' '),
            }
            b.push_str(l.content);
            if !l.content.ends_with('\n') {
                b.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    b
}

/// Splits s into lines keeping their trailing newline, like go-udiff's
/// splitLines.
fn split_keep_newlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            lines.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

fn add_equal<'a>(lines: &[&'a str], h: &mut UHunk<'a>, start: i64, end: i64) -> i64 {
    let mut delta = 0i64;
    let mut i = start;
    while i < end {
        if i < 0 {
            i += 1;
            continue;
        }
        if i as usize >= lines.len() {
            return delta;
        }
        h.lines.push(ULine {
            kind: OpKind::Equal,
            content: lines[i as usize],
        });
        delta += 1;
        i += 1;
    }
    delta
}

// ---------------------------------------------------------------------------
// The colorizer (port of highlight.go renderDiff).
// ---------------------------------------------------------------------------

/// One parsed side-by-side row of a unified diff.
#[derive(Debug, Clone, Default)]
pub struct DiffSideLine {
    /// ' ', '+', '-', '@'
    pub kind: u8,
    pub code: String,
    pub old_idx: usize,
    pub new_idx: usize,
    pub hunk: String,
    pub has_old: bool,
    pub has_new: bool,
}

pub fn skip_diff_meta(line: &str) -> bool {
    if line == r"\ No newline at end of file" {
        return true;
    }
    if line == "---" || line == "+++" {
        return true;
    }
    line.starts_with("--- ") || line.starts_with("+++ ")
}

/// parseUnifiedDiff splits a unified diff into reconstructed old/new
/// sources plus ordered rows for rendering.
pub fn parse_unified_diff(diff: &str) -> (String, String, Vec<DiffSideLine>) {
    let trimmed = diff.strip_suffix('\n').unwrap_or(diff);
    let mut rows: Vec<DiffSideLine> = Vec::new();
    let mut old_parts: Vec<&str> = Vec::new();
    let mut new_parts: Vec<&str> = Vec::new();
    for line in trimmed.split('\n') {
        if skip_diff_meta(line) {
            continue;
        }
        if line.starts_with("@@") {
            rows.push(DiffSideLine {
                kind: b'@',
                hunk: line.to_string(),
                ..Default::default()
            });
            continue;
        }
        if line.is_empty() {
            // Blank rows contribute to both sources but carry no flags,
            // matching Go's diffSideLine zero values.
            rows.push(DiffSideLine {
                kind: b' ',
                code: String::new(),
                ..Default::default()
            });
            old_parts.push("");
            new_parts.push("");
            continue;
        }
        match line.as_bytes()[0] {
            b'-' => {
                let code = &line[1..];
                rows.push(DiffSideLine {
                    kind: b'-',
                    code: code.to_string(),
                    old_idx: old_parts.len(),
                    has_old: true,
                    ..Default::default()
                });
                old_parts.push(code);
            }
            b'+' => {
                let code = &line[1..];
                rows.push(DiffSideLine {
                    kind: b'+',
                    code: code.to_string(),
                    new_idx: new_parts.len(),
                    has_new: true,
                    ..Default::default()
                });
                new_parts.push(code);
            }
            _ => {
                let code = line.strip_prefix(' ').unwrap_or(line);
                rows.push(DiffSideLine {
                    kind: b' ',
                    code: code.to_string(),
                    old_idx: old_parts.len(),
                    new_idx: new_parts.len(),
                    has_old: true,
                    has_new: true,
                    ..Default::default()
                });
                old_parts.push(code);
                new_parts.push(code);
            }
        }
    }
    (old_parts.join("\n"), new_parts.join("\n"), rows)
}

fn paint_wrapped_diff_line(gutter: u8, code_ansi: &str, width: usize, bg: &str) -> String {
    let width = width.max(1);
    let code_w = width.saturating_sub(1).max(1);
    let mut sb = String::new();
    let mut first = true;
    for seg in ansi_wrap(code_ansi, code_w).split('\n') {
        let g = if first { gutter } else { b' ' };
        first = false;
        sb.push_str(&ansi_bg(bg));
        sb.push_str(&ansi_fg(COLOR_MUTED));
        sb.push(g as char);
        sb.push_str(seg);
        sb.push_str(&ansi_bg(COLOR_CARD_DARK));
        sb.push('\n');
    }
    sb
}

/// renderDiff colorizes a unified diff with syntax highlighting on each
/// file side. changeBg applies GitHub-style add/del backgrounds
/// (edit_file). write_file passes changeBg=false: same gutters and
/// tokens, no wash.
pub fn render_diff(diff: &str, filename: &str, width: usize, change_bg: bool) -> String {
    if diff.is_empty() {
        return String::new();
    }
    let (old_src, new_src, rows) = parse_unified_diff(diff);
    let mut old_n = 0usize;
    let mut new_n = 0usize;
    for r in &rows {
        if r.has_old {
            old_n += 1;
        }
        if r.has_new {
            new_n += 1;
        }
    }
    let old_hl = pad_highlight_lines(highlight_document(filename, &old_src), old_n);
    let new_hl = pad_highlight_lines(highlight_document(filename, &new_src), new_n);

    let width = width.max(1);
    let mut sb = String::new();
    for r in &rows {
        match r.kind {
            b'@' => {}
            b'-' => {
                let code = if r.has_old && r.old_idx < old_hl.len() {
                    old_hl[r.old_idx].clone()
                } else {
                    String::new()
                };
                let bg = if change_bg {
                    COLOR_DIFF_DEL_BG
                } else {
                    COLOR_CARD_DARK
                };
                sb.push_str(&paint_wrapped_diff_line(b'-', &code, width, bg));
            }
            b'+' => {
                let code = if r.has_new && r.new_idx < new_hl.len() {
                    new_hl[r.new_idx].clone()
                } else {
                    String::new()
                };
                let bg = if change_bg {
                    COLOR_DIFF_ADD_BG
                } else {
                    COLOR_CARD_DARK
                };
                sb.push_str(&paint_wrapped_diff_line(b'+', &code, width, bg));
            }
            _ => {
                let code = if r.has_new && r.new_idx < new_hl.len() {
                    new_hl[r.new_idx].clone()
                } else if r.has_old && r.old_idx < old_hl.len() {
                    old_hl[r.old_idx].clone()
                } else {
                    String::new()
                };
                sb.push_str(&paint_wrapped_diff_line(
                    b' ',
                    &code,
                    width,
                    COLOR_CARD_DARK,
                ));
            }
        }
    }
    sb
}

// ---------------------------------------------------------------------------
// main.go helpers.
// ---------------------------------------------------------------------------

/// fileLineWindow returns up to limit lines of content starting at the
/// 0-based offset. A limit of 0 or less defaults to 1000. A negative
/// offset is treated as 0. If offset is past the last line, the result
/// is empty.
pub fn file_line_window(content: &str, offset: i64, limit: i64) -> String {
    if offset < 0 {
        return file_line_window(content, 0, limit);
    }
    let limit = if limit <= 0 {
        DEFAULT_READ_FILE_LIMIT
    } else {
        limit
    };
    if content.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let offset = offset as usize;
    if offset >= lines.len() {
        return String::new();
    }
    let end = (offset as i64 + limit).min(lines.len() as i64) as usize;
    lines[offset..end].concat()
}

/// sniffImageMIME detects the MIME type of an image file from its magic
/// bytes. Returns "" when the data isn't a recognizable image.
pub fn sniff_image_mime(data: &[u8]) -> &'static str {
    if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        "image/png"
    } else if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        "image/jpeg"
    } else if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        "image/gif"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.len() >= 2 && &data[..2] == b"BM" {
        "image/bmp"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::colors::{
        ansi_bg, ansi_fg, COLOR_DIFF_ADD, COLOR_DIFF_ADD_BG, COLOR_DIFF_DEL, COLOR_DIFF_DEL_BG,
        COLOR_MUTED,
    };

    #[test]
    fn file_diff_empty_when_equal() {
        assert_eq!(file_diff("x", b"a\n", b"a\n"), "");
        assert_eq!(file_diff("x", b"", b""), "");
    }

    #[test]
    fn file_diff_new_file_matches_go_shape() {
        let d = file_diff("main.go", b"", b"package main\n\nfunc main() {}\n");
        assert!(d.contains("+package main"), "{}", d);
        assert!(d.contains("--- main.go"));
        assert!(d.contains("+++ main.go"));
        assert!(
            d.contains("@@ -0,0 +1,3 @@\n+package main\n+\n+func main() {}\n"),
            "{}",
            d
        );
    }

    #[test]
    fn file_diff_edit_shows_both_sides() {
        let d = file_diff("a.txt", b"hello world\n", b"goodbye world\n");
        assert!(d.contains("-hello world\n"), "{}", d);
        assert!(d.contains("+goodbye world\n"), "{}", d);
        assert!(d.contains("@@ -1 +1 @@"), "{}", d);
    }

    #[test]
    fn file_diff_no_trailing_newline_marker() {
        let d = file_diff("a.txt", b"x", b"x\n");
        assert!(d.contains("\\ No newline at end of file"), "{}", d);
        assert!(d.contains("-x") && d.contains("+x\n"), "{}", d);
    }

    #[test]
    fn file_diff_two_far_edits_make_two_hunks() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..20 {
            old.push_str(&format!("line{}\n", i));
            new.push_str(
                if i == 2 || i == 15 {
                    format!("CHANGED{}\n", i)
                } else {
                    format!("line{}\n", i)
                }
                .as_str(),
            );
        }
        let d = file_diff("f.txt", old.as_bytes(), new.as_bytes());
        assert_eq!(d.matches("@@").count(), 4); // 2 per hunk header
    }

    #[test]
    fn nearby_edits_merge_into_one_hunk() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..10 {
            old.push_str(&format!("line{}\n", i));
            new.push_str(
                if i == 2 || i == 5 {
                    format!("CHANGED{}\n", i)
                } else {
                    format!("line{}\n", i)
                }
                .as_str(),
            );
        }
        let d = file_diff("f.txt", old.as_bytes(), new.as_bytes());
        assert_eq!(d.matches("@@").count(), 2);
    }

    #[test]
    fn parse_unified_round_trip() {
        let diff = "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n context\n-hello\n+world\n";
        let (old_src, new_src, rows) = parse_unified_diff(diff);
        assert_eq!(old_src, "context\nhello");
        assert_eq!(new_src, "context\nworld");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, b'@');
        assert_eq!(rows[1].kind, b' ');
        assert_eq!(rows[2].kind, b'-');
        assert_eq!(rows[3].kind, b'+');
    }

    #[test]
    fn skip_meta_variants() {
        assert!(skip_diff_meta("--- a.txt"));
        assert!(skip_diff_meta("+++ b.txt"));
        assert!(skip_diff_meta("---"));
        assert!(skip_diff_meta(r"\ No newline at end of file"));
        assert!(!skip_diff_meta("--normal"));
        assert!(!skip_diff_meta("context"));
    }

    // Ported TestRenderDiffColors.
    #[test]
    fn render_diff_colors() {
        let diff = "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n context\n-hello\n+world\n";
        let rendered = render_diff(diff, "a.txt", 80, true);
        assert!(
            rendered.contains(&ansi_bg(COLOR_DIFF_ADD_BG)),
            "{:?}",
            rendered
        );
        assert!(rendered.contains(&ansi_bg(COLOR_DIFF_DEL_BG)));
        assert!(
            !rendered.contains(&ansi_fg(COLOR_DIFF_ADD))
                && !rendered.contains(&ansi_fg(COLOR_DIFF_DEL)),
            "code text should not be forced to add/del foreground"
        );
        assert!(
            !rendered.contains("---") && !rendered.contains("+++"),
            "file headers should be omitted:\n{:?}",
            rendered
        );
        assert!(!rendered.contains("@@ -1 +1 @@"));
        assert!(
            rendered.contains(&ansi_fg(COLOR_MUTED)),
            "diff gutters should be dim"
        );
        assert!(rendered.contains("context"));

        let plain = render_diff(diff, "a.txt", 80, false);
        assert!(
            !plain.contains(&ansi_bg(COLOR_DIFF_ADD_BG))
                && !plain.contains(&ansi_bg(COLOR_DIFF_DEL_BG)),
            "changeBg=false should omit add/del backgrounds"
        );
    }

    // Ported cases from read_file_test.go (fileLineWindow).
    #[test]
    fn line_window_cases() {
        let content = "a\nb\nc\nd\ne\n";
        assert_eq!(file_line_window(content, 0, 2), "a\nb\n");
        assert_eq!(file_line_window(content, 2, 2), "c\nd\n");
        assert_eq!(file_line_window(content, 4, 10), "e\n");
        assert_eq!(file_line_window(content, 10, 2), "");
        assert_eq!(file_line_window(content, -3, 1), "a\n");
        assert_eq!(file_line_window(content, 0, 0), content);
    }

    #[test]
    fn line_window_no_trailing_newline() {
        let content = "a\nbb";
        assert_eq!(file_line_window(content, 1, 5), "bb");
        assert_eq!(file_line_window("", 0, 10), "");
    }

    #[test]
    fn sniff_mime_magic_bytes() {
        assert_eq!(sniff_image_mime(b"\x89PNG\r\n\x1a\nxxxx"), "image/png");
        assert_eq!(sniff_image_mime(b"\xFF\xD8\xFFxx"), "image/jpeg");
        assert_eq!(sniff_image_mime(b"GIF89axxxx"), "image/gif");
        assert_eq!(sniff_image_mime(b"GIF87axxxx"), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFFxxxxWEBP"), "image/webp");
        assert_eq!(sniff_image_mime(b"BMxx"), "image/bmp");
        assert_eq!(sniff_image_mime(b"nope"), "");
        assert_eq!(sniff_image_mime(b""), "");
    }

    /// Golden outputs captured from go-udiff v0.4.1 (udiff.Unified), the
    /// library the Go build uses, to keep the port byte-compatible.
    #[test]
    fn unified_goldens_match_go_udiff() {
        let cases: Vec<(&str, &str, &str, &str)> = vec![
            (
                "create",
                "",
                "package main\n\nfunc main() {}\n",
                "--- main.go\n+++ main.go\n@@ -0,0 +1,3 @@\n+package main\n+\n+func main() {}\n",
            ),
            (
                "edit",
                "hello world\n",
                "goodbye world\n",
                "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-hello world\n+goodbye world\n",
            ),
            (
                "noeol",
                "x",
                "x\n",
                "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+x\n",
            ),
            (
                "twohunks",
                "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\n",
                "l0\nl1\nC2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\nl13\nl14\nC15\nl16\nl17\nl18\nl19\n",
                "--- f.txt\n+++ f.txt\n@@ -1,6 +1,6 @@\n l0\n l1\n-l2\n+C2\n l3\n l4\n l5\n@@ -13,7 +13,7 @@\n l12\n l13\n l14\n-l15\n+C15\n l16\n l17\n l18\n",
            ),
            (
                "mergehunks",
                "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n",
                "a\nB\nc\nd\nE\nf\ng\nh\ni\nj\n",
                "--- g.txt\n+++ g.txt\n@@ -1,8 +1,8 @@\n a\n-b\n+B\n c\n d\n-e\n+E\n f\n g\n h\n",
            ),
            (
                "deletefile",
                "one\ntwo\nthree\n",
                "",
                "--- d.txt\n+++ d.txt\n@@ -1,3 +0,0 @@\n-one\n-two\n-three\n",
            ),
            (
                "append",
                "keep\n",
                "keep\nmore1\nmore2\n",
                "--- e.txt\n+++ e.txt\n@@ -1 +1,3 @@\n keep\n+more1\n+more2\n",
            ),
            (
                "middle",
                "aa\nbb\ncc\ndd\nee\n",
                "aa\nbb\nXX\ndd\nee\n",
                "--- m.txt\n+++ m.txt\n@@ -1,5 +1,5 @@\n aa\n bb\n-cc\n+XX\n dd\n ee\n",
            ),
        ];
        for (i, (_, old, new, want)) in cases.iter().enumerate() {
            let labels = [
                ("main.go", "main.go"),
                ("a.txt", "a.txt"),
                ("a.txt", "a.txt"),
                ("f.txt", "f.txt"),
                ("g.txt", "g.txt"),
                ("d.txt", "d.txt"),
                ("e.txt", "e.txt"),
                ("m.txt", "m.txt"),
            ][i];
            assert_eq!(unified(labels.0, labels.1, old, new), *want, "case {}", i);
        }
    }
}
