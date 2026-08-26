//! statusbar.rs renders the one-line (or two-line on narrow terminals)
//! status bar: model + thinking level + token usage + transient status.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ansi;
use crate::app::App;

/// Legacy cutoff retained for API compatibility; wrapping is now content-based.
pub const USAGE_WRAP_WIDTH: u16 = 50;

/// formatWindowTokens renders a context-window size: "128K", "1M", or
/// the same compact form as formatTokens.
pub fn format_window_tokens(n: i64) -> String {
    if n >= 1_000_000 && n % 1_000_000 == 0 {
        return format!("{}M", n / 1_000_000);
    }
    if n >= 1_000 && n % 1_000 == 0 {
        return format!("{}K", n / 1_000);
    }
    atom_core::util::format_tokens(n)
}

pub fn status_bar_rows(app: &App) -> usize {
    status_bar_layout(app).len()
}

pub fn wrap_status_bar(app: &App) -> bool {
    status_bar_rows(app) == 2
}

/// statusSuffix: working message, fresh copy flash, then error text.
pub fn status_suffix_spans(app: &App) -> Vec<Span<'static>> {
    status_suffix(app)
        .map(|(text, style)| vec![Span::styled(format!("  {text}"), style)])
        .unwrap_or_default()
}

fn status_suffix(app: &App) -> Option<(String, Style)> {
    if !app.working_msg.is_empty() {
        return Some((app.working_msg.clone(), ansi::style_dim()));
    }
    if !app.copied_msg.is_empty()
        && app
            .copied_at
            .map(|t| t.elapsed() < std::time::Duration::from_secs(4))
            .unwrap_or(false)
    {
        return Some((app.copied_msg.clone(), ansi::style_primary()));
    }
    if !app.err_msg.is_empty() {
        return Some((app.err_msg.clone(), ansi::style_dim()));
    }
    None
}

/// usageString renders the session's token usage for the status bar as a
/// single context meter: "243.5K (61%)" — the current context size and
/// the fraction of the model's context window it fills. Input, output,
/// and cache counters are no longer shown. Pure-text variant used by
/// tests; the spans version wraps it in dim style.
pub fn usage_string(app: &App, _inner_width: usize) -> String {
    let Some(u) = &app.session.usage else {
        return String::new();
    };
    if u.total_tokens <= 0 {
        return String::new();
    }
    let w = atom_core::providers::modelsdev::context_window_tokens(
        &app.sel_provider.name,
        &app.sel_model,
    );
    let ctx = atom_core::util::format_tokens(u.total_tokens);
    if w > 0 {
        // Round to the nearest percent so 243.5K in a 400K window reads 61%.
        // Floor at 1% so any non-empty context never reads 0%.
        let pct = ((u.total_tokens * 100 + w / 2) / w).max(1);
        format!("{ctx} ({pct}%)")
    } else {
        ctx
    }
}

fn text_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn truncate_width(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let mut out = String::new();
    let budget = width.saturating_sub(1);
    let mut used = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > budget {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out.push('…');
    out
}

fn status_head(app: &App) -> String {
    let lvl = app.thinking_level();
    if lvl.is_empty() {
        app.sel_model.clone()
    } else {
        format!("{} ({})", app.sel_model, lvl)
    }
}

fn fitted_status_head(app: &App, width: usize, head: &str) -> String {
    if text_width(head) <= width {
        return head.to_string();
    }
    if !app.sel_model.is_empty() {
        return truncate_width(&app.sel_model, width);
    }
    truncate_width(head, width)
}

fn usage_variants(app: &App, width: usize) -> Vec<String> {
    let candidate = usage_string(app, width);
    if candidate.is_empty() {
        Vec::new()
    } else {
        vec![candidate]
    }
}

/// Footer-menu and parent/child navigation hints. These used to occupy a
/// dedicated row above the prompt; they now share the status bar with the
/// context meter, wrapping across lines when the terminal is too narrow.
/// Returns the hint phrases as styled segments (each a short dim phrase);
/// empty when there is nothing to hint.
pub fn nav_segments(app: &App) -> Vec<Vec<Span<'static>>> {
    use crate::overlays::PickerKind;
    let mut segs: Vec<Vec<Span<'static>>> = Vec::new();
    if app.manage_visible
        || !matches!(app.picker_kind, PickerKind::None)
        || app.context_visible
        || app.reasoning_visible
    {
        segs.push(vec![Span::styled("esc to close", ansi::style_dim())]);
    } else {
        let n = app.manage_agents.len();
        if n > 0 {
            let label = if n == 1 {
                "(1 subagent) Shift ↓".to_string()
            } else {
                format!("({n} subagents) Shift ↓")
            };
            segs.push(vec![Span::styled(label, ansi::style_dim())]);
        }
    }
    if !app.session.parent_id.is_empty() {
        segs.push(vec![Span::styled("Shift ↑ to return", ansi::style_dim())]);
    }
    segs
}

