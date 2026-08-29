//! Ported from atom3d.go: the Bohr-atom splash renderer (pure math →
//! ANSI strings). A nucleus plus three tilted orbits with moving
//! electrons, drawn into a width×height cell box using Braille glyphs.
//! The nucleus is muted, orbit dots are muted-extra, and each of the
//! three electrons carries its own accent: primary, secondary, and
//! syntax-type. Color runs are batched per cell run. Same
//! (width, height, t) yields the same glyphs.

use std::f64::consts::PI;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::colors::{
    ansi_fg, COLOR_MUTED, COLOR_MUTED_EXTRA, COLOR_PRIMARY, COLOR_SECONDARY, COLOR_SYNTAX_TYPE,
};

/// Minimum cell box for a recognizable Bohr-style atom. Smaller viewports
/// hide the mark rather than clipping the orbits.
pub const ATOM3D_MIN_WIDTH: i64 = 24;
pub const ATOM3D_MIN_HEIGHT: i64 = 10;
const VIEW_TILT: f64 = 0.42;
const PERSP: f64 = 0.38;
/// Fraction of the shorter viewport axis used as the orbit scale.
/// Small enough that the rings read as a Bohr atom in empty space
/// rather than filling the pane.
const SCALE: f64 = 0.18;
const ANSI_RESET: &str = "\x1b[0m";

/// Paint layers of the Braille grid, ordered by draw priority (higher
/// wins when dots collide). Orbit paths are muted-extra, the nucleus is
/// muted, and the three electrons are PRIMARY / SECONDARY / SYNTAX.
const LAYER_ORBIT: u8 = 0;
const LAYER_NUCLEUS: u8 = 1;
const LAYER_PRIMARY: u8 = 2;
const LAYER_SECONDARY: u8 = 3;
const LAYER_SYNTAX: u8 = 4;

#[derive(Clone, Copy)]
struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vec3 {
    const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }

    fn rot_x(&self, a: f64) -> Vec3 {
        let (s, c) = a.sin_cos();
        Vec3 {
            x: self.x,
            y: self.y * c - self.z * s,
            z: self.y * s + self.z * c,
        }
    }

    #[allow(dead_code)]
    fn rot_y(&self, a: f64) -> Vec3 {
        let (s, c) = a.sin_cos();
        Vec3 {
            x: self.x * c + self.z * s,
            y: self.y,
            z: -self.x * s + self.z * c,
        }
    }

    fn rot_z(&self, a: f64) -> Vec3 {
        let (s, c) = a.sin_cos();
        Vec3 {
            x: self.x * c - self.y * s,
            y: self.x * s + self.y * c,
            z: self.z,
        }
    }

    /// spinView applies the per-frame rotY(spin) then rotX(view) using
    /// precomputed sines so orbit samples skip four trig calls each.
    fn spin_view(&self, sin_s: f64, cos_s: f64, sin_v: f64, cos_v: f64) -> Vec3 {
        let x1 = self.x * cos_s + self.z * sin_s;
        let z1 = -self.x * sin_s + self.z * cos_s;
        Vec3 {
            x: x1,
            y: self.y * cos_v - z1 * sin_v,
            z: self.y * sin_v + z1 * cos_v,
        }
    }
}

/// brailleGrid is a cell grid where each terminal cell holds 2×4 dots
/// (U+2800 block). Terminal cells are ~2:1 tall, so those dots are close
/// to square without extra squash.
struct BrailleGrid {
    w: usize,
    h: usize,
    orbit: Vec<u8>,
    nucleus: Vec<u8>,
    prim: Vec<u8>,
    sec: Vec<u8>,
    syn: Vec<u8>,
}

impl BrailleGrid {
    fn new(w: usize, h: usize) -> Self {
        BrailleGrid {
            w,
            h,
            orbit: vec![0; w * h],
            nucleus: vec![0; w * h],
            prim: vec![0; w * h],
            sec: vec![0; w * h],
            syn: vec![0; w * h],
        }
    }

