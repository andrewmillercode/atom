//! Knight Rider blocks scanner — the opencode-style loader bar.
//!
//! Ported from opencode packages/tui/src/ui/spinner.ts.
//! Used in the cwd footer row while a session is streaming.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::ansi;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Number of cells in the scanner bar.
const WIDTH: usize = 8;
/// Frames held at the left edge (after sweeping back).
const HOLD_START: usize = 14;
/// Frames held at the right edge (after sweeping forward).
const HOLD_END: usize = 9;
/// Gradient trail length (number of fading steps behind the lead).
const TRAIL_LEN: usize = 6;

/// Total frames in one full cycle: forward + hold_end + backward + hold_start.
pub const TOTAL_FRAMES: usize = WIDTH + HOLD_END + (WIDTH - 1) + HOLD_START;

const GLYPH_ACTIVE: &str = "■";
const GLYPH_INACTIVE: &str = "·";

// ---------------------------------------------------------------------------
// Trail color derivation (from deriveTrailColors in spinner.ts)
// ---------------------------------------------------------------------------

/// RGBA as f32 (0..1 for r,g,b,a)
#[derive(Clone, Copy)]
struct Rgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

fn derive_trail_colors(base: (u8, u8, u8)) -> [Rgba; TRAIL_LEN] {
    let br = base.0 as f32 / 255.0;
    let bg = base.1 as f32 / 255.0;
    let bb = base.2 as f32 / 255.0;

    let mut out = [Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    }; TRAIL_LEN];

    // Can't derive Copy for Rgba with f32 in const, just do it manually:
    for (i, slot) in out.iter_mut().enumerate() {
        let (alpha, bf): (f32, f32) = if i == 0 {
            (1.0, 1.0) // lead: full brightness
        } else if i == 1 {
            (0.9, 1.15) // bloom/glare
        } else {
            (0.65_f32.powi(i as i32 - 1), 1.0) // exponential decay
        };
        *slot = Rgba {
            r: (br * bf).min(1.0),
            g: (bg * bf).min(1.0),
            b: (bb * bf).min(1.0),
            a: alpha,
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Scanner state machine (from getScannerState)
// ---------------------------------------------------------------------------

struct ScannerState {
    active: usize,
    moving_forward: bool,
    holding: bool,
    hold_progress: usize,
}

fn scanner_state(frame: usize) -> ScannerState {
    let forward = WIDTH;
    let backward = WIDTH - 1;

    if frame < forward {
        return ScannerState {
            active: frame,
            moving_forward: true,
            holding: false,
            hold_progress: 0,
        };
    }
    let f = frame - forward;
    if f < HOLD_END {
        return ScannerState {
            active: WIDTH - 1,
            moving_forward: true,
            holding: true,
            hold_progress: f,
        };
    }
    let f = f - HOLD_END;
    if f < backward {
        return ScannerState {
            active: WIDTH - 2 - f,
            moving_forward: false,
            holding: false,
            hold_progress: 0,
        };
    }
    let f = f - backward;
    ScannerState {
        active: 0,
        moving_forward: false,
        holding: true,
        hold_progress: f,
    }
}

// ---------------------------------------------------------------------------
// Color index (from calculateColorIndex)
// ---------------------------------------------------------------------------

fn color_index(state: &ScannerState, char_idx: usize) -> i32 {
    let active = state.active as i32;
    let ci = char_idx as i32;
    let ddist = if state.moving_forward {
        active - ci
    } else {
        ci - active
    };

    if state.holding {
        return ddist + state.hold_progress as i32;
    }
    if ddist > 0 && ddist < TRAIL_LEN as i32 {
        return ddist;
    }
    if ddist == 0 {
        return 0;
    }
    -1
}

// ---------------------------------------------------------------------------
// Composite: blend RGBA over background
// ---------------------------------------------------------------------------

fn composite(color: &Rgba, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let a = color.a;
    let r = (color.r * a + bg.0 as f32 / 255.0 * (1.0 - a)) * 255.0;
    let g = (color.g * a + bg.1 as f32 / 255.0 * (1.0 - a)) * 255.0;
    let b = (color.b * a + bg.2 as f32 / 255.0 * (1.0 - a)) * 255.0;
    (r.round() as u8, g.round() as u8, b.round() as u8)
}

// ---------------------------------------------------------------------------
// Public: render one frame of the loader bar as a ratatui Line
// ---------------------------------------------------------------------------

/// Renders the Knight Rider loader bar for `frame` (0..TOTAL_FRAMES-1).
/// Uses the theme primary color for the trail, composited over the
/// terminal background. Appends "esc stop" hint.
pub fn loader_line(frame: usize) -> Line<'static> {
    let primary = ansi::c_muted();
    let base = match primary {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0x8C, 0xAD, 0xD1), // fallback: theme primary #8cadd1
    };

    let bg = match ansi::c_background() {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0x11, 0x11, 0x12), // fallback: theme background #111112
    };

    let trail = derive_trail_colors(base);
    let inactive_alpha = 0.6_f32;
    let inactive = Rgba {
        r: base.0 as f32 / 255.0,
        g: base.1 as f32 / 255.0,
        b: base.2 as f32 / 255.0,
        a: inactive_alpha,
    };

    let state = scanner_state(frame % TOTAL_FRAMES);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(WIDTH + 4);

    for ci in 0..WIDTH {
        let idx = color_index(&state, ci);
        let (glyph, rgb) = if idx >= 0 && (idx as usize) < TRAIL_LEN {
            let c = &trail[idx as usize];
            (GLYPH_ACTIVE, composite(c, bg))
        } else {
            (GLYPH_INACTIVE, composite(&inactive, bg))
        };
        spans.push(Span::styled(
            glyph.to_string(),
            Style::new().fg(Color::Rgb(rgb.0, rgb.1, rgb.2)),
        ));
    }

    // hint
    spans.push(Span::styled("  esc ", Style::new().fg(ansi::c_muted())));
    spans.push(Span::styled("stop", Style::new().fg(ansi::c_muted_extra())));

    Line::from(spans)
}
