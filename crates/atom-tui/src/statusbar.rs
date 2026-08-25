//! statusbar.rs renders the one-line (or two-line on narrow terminals)
//! status bar: model + thinking level + token usage + transient status.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ansi;
use crate::app::App;

/// Status bar thresholds from tui.go.
pub const USAGE_COMPACT_INNER_WIDTH: usize = 90;
pub const USAGE_MINIMAL_INNER_WIDTH: usize = 70;
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

/// usageString renders the session's token usage for the status bar:
/// "Input 8.4K, Output 100, Cache 50%, Context 6% (8.4K/128K)". Labels
/// shorten below usageCompactInnerWidth and drop below minimal width.
/// Pure-text variant used by tests; spans version wraps it in dim style.
pub fn usage_string(app: &App, inner_width: usize) -> String {
    let Some(u) = &app.session.usage else {
        return String::new();
    };
    if u.total_tokens <= 0 {
        return String::new();
    }
    let mut input = "--".to_string();
    let mut out = "--".to_string();
    let mut cache = "--".to_string();
    if u.prompt_tokens > 0 || u.completion_tokens > 0 {
        input = atom_core::util::format_tokens(u.prompt_tokens);
        out = atom_core::util::format_tokens(u.completion_tokens);
    }
    let denom = if u.prompt_tokens_all > 0 {
        u.prompt_tokens_all
    } else {
        u.prompt_tokens
    };
    if (u.cache_read_tokens > 0 || u.cache_write_tokens > 0) && denom > 0 {
        cache = format!("{}%", u.cache_read_tokens * 100 / denom);
    }
    let mut ctx = atom_core::util::format_tokens(u.total_tokens);
    let w = atom_core::providers::modelsdev::context_window_tokens(
        &app.sel_provider.name,
        &app.sel_model,
    );
    if w > 0 {
        ctx = format!(
            "{}% ({}/{})",
            u.total_tokens * 100 / w,
            atom_core::util::format_tokens(u.total_tokens),
            format_window_tokens(w)
        );
    }
    if app.width > 0 && inner_width < USAGE_MINIMAL_INNER_WIDTH {
        return ctx;
    }
    let (in_l, out_l, cache_l, ctx_l) = if app.width > 0 && inner_width < USAGE_COMPACT_INNER_WIDTH
    {
        ("In", "Out", "C", "Ctx")
    } else {
        ("Input", "Output", "Cache", "Context")
    };
    format!("{in_l} {input}, {out_l} {out}, {cache_l} {cache}, {ctx_l} {ctx}")
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
    let mut variants = Vec::new();
    for candidate_width in [
        width,
        USAGE_COMPACT_INNER_WIDTH.saturating_sub(1),
        USAGE_MINIMAL_INNER_WIDTH.saturating_sub(1),
    ] {
        let candidate = usage_string(app, candidate_width);
        if !candidate.is_empty() && !variants.contains(&candidate) {
            variants.push(candidate);
        }
    }
    variants
}

fn combined_width(head: &str, usage: Option<&str>, suffix: Option<&str>) -> usize {
    text_width(head)
        + usage.map(|text| 2 + text_width(text)).unwrap_or(0)
        + suffix.map(|text| 2 + text_width(text)).unwrap_or(0)
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

fn status_bar_layout(app: &App) -> Vec<Line<'static>> {
    let width = app.inner_width();
    let head = status_head(app);
    let usage_variants = usage_variants(app, width);
    let suffix = status_suffix(app);

    if usage_variants.is_empty() {
        if combined_width(&head, None, suffix.as_ref().map(|(text, _)| text.as_str())) <= width {
            return vec![one_line(head, None, suffix)];
        }
    } else if let Some(usage) = usage_variants.iter().find(|usage| {
        combined_width(
            &head,
            Some(usage),
            suffix.as_ref().map(|(text, _)| text.as_str()),
        ) <= width
    }) {
        return vec![one_line(head, Some(usage.clone()), suffix)];
    }

    let fitted_head = fitted_status_head(app, width, &head);
    if usage_variants.is_empty() && suffix.is_none() {
        return vec![one_line(fitted_head, None, None)];
    }

    let secondary = secondary_line(&usage_variants, suffix.as_ref(), width);
    if fitted_head.is_empty() {
        vec![secondary]
    } else if secondary.spans.is_empty() || ansi::line_plain(&secondary).is_empty() {
        vec![one_line(fitted_head, None, None)]
    } else {
        vec![one_line(fitted_head, None, None), secondary]
    }
}

/// renderStatusBar builds the dim status line(s).
pub fn status_bar_lines(app: &App) -> Vec<Line<'static>> {
    status_bar_layout(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atom_core::types::StreamUsage;
    use std::time::Instant;

    fn app_with_usage(u: StreamUsage, width: u16) -> App {
        let mut app = App::new_test(width, 24);
        app.session.model = "gpt-5".into();
        app.sel_model = "gpt-5".into();
        app.session.usage = Some(u);
        app
    }

    #[test]
    fn usage_full_labels() {
        let u = StreamUsage {
            prompt_tokens: 8400,
            completion_tokens: 100,
            total_tokens: 8500,
            cache_read_tokens: 4200,
            ..Default::default()
        };
        // prompt_tokens_all == 0 → denom = prompt_tokens → 50%
        let app = app_with_usage(u, 120);
        let s = usage_string(&app, 120);
        assert_eq!(
            s,
            "Input 8.4K, Output 100, Cache 50%, Context 6% (8.5K/128K)"
        );
    }

    #[test]
    fn usage_compact_and_minimal() {
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
        assert_eq!(
            usage_string(&app, 89),
            "In 12.3K, Out 678, C 7%, Ctx 10% (13K/128K)"
        );
        assert_eq!(usage_string(&app, 69), "10% (13K/128K)");
    }

    #[test]
    fn usage_unknown_parts_render_dashes() {
        let u = StreamUsage {
            total_tokens: 500,
            ..Default::default()
        };
        let app = app_with_usage(u, 120);
        // Context always carries the catalog-window percentage (128K fallback).
        assert_eq!(
            usage_string(&app, 120),
            "Input --, Output --, Cache --, Context 0% (500/128K)"
        );
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
}