    fn set(&mut self, x: i64, y: i64, layer: u8) {
        if x < 0 || y < 0 {
            return;
        }
        let (cx, cy) = (x / 2, y / 4);
        if cx as usize >= self.w || cy as usize >= self.h {
            return;
        }
        let bit = braille_dot((x % 2) as usize, (y % 4) as usize);
        let i = cy as usize * self.w + cx as usize;
        match layer {
            LAYER_ORBIT => self.orbit[i] |= bit,
            LAYER_NUCLEUS => self.nucleus[i] |= bit,
            LAYER_PRIMARY => self.prim[i] |= bit,
            LAYER_SECONDARY => self.sec[i] |= bit,
            LAYER_SYNTAX => self.syn[i] |= bit,
            _ => {}
        }
    }

    fn set_disk(&mut self, cx: f64, cy: f64, r: f64, layer: u8) {
        let ir = r.ceil() as i64;
        let icx = cx.round() as i64;
        let icy = cy.round() as i64;
        let r2 = r * r;
        for dy in -ir..=ir {
            for dx in -ir..=ir {
                if (dx * dx + dy * dy) as f64 <= r2 {
                    self.set(icx + dx, icy + dy, layer);
                }
            }
        }
    }
}

fn braille_dot(dx: usize, dy: usize) -> u8 {
    match dy {
        0 => {
            if dx == 0 {
                0x01
            } else {
                0x08
            }
        }
        1 => {
            if dx == 0 {
                0x02
            } else {
                0x10
            }
        }
        2 => {
            if dx == 0 {
                0x04
            } else {
                0x20
            }
        }
        _ => {
            if dx == 0 {
                0x40
            } else {
                0x80
            }
        }
    }
}

impl BrailleGrid {
    /// Renders the grid: batched color runs per row, trailing blank
    /// columns trimmed, no trailing newline. Layer priority on overlap:
    /// syntax > secondary > primary > nucleus > orbit.
    fn render(&self) -> String {
        let orbit = ansi_fg(COLOR_MUTED_EXTRA);
        let nucleus = ansi_fg(COLOR_MUTED);
        let prim = ansi_fg(COLOR_PRIMARY);
        let sec = ansi_fg(COLOR_SECONDARY);
        let syn = ansi_fg(COLOR_SYNTAX_TYPE);
        let mut b = String::with_capacity(self.h * (self.w * 3 + 1) + 64);
        // 0 none, 1 orbit, 2 nucleus, 3 primary, 4 secondary, 5 syntax
        let mut style = 0u8;
        for y in 0..self.h {
            if y > 0 {
                if style != 0 {
                    b.push_str(ANSI_RESET);
                    style = 0;
                }
                b.push('\n');
            }
            let mut row_end = self.w;
            while row_end > 0 {
                let i = y * self.w + row_end - 1;
                if self.orbit[i] | self.nucleus[i] | self.prim[i] | self.sec[i] | self.syn[i] != 0 {
                    break;
                }
                row_end -= 1;
            }
            for x in 0..row_end {
                let i = y * self.w + x;
                let bits =
                    self.orbit[i] | self.nucleus[i] | self.prim[i] | self.sec[i] | self.syn[i];
                if bits == 0 {
                    if style != 0 {
                        b.push_str(ANSI_RESET);
                        style = 0;
                    }
                    b.push(' ');
                    continue;
                }
                let want = if self.syn[i] != 0 {
                    5u8
                } else if self.sec[i] != 0 {
                    4u8
                } else if self.prim[i] != 0 {
                    3u8
                } else if self.nucleus[i] != 0 {
                    2u8
                } else {
                    1u8
                };
                if style != want {
                    if style != 0 {
                        b.push_str(ANSI_RESET);
                    }
                    match want {
                        3 => b.push_str(&prim),
                        4 => b.push_str(&sec),
                        5 => b.push_str(&syn),
                        2 => b.push_str(&nucleus),
                        _ => b.push_str(&orbit),
                    }
                    style = want;
                }
                b.push(char::from_u32(0x2800 + bits as u32).unwrap());
            }
        }
        if style != 0 {
            b.push_str(ANSI_RESET);
        }
        b
    }
}

struct AtomRing {
    radius: f64,
    tilt: f64,
    twist: f64,
    phase: f64,
    speed: f64,
    /// Braille layer painting this ring's electron (primary/secondary/syntax).
    color: u8,
}

