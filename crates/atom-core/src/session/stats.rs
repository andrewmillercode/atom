//! Stats aggregation for atom, modeled on opencode's `opencode stats`
//! command. The server exposes GET /api/stats (aggregated from every
//! session's per-message usage records), and the client renders the
//! report as a boxed table from the CLI (-stats) or the TUI (/stats).
//! Per-model usage always shows, so there's no extra flag to remember
//! like opencode's hidden-by-default --models. Ported from stats.go.

use crate::session::store::SessionStore;
use crate::types::StreamUsage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// tokenUsage is a token breakdown with the same shape opencode's stats
/// API reports: input and output (output includes reasoning), plus a
/// separate reasoning count and the cache read/write split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache: CacheUsage,
}

/// cacheUsage is the cache read/write split within a tokenUsage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheUsage {
    pub read: i64,
    pub write: i64,
}

/// modelUsage is the per-model aggregation: how many assistant messages
/// the model answered, the tokens those requests used, and the total
/// cost (best-effort: only counted when the provider reports it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub messages: i64,
    pub tokens: TokenUsage,
    pub cost: f64,
}

/// dateRange is the span of session activity covered by a statsReport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DateRange {
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

impl Default for DateRange {
    fn default() -> Self {
        // Go's zero time.Time marshals as 0001-01-01T00:00:00Z.
        let zero = DateTime::<Utc>::from_timestamp(-62_135_596_800, 0).unwrap_or_else(Utc::now);
        DateRange {
            earliest: zero,
            latest: zero,
        }
    }
}

/// statsReport is the full aggregation, JSON-shaped like opencode's
/// SessionStats so the two APIs stay comparable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsReport {
    #[serde(rename = "totalSessions")]
    pub total_sessions: i64,
    #[serde(rename = "totalMessages")]
    pub total_messages: i64,
    #[serde(rename = "totalCost")]
    pub total_cost: f64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: TokenUsage,
    #[serde(
        rename = "toolUsage",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub tool_usage: HashMap<String, i64>,
    #[serde(
        rename = "modelUsage",
        default,
        deserialize_with = "crate::serde_null::null_as_default"
    )]
    pub model_usage: HashMap<String, ModelUsage>,
    #[serde(rename = "dateRange")]
    pub date_range: DateRange,
    pub days: i64,
    #[serde(rename = "costPerDay")]
    pub cost_per_day: f64,
    #[serde(rename = "tokensPerSession")]
    pub tokens_per_session: f64,
    #[serde(rename = "medianTokensPerSession")]
    pub median_tokens_per_session: f64,
}