/// Cell width of the nav phrases joined by their internal " / "
/// separators (the leading gap before the whole block is added by the
/// caller, so this counts only the separators between phrases).
fn nav_width(segments: &[Vec<Span<'static>>]) -> usize {
    let mut w = 0;
    for (i, seg) in segments.iter().enumerate() {
        w += ansi::line_width(&Line::from(seg.clone()));
        if i + 1 < segments.len() {
            w += 3; // " / "
        }
    }
    w
}

/// Flattens the nav phrases into one span list, joining segments with
/// the styled " / " separator (for placement on a single line).
fn nav_spans(segments: &[Vec<Span<'static>>]) -> Vec<Span<'static>> {
    let sep = Span::styled(" / ", ansi::style_prompt_border());
    let mut spans = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(sep.clone());
        }
        spans.extend(seg.iter().cloned());
    }
    spans
}

/// Truncates a single-span phrase to `width` cells, appending an
/// ellipsis; multi-span phrases are returned whole (assumed to fit).
fn truncate_phrase(seg: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    if seg.len() == 1 {
        vec![Span::styled(
            truncate_width(&seg[0].content, width),
            seg[0].style,
        )]
    } else {
        seg.to_vec()
    }
}

/// Lays out the nav phrases across lines, wrapping at phrase boundaries
/// (and dropping the leading separator on wrapped lines) so the hints
/// never overflow the row. A phrase wider than the row is truncated.
/// `prefix` (e.g. the context meter) starts the first line so the hints
/// share its row; it is flushed to its own line when it alone fills the
/// row.
fn wrap_nav(
    segments: &[Vec<Span<'static>>],
    prefix: Option<(Vec<Span<'static>>, usize)>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    if let Some((p_spans, p_w)) = prefix {
        if p_w >= width {
            lines.push(Line::from(p_spans));
        } else {
            cur = p_spans;
            cur_w = p_w;
        }
    }

    for (i, seg) in segments.iter().enumerate() {
        let seg_w = ansi::line_width(&Line::from(seg.clone()));
        // A dim "  " opens the hint block (so it reads as part of the
        // status line); the styled " / " separates later phrases.
        let (sep, sep_w) = if i == 0 {
            (vec![Span::styled("  ", ansi::style_dim())], 2)
        } else {
            (vec![Span::styled(" / ", ansi::style_prompt_border())], 3)
        };
        if cur.is_empty() {
            let placed = if seg_w > width {
                truncate_phrase(seg, width)
            } else {
                seg.to_vec()
            };
            cur = placed;
            cur_w = ansi::line_width(&Line::from(cur.clone()));
        } else if cur_w + sep_w + seg_w <= width {
            cur.extend(sep);
            cur.extend(seg.iter().cloned());
            cur_w += sep_w + seg_w;
        } else {
            lines.push(std::mem::take(&mut cur).into());
            let placed = if seg_w > width {
                truncate_phrase(seg, width)
            } else {
                seg.to_vec()
            };
            cur = placed;
            cur_w = ansi::line_width(&Line::from(cur.clone()));
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

/// Appends the transient suffix after the laid-out tail: onto the last
/// line when it fits, otherwise on its own (truncated) line.
fn append_suffix(lines: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize) {
    let sw = text_width(text);
    if let Some(last) = lines.last_mut() {
        let last_w = ansi::line_width(last);
        if last_w + 2 + sw <= width {
            last.spans.push(Span::styled(format!("  {text}"), style));
            return;
        }
    }
    lines.push(Line::from(Span::styled(truncate_width(text, width), style)));
}

fn one_line(head: String, usage: Option<String>, suffix: Option<(String, Style)>) -> Line<'static> {
    let mut spans = vec![Span::styled(head, ansi::style_dim())];
    if let Some(usage) = usage {
        spans.push(Span::styled(format!("  {usage}"), ansi::style_dim()));
    }
    if let Some((suffix, style)) = suffix {
        spans.push(Span::styled(format!("  {suffix}"), style));
    }
    Line::from(spans)
}

fn secondary_line(
    usage_variants: &[String],
    suffix: Option<&(String, Style)>,
    width: usize,
) -> Line<'static> {
    let Some((suffix, suffix_style)) = suffix else {
        let usage = usage_variants
            .iter()
            .find(|usage| text_width(usage) <= width)
            .cloned()
            .or_else(|| {
                usage_variants
                    .last()
                    .map(|usage| truncate_width(usage, width))
            })
            .unwrap_or_default();
        return Line::from(Span::styled(usage, ansi::style_dim()));
    };

    if text_width(suffix) >= width {
        return Line::from(Span::styled(truncate_width(suffix, width), *suffix_style));
    }

    let usage_budget = width.saturating_sub(text_width(suffix) + 2);
    let usage = usage_variants
        .iter()
        .find(|usage| text_width(usage) <= usage_budget)
        .cloned()
        .or_else(|| {
            if usage_budget >= 4 {
                usage_variants
                    .last()
                    .map(|usage| truncate_width(usage, usage_budget))
            } else {
                None
            }
        });

    if let Some(usage) = usage {
        Line::from(vec![
            Span::styled(usage, ansi::style_dim()),
            Span::styled(format!("  {suffix}"), *suffix_style),
        ])
    } else {
        Line::from(Span::styled(suffix.clone(), *suffix_style))
    }
}

/// Builds a single status line from all four segments (model head,
/// context meter, nav hints, transient suffix), joined by dim gaps. The
/// nav hints already carry their own internal " / " styling.
fn one_line_full(
    head: String,
    usage: Option<String>,
    nav: Option<Vec<Span<'static>>>,
    suffix: Option<(String, Style)>,
) -> Line<'static> {
    let mut spans = vec![Span::styled(head, ansi::style_dim())];
    if let Some(usage) = usage {
        spans.push(Span::styled(format!("  {usage}"), ansi::style_dim()));
    }
    if let Some(nav) = nav {
        spans.push(Span::styled("  ", ansi::style_dim()));
        spans.extend(nav);
    }
    if let Some((suffix, style)) = suffix {
        spans.push(Span::styled(format!("  {suffix}"), style));
    }
    Line::from(spans)
}

/// Returns a single-line status bar when every segment fits in `width`.
fn try_one_line(
    width: usize,
    head: &str,
    usage_variants: &[String],
    nav: &[Vec<Span<'static>>],
    suffix: Option<&(String, Style)>,
) -> Option<Line<'static>> {
    let navw = if nav.is_empty() {
        0
    } else {
        2 + nav_width(nav)
    };
    let suffixw = suffix.map(|(text, _)| 2 + text_width(text)).unwrap_or(0);
    let headw = text_width(head);
    let nav_spans_opt = if nav.is_empty() {
        None
    } else {
        Some(nav_spans(nav))
    };
    if usage_variants.is_empty() {
        if headw + navw + suffixw <= width {
            return Some(one_line_full(
                head.to_string(),
                None,
                nav_spans_opt,
                suffix.cloned(),
            ));
        }
    } else {
        let usage = &usage_variants[0];
        if headw + 2 + text_width(usage) + navw + suffixw <= width {
            return Some(one_line_full(
                head.to_string(),
                Some(usage.clone()),
                nav_spans_opt,
                suffix.cloned(),
            ));
        }
    }
    None
}

/// Lays out the segments that follow the model head (context meter, nav
/// hints, transient suffix) across one or more lines. With no nav hints
/// this is the legacy two-segment tail so narrow terminals still drop the
/// meter behind a long transient message; with nav hints the hints share
/// the meter's row and wrap to further rows instead of overflowing.
fn layout_tail(
    usage_variants: &[String],
    nav: &[Vec<Span<'static>>],
    suffix: Option<&(String, Style)>,
    width: usize,
) -> Vec<Line<'static>> {
    if nav.is_empty() {
        return vec![secondary_line(usage_variants, suffix, width)];
    }

    // The context meter prefixes the first hint line so the hints share
    // the meter's row; it is flushed to its own line when it fills the row.
    let prefix = usage_variants.first().map(|u| {
        let spans = vec![Span::styled(u.clone(), ansi::style_dim())];
        (spans, text_width(u))
    });
    let mut lines = wrap_nav(nav, prefix, width);
    if let Some((suffix_text, suffix_style)) = suffix {
        append_suffix(&mut lines, suffix_text, *suffix_style, width);
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), ansi::style_dim())));
    }
    lines
}