static ATOM3D_RINGS: Lazy<Vec<AtomRing>> = Lazy::new(|| {
    vec![
        AtomRing {
            radius: 1.00,
            tilt: 1.20,
            twist: 0.15,
            phase: 0.4,
            speed: 1.65,
            color: LAYER_PRIMARY,
        },
        AtomRing {
            radius: 0.78,
            tilt: -0.95,
            twist: 2.05,
            phase: 2.3,
            speed: 1.25,
            color: LAYER_SECONDARY,
        },
        AtomRing {
            radius: 0.56,
            tilt: 0.40,
            twist: 3.70,
            phase: 4.1,
            speed: 2.05,
            color: LAYER_SYNTAX,
        },
    ]
});

/// atom3DRingPts[i] is ring i after tilt/twist, before the per-frame spin.
static ATOM3D_RING_PTS: Lazy<Vec<Vec<Vec3>>> = Lazy::new(|| {
    ATOM3D_RINGS
        .iter()
        .map(|ring| {
            // The atom is small on screen; ~50 samples keep the ellipses
            // round without the old 100+ segment stroke cost.
            let n = 36 + (ring.radius * 24.0) as usize;
            let mut pts = Vec::with_capacity(n + 1);
            for i in 0..=n {
                let a = 2.0 * PI * i as f64 / n as f64;
                let p = Vec3::new(ring.radius * a.cos(), ring.radius * a.sin(), 0.0);
                pts.push(p.rot_x(ring.tilt).rot_z(ring.twist));
            }
            pts
        })
        .collect()
});

#[derive(Clone)]
struct Frame {
    w: i64,
    h: i64,
    q: i64,
    s: String,
}

static ATOM3D_LAST: Lazy<Mutex<Option<Frame>>> = Lazy::new(|| Mutex::new(None));

/// renderAtom3D draws a Bohr-style atom (nucleus + three tilted orbits
/// and moving electrons) into a width×height cell box using Braille.
/// Same (width, height, t) yields the same glyphs. Returns "" when the
/// box is too small to read as an atom.
pub fn render_atom3d(width: i64, height: i64, t: f64) -> String {
    if width < ATOM3D_MIN_WIDTH || height < ATOM3D_MIN_HEIGHT {
        return String::new();
    }
    let q = (t * 30.0).round() as i64; // match the 30 fps splash ticker
    {
        let last = ATOM3D_LAST.lock().unwrap();
        if let Some(f) = last.as_ref() {
            if f.w == width && f.h == height && f.q == q && !f.s.is_empty() {
                return f.s.clone();
            }
        }
    }

    let dot_w = (width * 2) as f64;
    let dot_h = (height * 4) as f64;
    let ox = (dot_w - 1.0) / 2.0;
    let oy = (dot_h - 1.0) / 2.0;
    // Geometric mean biased toward width (0.6/0.4) so the atom stays a
    // little larger when the pane gets short; a plain sqrt (0.5/0.5)
    // shrinks too fast with height.
    let scale = dot_w.powf(0.6) * dot_h.powf(0.4) * SCALE;

    let spin = t * 0.85;
    let (sin_s, cos_s) = spin.sin_cos();
    let (sin_v, cos_v) = VIEW_TILT.sin_cos();

    let project = |v: Vec3| -> (f64, f64) {
        let mut d = 1.0 + v.z * PERSP;
        if d < 0.25 {
            d = 0.25;
        }
        (ox + v.x / d * scale, oy - v.y / d * scale)
    };

    let mut g = BrailleGrid::new(width as usize, height as usize);

    for pts in ATOM3D_RING_PTS.iter() {
        let mut prev = (0.0f64, 0.0f64);
        let mut have_prev = false;
        for p in pts {
            let (x, y) = project(p.spin_view(sin_s, cos_s, sin_v, cos_v));
            if have_prev {
                stroke(&mut g, prev.0, prev.1, x, y, LAYER_ORBIT);
            }
            prev = (x, y);
            have_prev = true;
        }
    }

    let (nx, ny) = project(Vec3::ZERO.spin_view(sin_s, cos_s, sin_v, cos_v));
    g.set_disk(nx, ny, 2.4, LAYER_NUCLEUS);

    for ring in ATOM3D_RINGS.iter() {
        let a = t * ring.speed + ring.phase;
        let (sa, ca) = a.sin_cos();
        let p = Vec3::new(ring.radius * ca, ring.radius * sa, 0.0)
            .rot_x(ring.tilt)
            .rot_z(ring.twist);
        let (x, y) = project(p.spin_view(sin_s, cos_s, sin_v, cos_v));
        g.set_disk(x, y, 1.35, ring.color);
    }

    let s = g.render();
    *ATOM3D_LAST.lock().unwrap() = Some(Frame {
        w: width,
        h: height,
        q,
        s: s.clone(),
    });
    s
}