/// aggregateStats computes token usage across all sessions, optionally
/// restricted to the last N days (0 = all time). Per-message usage
/// records are exact; sessions recorded before this feature existed only
/// carry the latest request's usage, which is attributed to the session's
/// model once so old data still shows up. Empty sessions (created but
/// never messaged) are skipped, matching the session picker.
pub fn aggregate_stats(store: &SessionStore, days: i64) -> StatsReport {
    let mut report = StatsReport::default();
    let cutoff = if days > 0 {
        Some(Utc::now() - chrono::Duration::days(days))
    } else {
        None
    };
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;
    let mut session_totals: Vec<f64> = Vec::new();

    for info in store.list_info() {
        if info.message_count == 0 {
            continue;
        }
        if let Some(cutoff) = cutoff {
            if info.updated_at < cutoff {
                continue;
            }
        }
        let Some(sess) = store.get(&info.id) else {
            continue;
        };
        if sess.messages.is_empty() {
            continue;
        }
        report.total_sessions += 1;
        report.total_messages += sess.messages.len() as i64;
        if earliest.map_or(true, |e| sess.created_at < e) {
            earliest = Some(sess.created_at);
        }
        if latest.map_or(true, |l| sess.updated_at > l) {
            latest = Some(sess.updated_at);
        }

        // Every assistant message that carries a usage record lands
        // under the provider/model that answered it. Tool calls count
        // toward the tool usage chart.
        let mut sess_tokens = TokenUsage::default();
        let mut saw_usage = false;
        for msg in &sess.messages {
            if let Some(u) = &msg.usage {
                saw_usage = true;
                let key = if !msg.provider.is_empty() {
                    format!("{}/{}", msg.provider, msg.model)
                } else {
                    msg.model.clone()
                };
                let mu = report.model_usage.entry(key).or_default();
                mu.messages += 1;
                add_usage(&mut mu.tokens, u);
                mu.cost += u.cost;
                report.total_cost += u.cost;
                add_usage(&mut sess_tokens, u);
            }
            for tc in &msg.tool_calls {
                *report
                    .tool_usage
                    .entry(tc.function.name.clone())
                    .or_insert(0) += 1;
            }
        }
        // Pre-feature sessions have no per-message usage; attribute the
        // session's stored latest-request usage to its model once.
        if !saw_usage {
            if let (Some(u), false) = (&sess.usage, sess.model.is_empty()) {
                let mu = report.model_usage.entry(sess.model.clone()).or_default();
                mu.messages += 1;
                add_usage(&mut mu.tokens, u);
                mu.cost += u.cost;
                report.total_cost += u.cost;
                add_usage(&mut sess_tokens, u);
            }
        }

        report.total_tokens.input += sess_tokens.input;
        report.total_tokens.output += sess_tokens.output;
        report.total_tokens.reasoning += sess_tokens.reasoning;
        report.total_tokens.cache.read += sess_tokens.cache.read;
        report.total_tokens.cache.write += sess_tokens.cache.write;
        session_totals.push(
            (sess_tokens.input
                + sess_tokens.output
                + sess_tokens.cache.read
                + sess_tokens.cache.write) as f64,
        );
    }

    if report.total_sessions > 0 {
        let earliest = earliest.unwrap_or_default();
        let latest = latest.unwrap_or_default();
        report.date_range = DateRange { earliest, latest };
        report.days = days;
        if report.days == 0 {
            report.days = ((latest - earliest).num_hours() as f64 / 24.0).ceil() as i64;
            if report.days < 1 {
                report.days = 1;
            }
        }
        report.cost_per_day = report.total_cost / report.days as f64;
        report.tokens_per_session =
            tokens_total(&report.total_tokens) / report.total_sessions as f64;
        session_totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = session_totals.len() / 2;
        report.median_tokens_per_session = if session_totals.len() % 2 == 0 {
            (session_totals[mid - 1] + session_totals[mid]) / 2.0
        } else {
            session_totals[mid]
        };
    }
    report
}

/// addUsage folds one provider usage report into a tokenUsage breakdown.
pub fn add_usage(dst: &mut TokenUsage, u: &StreamUsage) {
    dst.input += u.prompt_tokens;
    dst.output += u.completion_tokens;
    dst.reasoning += u.reasoning_tokens;
    dst.cache.read += u.cache_read_tokens;
    dst.cache.write += u.cache_write_tokens;
}

/// tokensTotal returns the non-overlapping token total: input plus output
/// plus cache. Reasoning is a subset of output, so it isn't added again.
pub fn tokens_total(t: &TokenUsage) -> f64 {
    (t.input + t.output + t.cache.read + t.cache.write) as f64
}

/// formatTokens renders a token count exactly like Go's main.go variant:
/// "%.1fK" / "%.1fM" with no trailing-zero trimming (unlike
/// util::format_tokens, which emits "1K" for 1000).
pub fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ANSI styling matching colors.go's lipgloss styles byte-for-byte:
// styleDim = fg muted; styleInactive = fg primary dark;
// styleTool = fg foreground on bg tool background (single sequence).

fn hex_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    (
        u8::from_str_radix(&h[0..2], 16).unwrap_or(0),
        u8::from_str_radix(&h[2..4], 16).unwrap_or(0),
        u8::from_str_radix(&h[4..6], 16).unwrap_or(0),
    )
}

