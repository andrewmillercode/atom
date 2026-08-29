//! math.rs renders `$$…$$` display math in assistant messages as
//! Kitty-graphics formulas using the vendored `ratatex` crate.
//!
//! One engine per process lives in a global slot, started by [`init`]
//! from the event loop only when the terminal speaks the Kitty graphics
//! protocol (no engine → the markdown path renders the LaTeX verbatim,
//! exactly as before). The engine rasterizes each closed display-math
//! region to a cell-aligned PNG on bounded background workers, caches
//! it on disk, and hands us two things: raw Kitty upload/place command
//! bytes to write to the tty ( [`flush_terminal_commands`], called while
//! the frame's tty guard is held, before the placeholder cells are
//! drawn) and self-describing placeholder cells for the visible grid.
//!
//! The placeholder cells are ordinary terminal cells: U+10EEEE plus
//! row/column combining diacritics encode the image id and tile
//! coordinates, and the RGB foreground repeats the image id. So a
//! formula is converted to one [`Line`] per row ([`formula_to_lines`],
//! by rendering ratatex's `FormulaWidget` into a scratch buffer) and
//! spliced into the block's cached lines like any other text — viewport
//! clipping simply truncates rows, which preserves the tile coordinates
//! kitty needs. No overlay pass or signed positioning is required.
//!
//! Cache invalidation: every completed render (success or failure)
//! bumps the engine's monotonic [`generation`]. Blocks store the
//! generation they were rendered under (`Block::line_formula_gen`) and
//! treat a mismatch as a stale cache, re-rendering on the next frame.
//! A render that is still pending keeps its LaTeX fallback visible
//! until the worker's completion callback fires [`AppMsg::MathWake`],
//! which marks the viewport dirty and redraws.

use std::sync::OnceLock;

use ratatex::{ColorScheme, Formula, FormulaState, FormulaWidget, MarkdownSegment, PixelSize};
use ratatex::{Ratatex, RenderFailureKind, Rgb, TerminalProfile};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use tokio::sync::mpsc::UnboundedSender;

use crate::ansi;
use crate::events::AppMsg;

static ENGINE: OnceLock<Ratatex> = OnceLock::new();

/// Starts the global math engine if the terminal supports Kitty
/// graphics. Safe to call repeatedly; later calls are no-ops. `wake`
/// receives [`AppMsg::MathWake`] whenever a background render completes
/// so the event loop can redraw the affected blocks.
pub fn init(wake: UnboundedSender<AppMsg>) {
    if !crate::preview::kitty_terminal() || ENGINE.get().is_some() {
        return;
    }
    // Formula geometry is its own knob, deliberately not preview.rs's
    // thumbnail constants: ratatex slices the rasterized formula into
    // "cells" of this assumed size and kitty then scales the PNG across
    // that many real terminal cells, so a smaller assumed cell renders
    // the same glyphs larger — and sharper, because kitty downscales a
    // denser PNG instead of upscaling a sparse one. 20×40 keeps the 1:2
    // aspect of typical terminal cells (no distortion), and with dpi 240
    // a display formula lands around two to two-and-a-half text lines
    // tall instead of being squeezed into one. tmux passthrough is not
    // used anywhere in atom; formulas are no different.
    const MATH_CELL_W: u16 = 20;
    const MATH_CELL_H: u16 = 40;
    const MATH_DPI: u16 = 240;
    let engine = Ratatex::builder(TerminalProfile::kitty(
        PixelSize::new(MATH_CELL_W, MATH_CELL_H),
        false,
    ))
    .dpi(MATH_DPI)
    .colors(ColorScheme {
        foreground: theme_foreground(),
        // Transparent background + grayscale antialiasing composites
        // onto whatever the terminal shows, so theme switches and
        // selection highlights never leave formula halos.
        background: None,
    })
    .on_update(move || {
        // Best-effort: a send error means the TUI is shutting down.
        let _ = wake.send(AppMsg::MathWake);
    })
    .build();
    if let Ok(engine) = engine {
        let _ = ENGINE.set(engine);
    }
}

/// The global engine, when the terminal supports Kitty graphics.
pub fn engine() -> Option<&'static Ratatex> {
    ENGINE.get()
}

/// Monotonic engine generation (0 without an engine). Blocks rendered
/// with a nonzero generation that no longer matches must re-render:
/// some formula finished rendering since.
pub fn generation() -> u64 {
    ENGINE.get().map_or(0, Ratatex::generation)
}

/// Writes any pending Kitty upload/place commands to the tty. The
/// caller MUST hold `preview::lock_tty()` — the frame draw shares that
/// guard, and the commands must reach the terminal before the frame
/// containing the matching placeholder cells.
pub fn flush_terminal_commands() {
    let Some(engine) = ENGINE.get() else {
        return;
    };
    for command in engine.drain_terminal_commands() {
        crate::preview::write_tty_locked_bytes(command.as_bytes());
    }
}