fn status_bar_layout(app: &App) -> Vec<Line<'static>> {
    let width = app.inner_width();
    let head = status_head(app);
    let usage_variants = usage_variants(app, width);
    let nav = nav_segments(app);
    let suffix = status_suffix(app);

    // Everything fits on one line: model + context meter + hints + status.
    if let Some(line) = try_one_line(width, &head, &usage_variants, &nav, suffix.as_ref()) {
        return vec![line];
    }

    // Otherwise the model gets its own (truncated) line and the meter,
    // hints, and transient status wrap below it.
    let fitted_head = fitted_status_head(app, width, &head);
    if usage_variants.is_empty() && nav.is_empty() && suffix.is_none() {
        return vec![one_line(fitted_head, None, None)];
    }

    let tail = layout_tail(&usage_variants, &nav, suffix.as_ref(), width);
    let tail_empty = tail
        .iter()
        .all(|line| ansi::line_plain(line).trim().is_empty());
    if fitted_head.is_empty() {
        if tail_empty {
            vec![one_line(fitted_head, None, None)]
        } else {
            tail
        }
    } else if tail_empty {
        vec![one_line(fitted_head, None, None)]
    } else {
        let mut lines = vec![one_line(fitted_head, None, None)];
        lines.extend(tail);
        lines
    }
}