fn style_render(fg: &str, bg: Option<&str>, s: &str) -> String {
    use crate::render::colors::{theme_color, ThemeColor};
    let color = theme_color(match fg {
        "dim" => ThemeColor::Muted,
        "inactive" => ThemeColor::PrimaryDark,
        _ => ThemeColor::Foreground,
    });
    let (r, g, b) = hex_rgb(&color);
    let mut params = format!("38;2;{r};{g};{b}");
    if bg == Some("tool") {
        let (br, bgc, bb) = hex_rgb(&theme_color(ThemeColor::CardDark));
        params.push_str(&format!(";48;2;{br};{bgc};{bb}"));
    }
    format!("\x1b[{params}m{s}\x1b[0m")
}

fn style_dim(s: &str) -> String {
    style_render("dim", None, s)
}

fn style_inactive(s: &str) -> String {
    style_render("inactive", None, s)
}

fn style_tool(s: &str) -> String {
    style_render("foreground", Some("tool"), s)
}

/// renderStats renders the report as a boxed table like opencode's stats
/// output. width is the box width (<= 0 for the default 56, clamped to
/// 34..64); color enables ANSI styling (borders dim, section titles
/// primary dark). Returns the report as lines, ready to join with "\n".
pub fn render_stats(report: &StatsReport, width: i32, color: bool) -> Vec<String> {
    let mut width = width;
    if width <= 0 {
        width = 56;
    }
    if width < 34 {
        width = 34;
    }
    if width > 64 {
        width = 64;
    }

    let box_line = |corner: &str| -> String {
        let line = format!("{}{}{}", corner, "─".repeat((width - 2) as usize), corner);
        if color {
            style_dim(&line)
        } else {
            line
        }
    };
    let title = |s: &str| -> String {
        let line = pad_box_line(&format!(" {s}"), width);
        if color {
            style_inactive(&line)
        } else {
            line
        }
    };
    let row = |label: &str, value: &str| -> String {
        let inner = width - 2;
        let mut label = label.to_string();
        let mut gap = inner - cell_width(&label) - cell_width(value) - 1;
        if gap < 1 {
            let max_label = (inner - cell_width(value) - 2).max(0);
            label = truncate(&label, max_label);
            gap = inner - cell_width(&label) - cell_width(value) - 1;
            if gap < 1 {
                gap = 1;
            }
        }
        pad_box_line(
            &format!("{}{}{} ", label, " ".repeat(gap as usize), value),
            width,
        )
    };

    if report.total_sessions == 0 {
        let line = "no sessions yet — start chatting to see stats";
        return vec![if color {
            style_dim(line)
        } else {
            line.to_string()
        }];
    }

    let mut lines: Vec<String> = Vec::new();

    // Overview: session and message counts plus the span in days.
    lines.push(box_line("┌"));
    lines.push(title("OVERVIEW"));
    lines.push(box_line("├"));
    lines.push(row("Sessions", &report.total_sessions.to_string()));
    lines.push(row("Messages", &report.total_messages.to_string()));
    lines.push(row("Days", &report.days.to_string()));
    lines.push(box_line("└"));
    lines.push(String::new());

    // Tokens (and cost, when any provider reported it).
    let heading = if report.total_cost > 0.0 {
        "COST & TOKENS"
    } else {
        "TOKENS"
    };
    lines.push(box_line("┌"));
    lines.push(title(heading));
    lines.push(box_line("├"));
    if report.total_cost > 0.0 {
        lines.push(row("Total Cost", &format!("${:.2}", report.total_cost)));
        lines.push(row("Avg Cost/Day", &format!("${:.2}", report.cost_per_day)));
    }
    lines.push(row(
        "Avg Tokens/Session",
        &format_tokens((report.tokens_per_session.round()) as i64),
    ));
    lines.push(row(
        "Median Tokens/Session",
        &format_tokens((report.median_tokens_per_session.round()) as i64),
    ));
    lines.push(row("Input", &format_tokens(report.total_tokens.input)));
    lines.push(row("Output", &format_tokens(report.total_tokens.output)));
    if report.total_tokens.reasoning > 0 {
        lines.push(row(
            "Reasoning",
            &format_tokens(report.total_tokens.reasoning),
        ));
    }
    if report.total_tokens.cache.read > 0 {
        lines.push(row(
            "Cache Read",
            &format_tokens(report.total_tokens.cache.read),
        ));
    }
    if report.total_tokens.cache.write > 0 {
        lines.push(row(
            "Cache Write",
            &format_tokens(report.total_tokens.cache.write),
        ));
    }
    lines.push(box_line("└"));
    lines.push(String::new());

    // Per-model breakdown, biggest consumers first.
    if !report.model_usage.is_empty() {
        lines.push(box_line("┌"));
        lines.push(title("MODEL USAGE"));
        lines.push(box_line("├"));
        let models = sorted_models(&report.model_usage);
        for (i, key) in models.iter().enumerate() {
            let mu = &report.model_usage[key];
            lines.push(row(
                &format!(" {}", truncate(key, width - 12)),
                &format!("{} msgs", mu.messages),
            ));
            lines.push(row("  Input", &format_tokens(mu.tokens.input)));
            lines.push(row("  Output", &format_tokens(mu.tokens.output)));
            if mu.tokens.reasoning > 0 {
                lines.push(row("  Reasoning", &format_tokens(mu.tokens.reasoning)));
            }
            if mu.tokens.cache.read > 0 {
                lines.push(row("  Cache Read", &format_tokens(mu.tokens.cache.read)));
            }
            if mu.tokens.cache.write > 0 {
                lines.push(row("  Cache Write", &format_tokens(mu.tokens.cache.write)));
            }
            if mu.cost > 0.0 {
                lines.push(row("  Cost", &format!("${:.4}", mu.cost)));
            }
            if i < models.len() - 1 {
                lines.push(box_line("├"));
            }
        }
        lines.push(box_line("└"));
        lines.push(String::new());
    }

    // Tool usage as a bar chart, most-used first.
    if !report.tool_usage.is_empty() {
        lines.push(box_line("┌"));
        lines.push(title("TOOL USAGE"));
        lines.push(box_line("├"));
        let tools = sorted_tools(&report.tool_usage);
        let mut max_count = 0i64;
        let mut total_calls = 0i64;
        for t in &tools {
            let count = report.tool_usage[t];
            if count > max_count {
                max_count = count;
            }
            total_calls += count;
        }
        for t in &tools {
            let count = report.tool_usage[t];
            let pct = format!(" ({:.1}%)", count as f64 / total_calls as f64 * 100.0);
            let suffix = format!(" {}{}", count, pct);
            let inner = width - 2;
            let mut name_cols = 16i32;
            let mut prefix;
            let mut max_bar;
            loop {
                prefix = format!(" {} ", truncate(t, name_cols));
                max_bar = inner - cell_width(&prefix) - cell_width(&suffix);
                if max_bar >= 1 || name_cols <= 1 {
                    break;
                }
                name_cols -= 1;
            }
            if max_bar < 1 {
                max_bar = 0;
            }
            let mut bar_len = 0usize;
            if max_bar >= 1 {
                bar_len = (count * max_bar as i64 / max_count) as usize;
                if bar_len < 1 {
                    bar_len = 1;
                }
            }
            let content = format!("{}{}{}", prefix, "█".repeat(bar_len), suffix);
            let line = pad_box_line(&content, width);
            let line = if color { style_tool(&line) } else { line };
            lines.push(line);
        }
        lines.push(box_line("└"));
        lines.push(String::new());
    }

    lines
}