/// Renders one assistant message with display math through the engine.
///
/// Splits the message into prose and closed display-math segments:
/// prose goes through the regular markdown renderer, ready formulas
/// become Kitty placeholder rows, and formulas that are still
/// rendering (or failed) fall back to their LaTeX source so the
/// transcript never blanks or hides output. Paragraph spacing is kept
/// around each formula.
///
/// Returns `None` when there is nothing math-specific to do — no
/// engine (non-Kitty terminal or tests), or no closed display math in
/// `text` — and the caller should use the plain markdown path.
pub fn render_assistant_markdown(text: &str, width: usize) -> Option<ansi::LinkedLines> {
    render_assistant_markdown_with(ENGINE.get()?, text, width)
}

/// [`render_assistant_markdown`] against an explicit engine (the global
/// slot cannot be set from tests).
fn render_assistant_markdown_with(
    engine: &Ratatex,
    text: &str,
    width: usize,
) -> Option<ansi::LinkedLines> {
    let segments = ratatex::markdown_segments(text);
    if !segments
        .iter()
        .any(|s| matches!(s, MarkdownSegment::DisplayMath(_)))
    {
        return None;
    }
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<Vec<ansi::LinkRegion>> = Vec::new();
    let mut iter = segments.into_iter().peekable();
    while let Some(segment) = iter.next() {
        match segment {
            MarkdownSegment::Text(prose) => {
                let parsed = ansi::ansi_to_lines_linked(
                    &atom_core::render::markdown::render_markdown(prose, width, ""),
                );
                lines.extend(parsed.lines);
                links.extend(parsed.links);
            }
            MarkdownSegment::DisplayMath(math) => {
                // Paragraph break between preceding prose and the formula.
                if !lines.is_empty() {
                    lines.push(Line::from(String::new()));
                    links.push(Vec::new());
                }
                match engine.request(math.source(), width.min(u16::MAX as usize) as u16) {
                    FormulaState::Ready(formula) => {
                        let rows = formula_to_lines(&formula);
                        if rows.is_empty() {
                            render_math_fallback(math.full_source(), width, &mut lines, &mut links);
                        } else {
                            links.extend(std::iter::repeat_n(Vec::new(), rows.len()));
                            lines.extend(rows);
                        }
                    }
                    // Transient queue overflow: the request was dropped, so
                    // abandon the math path for this frame and let the next
                    // wake (other formulas are in flight) retry it.
                    FormulaState::Failed(failure) if failure.kind() == RenderFailureKind::Busy => {
                        return None;
                    }
                    // Pending: the worker owns it and a MathWake will follow.
                    // Failed: deterministic (parse/limit) — keep the LaTeX.
                    FormulaState::Pending | FormulaState::Failed(_) | FormulaState::Unsupported => {
                        render_math_fallback(math.full_source(), width, &mut lines, &mut links);
                    }
                }
                // Paragraph break between the formula and following prose.
                if matches!(iter.peek(), Some(MarkdownSegment::Text(_))) {
                    lines.push(Line::from(String::new()));
                    links.push(Vec::new());
                }
            }
        }
    }
    Some(ansi::LinkedLines { lines, links })
}

/// Renders the raw display region (delimiters included) as ordinary
/// markdown — the pre-math appearance.
fn render_math_fallback(
    source: &str,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<Vec<ansi::LinkRegion>>,
) {
    let parsed = ansi::ansi_to_lines_linked(&atom_core::render::markdown::render_markdown(
        source, width, "",
    ));
    lines.extend(parsed.lines);
    links.extend(parsed.links);
}

/// Converts one prepared formula into ordinary ratatui lines: one line
/// per reserved row, whose cells carry ratatex's placeholder symbols
/// (U+10EEEE + row/column diacritics) with the image id as RGB
/// foreground. Rendering `FormulaWidget` into a scratch buffer reuses
/// ratatex's own cell encoding instead of duplicating it.
fn formula_to_lines(formula: &Formula) -> Vec<Line<'static>> {
    let (cols, rows) = (usize::from(formula.columns()), usize::from(formula.rows()));
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let area = Rect::new(0, 0, formula.columns(), formula.rows());
    let mut buf = Buffer::empty(area);
    FormulaWidget::new(formula).render(area, &mut buf);
    let mut lines = Vec::with_capacity(rows);
    for y in 0..rows {
        let mut symbols = String::with_capacity(cols * 4);
        let mut style = Style::new();
        for x in 0..cols {
            let cell = &buf[(x as u16, y as u16)];
            symbols.push_str(cell.symbol());
            style = Style::new().fg(cell.fg);
        }
        lines.push(Line::styled(symbols, style));
    }
    lines
}

