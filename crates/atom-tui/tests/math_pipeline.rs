//! End-to-end guard for the pinned `ratatex` render pipeline (0.1.0).
//!
//! Builds the same engine `math::init` builds and asserts that a display
//! formula reaches `FormulaState::Ready` with a PNG that actually contains
//! visible (non-transparent) pixels. A blank PNG would show up in the
//! transcript as an empty gap where the formula should be.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatex::{FormulaState, PixelSize, Ratatex, TerminalProfile};

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

#[test]
fn integral_renders_visible_pixels() {
    let (engine, renders, _dir) = test_engine();
    let source = r"\int_0^1 x^2 \, dx = \frac{1}{3}";
    assert!(matches!(
        engine.request(source, 60),
        FormulaState::Pending | FormulaState::Ready(_)
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while renders.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < deadline, "render never completed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let FormulaState::Ready(formula) = engine.request(source, 60) else {
        panic!("formula should be ready");
    };
    assert!(formula.columns() > 0 && formula.rows() > 0);

    // The cell-aligned PNG must not be blank: count alpha > 0 pixels.
    let image = image::load_from_memory(formula.png())
        .expect("decode formula png")
        .to_rgba8();
    let opaque = image.pixels().filter(|p| p.0[3] > 0).count();
    assert!(opaque > 0, "formula PNG is entirely transparent (blank)");

    engine.shutdown();
}