/// sortedModels returns model keys ordered by total tokens descending
/// (ties break alphabetically), so the biggest consumers come first.
pub fn sorted_models(usage: &HashMap<String, ModelUsage>) -> Vec<String> {
    let mut keys: Vec<String> = usage.keys().cloned().collect();
    keys.sort_by(|a, b| {
        let sa = tokens_total(&usage[a].tokens);
        let sb = tokens_total(&usage[b].tokens);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    keys
}

/// sortedTools returns tool names ordered by call count descending.
pub fn sorted_tools(usage: &HashMap<String, i64>) -> Vec<String> {
    let mut keys: Vec<String> = usage.keys().cloned().collect();
    keys.sort_by(|a, b| {
        let ca = usage[a];
        let cb = usage[b];
        cb.cmp(&ca).then(a.cmp(b))
    });
    keys
}

/// cellWidth is the terminal column count of s (one per rune). Used
/// instead of byte length so multi-byte glyphs like █ don't throw off
/// padding.
pub fn cell_width(s: &str) -> i32 {
    s.chars().count() as i32
}

/// padBoxLine wraps inner in │…│ and pads so the line is exactly width
/// columns. Callers must size inner to fit; leftover space goes before
/// the right border.
pub fn pad_box_line(inner: &str, width: i32) -> String {
    let max_inner = (width - 2).max(0);
    let mut inner = inner.to_string();
    if cell_width(&inner) > max_inner {
        inner = truncate(&inner, max_inner);
    }
    let pad = (max_inner - cell_width(&inner)).max(0) as usize;
    format!("│{}{}│", inner, " ".repeat(pad))
}

/// truncate shortens s to n runes, appending ".." when it had to cut.
pub fn truncate(s: &str, n: i32) -> String {
    let r: Vec<char> = s.chars().collect();
    if r.len() <= n.max(0) as usize {
        return s.to_string();
    }
    if n <= 2 {
        return r[..n.max(0) as usize].iter().collect();
    }
    let mut out: String = r[..(n - 2) as usize].iter().collect();
    out.push_str("..");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::SessionStore;
    use crate::types::{FunctionCall, Message, ToolCall};

    fn temp_store(tag: &str) -> (std::path::PathBuf, SessionStore) {
        let dir = std::env::temp_dir().join(format!(
            "atom-stats-test-{}-{}-{tag}",
            std::process::id(),
            crate::session::store::new_session_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SessionStore::open_in_dir(&dir).unwrap();
        (dir, store)
    }

    fn cleanup(dir: &std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn role_msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn assistant_with_usage(provider: &str, model: &str, content: &str, u: StreamUsage) -> Message {
        Message {
            role: "assistant".into(),
            provider: provider.into(),
            model: model.into(),
            content: content.into(),
            usage: Some(u),
            ..Default::default()
        }
    }

    #[test]
    fn aggregate_stats_per_model() {
        let (d, store) = temp_store("per-model");

        let s1 = store.create("deepseek-v4-flash:cloud", "/tmp", vec![]);
        let mut messages = vec![
            role_msg("user", "hi"),
            assistant_with_usage(
                "ollama",
                "deepseek-v4-flash:cloud",
                "a",
                StreamUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    reasoning_tokens: 20,
                    cache_read_tokens: 10,
                    cache_write_tokens: 5,
                    total_tokens: 150,
                    ..Default::default()
                },
            ),
            role_msg("user", "again"),
        ];
        messages.push(Message {
            tool_calls: vec![ToolCall {
                id: "1".into(),
                call_type: String::new(),
                function: FunctionCall {
                    name: "web_search".into(),
                    arguments: r#"{"query":"x"}"#.into(),
                },
            }],
            ..assistant_with_usage(
                "ollama",
                "deepseek-v4-flash:cloud",
                "b",
                StreamUsage {
                    prompt_tokens: 200,
                    completion_tokens: 60,
                    total_tokens: 260,
                    ..Default::default()
                },
            )
        });
        store.update(&s1.id, messages, "");

        let s2 = store.create("gpt-oss:120b-cloud", "/tmp", vec![]);
        store.update(
            &s2.id,
            vec![
                role_msg("user", "q"),
                assistant_with_usage(
                    "opencode-go",
                    "gpt-oss:120b-cloud",
                    "c",
                    StreamUsage {
                        prompt_tokens: 1000,
                        completion_tokens: 400,
                        total_tokens: 1400,
                        ..Default::default()
                    },
                ),
            ],
            "",
        );

        let r = aggregate_stats(&store, 0);
        assert_eq!(r.total_sessions, 2);
        assert_eq!(r.total_messages, 6);
        assert_eq!(r.total_tokens.input, 1300);
        assert_eq!(r.total_tokens.output, 510);
        assert_eq!(r.total_tokens.reasoning, 20);
        assert_eq!(r.total_tokens.cache.read, 10);
        assert_eq!(r.total_tokens.cache.write, 5);

        let ds = &r.model_usage["ollama/deepseek-v4-flash:cloud"];
        assert_eq!(ds.messages, 2);
        assert_eq!(ds.tokens.input, 300);
        assert_eq!(ds.tokens.output, 110);
        assert_eq!(r.model_usage["opencode-go/gpt-oss:120b-cloud"].messages, 1);
        assert_eq!(r.tool_usage["web_search"], 1);

        // Session totals: s1 = 165 + 260 = 425, s2 = 1400. Median and
        // per-session average are both (425 + 1400) / 2 = 912.5.
        assert_eq!(r.median_tokens_per_session, 912.5);
        assert_eq!(r.tokens_per_session, 912.5);
        assert!(r.days >= 1);
        cleanup(&d);
    }

    #[test]
    fn aggregate_stats_fallback_to_session_usage() {
        let (d, store) = temp_store("fallback");
        let s = store.create("deepseek-v4-flash:cloud", "/tmp", vec![]);
        store.modify(&s.id, |sess| {
            sess.messages = vec![role_msg("user", "hi"), role_msg("assistant", "yo")];
            sess.usage = Some(StreamUsage {
                prompt_tokens: 500,
                completion_tokens: 100,
                total_tokens: 600,
                ..Default::default()
            });
        });

        let r = aggregate_stats(&store, 0);
        assert_eq!(r.total_tokens.input, 500);
        assert_eq!(r.total_tokens.output, 100);
        let mu = &r.model_usage["deepseek-v4-flash:cloud"];
        assert_eq!(mu.messages, 1);
        assert_eq!(mu.tokens.input, 500);
        cleanup(&d);
    }

    #[test]
    fn aggregate_stats_days_window() {
        let (d, store) = temp_store("days");
        let old = store.create("m1", "/tmp", vec![]);
        store.modify(&old.id, |sess| {
            sess.messages = vec![
                role_msg("user", "hi"),
                assistant_with_usage(
                    "",
                    "",
                    "yo",
                    StreamUsage {
                        prompt_tokens: 10,
                        completion_tokens: 10,
                        total_tokens: 20,
                        ..Default::default()
                    },
                ),
            ];
            sess.updated_at = Utc::now() - chrono::Duration::days(10);
        });

        let recent = store.create("m2", "/tmp", vec![]);
        store.modify(&recent.id, |sess| {
            sess.messages = vec![
                role_msg("user", "hi"),
                assistant_with_usage(
                    "",
                    "",
                    "yo",
                    StreamUsage {
                        prompt_tokens: 20,
                        completion_tokens: 20,
                        total_tokens: 40,
                        ..Default::default()
                    },
                ),
            ];
        });

        let r = aggregate_stats(&store, 7);
        assert_eq!(r.total_sessions, 1);
        assert_eq!(r.total_tokens.input, 20);
        assert_eq!(r.days, 7);

        let r = aggregate_stats(&store, 0);
        assert_eq!(r.total_sessions, 2);
        cleanup(&d);
    }

    #[test]
    fn aggregate_stats_empty_store() {
        let (d, store) = temp_store("empty");
        let r = aggregate_stats(&store, 0);
        assert_eq!(r.total_sessions, 0);
        assert_eq!(r.total_messages, 0);
        assert_eq!(r.total_tokens.input, 0);
        assert!(r.model_usage.is_empty());
        assert!(r.tool_usage.is_empty());
        cleanup(&d);
    }

    #[test]
    fn render_stats_sections() {
        let mut r = StatsReport {
            total_sessions: 2,
            total_messages: 6,
            days: 3,
            total_tokens: TokenUsage {
                input: 1300,
                output: 510,
                ..Default::default()
            },
            model_usage: HashMap::from([(
                "ollama/deepseek-v4-flash:cloud".to_string(),
                ModelUsage {
                    messages: 2,
                    tokens: TokenUsage {
                        input: 300,
                        output: 110,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )]),
            tool_usage: HashMap::from([("web_search".to_string(), 1)]),
            ..Default::default()
        };
        let out = render_stats(&r, 0, false).join("\n");
        for want in [
            "OVERVIEW",
            "Sessions",
            "TOKENS",
            "MODEL USAGE",
            "deepseek-v4-flash:cloud",
            "TOOL USAGE",
            "web_search",
        ] {
            assert!(out.contains(want), "missing {want}:\n{out}");
        }

        // Cost rows appear only when a provider reported cost.
        r.total_cost = 1.5;
        r.cost_per_day = 0.5;
        let out = render_stats(&r, 0, false).join("\n");
        assert!(out.contains("COST & TOKENS"), "{out}");
        assert!(out.contains("$1.50"), "{out}");

        let got = render_stats(&StatsReport::default(), 0, false).join("\n");
        assert!(got.contains("no sessions yet"), "{got}");
    }

    #[test]
    fn render_stats_tool_usage_width() {
        let r = StatsReport {
            total_sessions: 1,
            total_messages: 2,
            days: 1,
            total_tokens: TokenUsage {
                input: 10,
                output: 10,
                ..Default::default()
            },
            tool_usage: HashMap::from([
                ("web_search".to_string(), 12),
                ("read_file".to_string(), 3),
                ("very_long_tool_name_that_needs_truncating".to_string(), 1),
            ]),
            ..Default::default()
        };
        for width in [40, 56] {
            let lines = render_stats(&r, width, false);
            let mut saw_tool = false;
            for line in &lines {
                if line.is_empty() {
                    continue;
                }
                assert_eq!(cell_width(line), width, "width {width}: {line:?}");
                let runes: Vec<char> = line.chars().collect();
                assert!(!runes.is_empty(), "width {width}: empty box line");
                if runes[0] == '│' {
                    assert_eq!(
                        *runes.last().unwrap(),
                        '│',
                        "width {width}: right border missing: {line:?}"
                    );
                }
                if line.contains("web_search") || line.contains('█') {
                    saw_tool = true;
                }
            }
            assert!(saw_tool, "width {width}: missing tool usage bars");
        }
    }

    #[test]
    fn helpers_match_go() {
        assert_eq!(truncate("hello world", 8), "hello ..");
        assert_eq!(truncate("hi", 5), "hi");
        assert_eq!(pad_box_line("ab", 6), "│ab  │");
        assert_eq!(format_tokens(1300), "1.3K");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(
            sorted_tools(&HashMap::from([
                ("a".to_string(), 1),
                ("b".to_string(), 3),
                ("c".to_string(), 3),
            ])),
            vec!["b".to_string(), "c".to_string(), "a".to_string()]
        );
        let usage = HashMap::from([
            (
                "x".to_string(),
                ModelUsage {
                    tokens: TokenUsage {
                        input: 10,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "y".to_string(),
                ModelUsage {
                    tokens: TokenUsage {
                        input: 20,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            sorted_models(&usage),
            vec!["y".to_string(), "x".to_string()]
        );
        assert_eq!(
            tokens_total(&TokenUsage {
                input: 1,
                output: 2,
                reasoning: 9,
                cache: CacheUsage { read: 3, write: 4 }
            }),
            10.0
        );
    }
}