fn stroke(g: &mut BrailleGrid, x0: f64, y0: f64, x1: f64, y1: f64, layer: u8) {
    let mut x = x0.round() as i64;
    let mut y = y0.round() as i64;
    let x1i = x1.round() as i64;
    let y1i = y1.round() as i64;
    let mut dx = x1i - x;
    let mut dy = y1i - y;
    if dx < 0 {
        dx = -dx;
    }
    if dy < 0 {
        dy = -dy;
    }
    let sx = if x > x1i { -1 } else { 1 };
    let sy = if y > y1i { -1 } else { 1 };
    let mut err = dx - dy;
    loop {
        g.set(x, y, layer);
        if x == x1i && y == y1i {
            return;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_braille_glyph(s: &str) -> bool {
        s.chars().any(|c| ('\u{2801}'..='\u{28FF}').contains(&c))
    }

    fn strip(ansi: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in ansi.chars() {
            if c == '\x1b' {
                esc = true;
                continue;
            }
            if esc {
                if c.is_ascii_alphabetic() || c == '\\' {
                    esc = false;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn too_small_is_empty() {
        assert_eq!(render_atom3d(ATOM3D_MIN_WIDTH - 1, 30, 0.0), "");
        assert_eq!(render_atom3d(80, ATOM3D_MIN_HEIGHT - 1, 0.0), "");
    }

    #[test]
    fn contains_braille() {
        let got = strip(&render_atom3d(80, 24, 0.0));
        assert!(
            has_braille_glyph(&got),
            "large canvas missing braille: {:?}",
            got
        );
        assert!(!got.trim().is_empty());
    }

    #[test]
    fn animates_deterministically() {
        let a = strip(&render_atom3d(80, 24, 0.0));
        let b = strip(&render_atom3d(80, 24, 0.8));
        assert_ne!(a, b, "expected different frames at t=0 and t=0.8");
        assert_eq!(
            a,
            strip(&render_atom3d(80, 24, 0.0)),
            "should be deterministic for the same t"
        );
    }

    #[test]
    fn batches_color_runs() {
        let got = render_atom3d(80, 24, 0.0);
        let glyphs = strip(&got)
            .chars()
            .filter(|c| ('\u{2801}'..='\u{28FF}').contains(c))
            .count();
        assert!(glyphs > 0);
        let opens = got.matches("\x1b[38;2").count();
        assert!(
            opens < glyphs,
            "color is per-cell ({} SGR for {} glyphs), want batched runs",
            opens,
            glyphs
        );
    }

    #[test]
    fn electrons_use_primary_secondary_and_syntax_colors() {
        let got = render_atom3d(80, 24, 0.0);
        for (name, hex) in [
            ("primary", COLOR_PRIMARY),
            ("secondary", COLOR_SECONDARY),
            ("syntax type", COLOR_SYNTAX_TYPE),
        ] {
            assert!(
                got.contains(&ansi_fg(hex)),
                "expected {name} electron ({hex}) in atom3d output"
            );
        }
    }

    #[test]
    fn nucleus_muted_and_orbits_muted_extra() {
        let got = render_atom3d(80, 24, 0.0);
        assert!(
            got.contains(&ansi_fg(COLOR_MUTED_EXTRA)),
            "expected muted-extra orbits in atom3d output"
        );
        assert!(
            got.contains(&ansi_fg(COLOR_MUTED)),
            "expected muted nucleus in atom3d output"
        );
    }
}