/// renderStatusBar builds the dim status line(s).
pub fn status_bar_lines(app: &App) -> Vec<Line<'static>> {
    status_bar_layout(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atom_core::providers::{
        set_models_dev_catalog_for_test, ModelsDevCatalog, ModelsDevLimit, ModelsDevModel,
        ModelsDevProvider,
    };
    use atom_core::types::StreamUsage;
    use std::collections::HashMap;
    use std::time::Instant;

    fn app_with_usage(u: StreamUsage, width: u16) -> App {
        let mut app = App::new_test(width, 24);
        app.session.model = "gpt-5".into();
        app.sel_model = "gpt-5".into();
        app.session.usage = Some(u);
        app
    }

    /// Installs a single-entry models.dev catalog for one test and restores
    /// the process-wide catalog (None) on drop so other tests keep the
    /// 128K fallback. Use a unique model id so concurrent lookups for
    /// other models are unaffected.
    struct TestCatalogGuard;
    impl TestCatalogGuard {
        fn install(provider: &str, model: &str, context: i64) -> Self {
            let mut cat = ModelsDevCatalog::new();
            cat.insert(
                provider.to_string(),
                ModelsDevProvider {
                    models: HashMap::from([(
                        model.to_string(),
                        ModelsDevModel {
                            limit: ModelsDevLimit { context },
                            ..Default::default()
                        },
                    )]),
                    ..Default::default()
                },
            );
            set_models_dev_catalog_for_test(Some(cat));
            Self
        }
    }
    impl Drop for TestCatalogGuard {
        fn drop(&mut self) {
            set_models_dev_catalog_for_test(None);
        }
    }

    #[test]
    fn usage_context_meter() {
        let u = StreamUsage {
            prompt_tokens: 8400,
            completion_tokens: 100,
            total_tokens: 8500,
            cache_read_tokens: 4200,
            ..Default::default()
        };
        // Only context is shown: tokens then rounded percent of the window.
        // 8500/128000 ≈ 6.6% rounds to 7%.
        let app = app_with_usage(u, 120);
        assert_eq!(usage_string(&app, 120), "8.5K (7%)");
    }

    #[test]
    fn usage_context_meter_matches_example() {
        // The canonical example: 243.5K in a 400K window reads "243.5K (61%)".
        // Install a throwaway catalog entry for a unique model id so the
        // 400K window resolves; reset it on exit so other tests keep the
        // 128K fallback. The unique id avoids cross-test interference.
        let _reset = TestCatalogGuard::install("openai", "ctx-example-400k", 400_000);
        let u = StreamUsage {
            total_tokens: 243_500,
            ..Default::default()
        };
        let mut app = App::new_test(120, 24);
        app.sel_model = "ctx-example-400k".into();
        app.sel_provider.name = "openai".into();
        app.session.usage = Some(u);
        assert_eq!(usage_string(&app, 120), "243.5K (61%)");
    }

    #[test]
    fn usage_context_meter_ignores_width() {
        let u = StreamUsage {
            prompt_tokens: 12345,
            completion_tokens: 678,
            total_tokens: 13023,
            cache_read_tokens: 9000,
            cache_write_tokens: 2100,
            prompt_tokens_all: 120000,
            ..Default::default()
        };
        let app = app_with_usage(u.clone(), 100);
        // 13023/128000 ≈ 10.2% rounds to 10%; width no longer changes the form.
        assert_eq!(usage_string(&app, 89), "13K (10%)");
        assert_eq!(usage_string(&app, 69), "13K (10%)");
    }

    #[test]
    fn usage_context_only_without_other_counters() {
        let u = StreamUsage {
            total_tokens: 500,
            ..Default::default()
        };
        let app = app_with_usage(u, 120);
        // Context always carries the catalog-window percentage (128K fallback),
        // floored at 1% so a non-empty context never reads 0%.
        assert_eq!(usage_string(&app, 120), "500 (1%)");
    }

    #[test]
    fn empty_when_no_usage() {
        let app = App::new_test(120, 30);
        assert_eq!(usage_string(&app, 120), "");
    }

    #[test]
    fn row_count_depends_on_content_width() {
        let mut app = App::new_test(48, 24);
        app.sel_model = "m".into();
        assert_eq!(status_bar_rows(&app), 1);
        assert_eq!(status_bar_lines(&app).len(), 1);
        assert!(!wrap_status_bar(&app));

        app.width = 12; // inner width 10: "gpt-5  err" fits exactly.
        app.sel_model = "gpt-5".into();
        app.err_msg = "err".into();
        assert_eq!(status_bar_rows(&app), 1);
        assert_eq!(ansi::line_width(&status_bar_lines(&app)[0]), 10);

        app.width = 11;
        assert_eq!(status_bar_rows(&app), 2);
        assert_eq!(status_bar_lines(&app).len(), 2);
        assert!(wrap_status_bar(&app));
    }

    #[test]
    fn thinking_level_in_head() {
        let mut app = App::new_test(90, 24);
        app.sel_model = "deepseek-v4".into();
        app.thinking_levels = vec!["none".to_string(), "high".to_string()];
        app.thinking_idx = 1;
        let line = &status_bar_lines(&app)[0];
        let txt = ansi::line_plain(line);
        assert_eq!(txt, "deepseek-v4 (high)");
    }

    #[test]
    fn status_head_only_describes_selected_model() {
        let mut app = App::new_test(90, 24);
        app.sel_model = "GPT-5.6 Sol".into();

        let txt = ansi::line_plain(&status_bar_lines(&app)[0]);

        assert_eq!(txt, "GPT-5.6 Sol");
    }

    #[test]
    fn error_suffix_after_usage() {
        let mut app = App::new_test(90, 24);
        app.sel_model = "m".into();
        app.err_msg = "boom".into();
        let txt = ansi::line_plain(&status_bar_lines(&app)[0]);
        assert_eq!(txt, "m  boom");
    }

    #[test]
    fn copied_flash_primary_for_four_seconds() {
        let mut app = App::new_test(90, 24);
        app.sel_model = "m".into();
        app.copied_msg = "Copied 5 chars".into();
        app.copied_at = Some(Instant::now());
        let line = &status_bar_lines(&app)[0];
        assert!(ansi::line_plain(line).contains("Copied 5 chars"));
        assert_eq!(line.spans.last().unwrap().style, ansi::style_primary());
        app.copied_at = Some(Instant::now() - std::time::Duration::from_secs(5));
        app.err_msg = "err".into();
        let txt = ansi::line_plain(&status_bar_lines(&app)[0]);
        assert!(txt.ends_with("err"));
    }

    #[test]
    fn long_unicode_model_is_truncated_to_inner_width() {
        let mut app = App::new_test(12, 24);
        app.sel_model = "模型模型模型".into();
        let lines = status_bar_lines(&app);
        assert_eq!(lines.len(), 1);
        assert!(ansi::line_plain(&lines[0]).starts_with("模型"));
        assert!(ansi::line_plain(&lines[0]).ends_with('…'));
        assert!(ansi::line_width(&lines[0]) <= app.inner_width());
    }

    #[test]
    fn long_model_and_suffix_are_bounded_and_recognizable() {
        let mut app = App::new_test(18, 24);
        app.sel_model = "模型-model-identity-very-long".into();
        app.err_msg = "error: a very long transient failure message".into();
        let lines = status_bar_lines(&app);
        assert_eq!(status_bar_rows(&app), 2);
        assert_eq!(lines.len(), 2);
        assert!(ansi::line_plain(&lines[0]).starts_with("模型-model"));
        assert!(ansi::line_plain(&lines[1]).starts_with("error:"));
        assert!(lines
            .iter()
            .all(|line| ansi::line_width(line) <= app.inner_width()));
        assert_eq!(lines[1].spans.last().unwrap().style, ansi::style_dim());
    }

    #[test]
    fn suffix_wins_space_over_usage() {
        let u = StreamUsage {
            prompt_tokens: 8400,
            completion_tokens: 100,
            total_tokens: 8500,
            ..Default::default()
        };
        let mut app = app_with_usage(u, 18);
        app.working_msg = "loading models for a long time".into();
        let lines = status_bar_lines(&app);
        assert_eq!(lines.len(), 2);
        assert!(ansi::line_plain(&lines[1]).starts_with("loading"));
        assert!(!ansi::line_plain(&lines[1]).contains("8.5K"));
        assert!(lines
            .iter()
            .all(|line| ansi::line_width(line) <= app.inner_width()));
        assert_eq!(lines[1].spans.last().unwrap().style, ansi::style_dim());
    }

    #[test]
    fn window_formatting() {
        assert_eq!(format_window_tokens(128000), "128K");
        assert_eq!(format_window_tokens(1_000_000), "1M");
        assert_eq!(format_window_tokens(150000), "150K");
    }

    fn app_with_subagent(u: StreamUsage, width: u16) -> App {
        let mut app = app_with_usage(u, width);
        app.manage_agents = vec![crate::app::empty_session_info()];
        app
    }

    #[test]
    fn nav_shares_status_row_with_context_meter() {
        let u = StreamUsage {
            total_tokens: 8500,
            ..Default::default()
        };
        let app = app_with_subagent(u, 100);
        let lines = status_bar_lines(&app);
        assert_eq!(lines.len(), 1, "hints fit on the status row");
        let txt = ansi::line_plain(&lines[0]);
        assert!(txt.contains("8.5K (7%)"), "context meter present: {txt}");
        assert!(
            txt.contains("(1 subagent) Shift ↓"),
            "nav hint shares the row: {txt}"
        );
        assert!(ansi::line_width(&lines[0]) <= app.inner_width());
    }

    #[test]
    fn nav_hints_wrap_within_status_width() {
        // Narrow terminal: the model is on its own line, the context
        // meter shares the next line with the first hint, and the second
        // hint wraps to a third line — nothing overflows.
        let u = StreamUsage {
            total_tokens: 8500,
            ..Default::default()
        };
        let mut app = app_with_subagent(u, 34);
        app.session.parent_id = "parent".into();
        let lines = status_bar_lines(&app);
        assert!(lines.len() >= 3, "hints wrap to extra rows");
        assert!(ansi::line_plain(&lines[0]).contains("gpt-5"));
        let row2 = ansi::line_plain(&lines[1]);
        assert!(row2.contains("8.5K (7%"), "meter on the hint row: {row2}");
        assert!(
            row2.contains("(1 subagent) Shift"),
            "first hint shares the meter row: {row2}"
        );
        let all = lines
            .iter()
            .map(ansi::line_plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("Shift ↑ to return"), "second hint wrapped");
        for line in &lines {
            assert!(
                ansi::line_width(line) <= app.inner_width(),
                "no row overflows"
            );
        }
    }

    #[test]
    fn nav_appends_transient_suffix_after_hints() {
        let u = StreamUsage {
            total_tokens: 8500,
            ..Default::default()
        };
        let mut app = app_with_subagent(u, 100);
        app.err_msg = "boom".into();
        let lines = status_bar_lines(&app);
        assert_eq!(lines.len(), 1);
        let txt = ansi::line_plain(&lines[0]);
        assert!(txt.contains("8.5K (7%)"));
        assert!(txt.contains("(1 subagent) Shift ↓"));
        assert!(txt.contains("boom"));
        assert!(txt.ends_with("boom"), "suffix trails the hints: {txt}");
    }

    #[test]
    fn nav_empty_is_plain_status_bar() {
        // No subagents, no parent: the status bar is unchanged.
        let u = StreamUsage {
            total_tokens: 8500,
            ..Default::default()
        };
        let app = app_with_usage(u, 100);
        let lines = status_bar_lines(&app);
        assert_eq!(lines.len(), 1);
        let txt = ansi::line_plain(&lines[0]);
        assert!(txt.contains("gpt-5"));
        assert!(txt.contains("8.5K (7%)"));
        assert!(!txt.contains("Shift"));
    }
}