/// Parses the theme's foreground hex color for formula glyphs, falling
/// back to ratatex's near-white default when the theme value does not
/// parse. Captured once at engine start; a live theme switch keeps
/// already-rendered formulas until restart (documented limitation).
fn theme_foreground() -> Rgb {
    fn parse(hex: &str) -> Option<Rgb> {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        Some(Rgb::new(
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ))
    }
    parse(&atom_core::render::colors::theme_color(
        atom_core::render::colors::ThemeColor::Foreground,
    ))
    .unwrap_or(Rgb::new(235, 235, 235))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{formula_to_lines, render_assistant_markdown_with};
    use ratatex::{PixelSize, Ratatex, TerminalProfile};

    /// Builds a real in-process engine writing to a throwaway cache and
    /// counting completions, mirroring ratatex's own engine tests.
    fn test_engine() -> (Ratatex, Arc<AtomicUsize>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let renders = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&renders);
        let engine = Ratatex::builder(TerminalProfile::kitty(PixelSize::new(32, 64), false))
            .cache_dir(dir.path())
            .on_update(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .expect("build math engine");
        (engine, renders, dir)
    }

    /// Waits until the worker completes the given number of renders.
    fn wait_for(renders: &AtomicUsize, count: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while renders.load(Ordering::SeqCst) < count {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn plain_markdown_takes_the_regular_path() {
        let (engine, renders, _dir) = test_engine();
        assert!(render_assistant_markdown_with(&engine, "just prose", 60).is_none());
        // Unclosed streaming math is deliberately not display math yet.
        assert!(render_assistant_markdown_with(&engine, "$$still streaming", 60).is_none());
        assert_eq!(renders.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ready_formulas_render_as_placeholder_rows_with_spacing() {
        let (engine, renders, _dir) = test_engine();
        let text = "Before\n\n$$x^2+y^2$$\n\nAfter";
        // First pass: the worker owns the request; the segment still takes
        // the math path with a LaTeX fallback.
        let first = render_assistant_markdown_with(&engine, text, 60).expect("math path");
        assert!(first
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("$$"))));
        assert!(wait_for(&renders, 1), "formula did not render in time");
        let ready = render_assistant_markdown_with(&engine, text, 60).expect("math path");
        // Structure: "Before", blank, placeholder rows, blank, "After".
        assert!(ready.lines.len() >= 5);
        assert_eq!(plain(&ready.lines[0]), "Before");
        assert_eq!(plain(&ready.lines[1]), "");
        let placeholder_rows: Vec<usize> = (2..ready.lines.len() - 2)
            .filter(|i| plain(&ready.lines[*i]).starts_with('\u{10EEEE}'))
            .collect();
        assert!(
            !placeholder_rows.is_empty(),
            "no placeholder rows among {}",
            ready.lines.len()
        );
        for row in &placeholder_rows {
            let line = &ready.lines[*row];
            assert_eq!(line.spans.len(), 1);
            // `Line::styled` attaches the style to the Line itself
            // (ratatui patches it onto spans only at draw time).
            let style = line.style;
            assert!(
                matches!(style.fg, Some(ratatui::style::Color::Rgb(..))),
                "placeholder fg must carry the image id RGB: {style:?}"
            );
            assert_eq!(style.bg, None);
        }
        assert_eq!(plain(&ready.lines[ready.lines.len() - 1]), "After");
        engine.shutdown();
    }

    #[test]
    fn placeholder_rows_match_the_reserved_grid() {
        let (engine, renders, _dir) = test_engine();
        let source = r"\sum_{k=0}^{n}k^2=\frac{n(n+1)(2n+1)}{6}";
        assert!(matches!(
            engine.request(source, 40),
            ratatex::FormulaState::Pending
        ));
        assert!(wait_for(&renders, 1), "formula did not render in time");
        let ratatex::FormulaState::Ready(formula) = engine.request(source, 40) else {
            panic!("formula should be ready");
        };
        let lines = formula_to_lines(&formula);
        assert_eq!(lines.len(), usize::from(formula.rows()));
        for line in &lines {
            let symbols: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            // One placeholder per cell: a base U+10EEEE per cell (plus 2-3
            // combining diacritics, depending on the image id's high byte).
            assert!(symbols.starts_with('\u{10EEEE}'));
            assert_eq!(
                symbols.matches('\u{10EEEE}').count(),
                usize::from(formula.columns())
            );
        }
        engine.shutdown();
    }

    fn plain(line: &ratatui::text::Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}
