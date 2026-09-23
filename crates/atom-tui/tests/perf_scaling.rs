//! Scratch perf probe (not a regression test): measures where CPU scales
//! with session size. Run with:
//!   cargo test -p atom-tui --test perf_scaling -- --ignored --nocapture

use std::time::Instant;

use atom_tui::app::App;
use atom_tui::blocks::{render_block_linked, Block, BlockKind};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn big_markdown(paras: usize) -> String {
    let mut s = String::new();
    for i in 0..paras {
        s.push_str(&format!(
            "## Section {i}\n\nThis paragraph talks about buffering strategies and \
             rendering costs in terminal user interfaces, with `code spans` and \
             [links](https://example.com/{i}) sprinkled through several sentences \
             to exercise the wrapper and style pipeline realistically.\n\n- item one\n\
             - item two with longer text that wraps around the block width a bit\n\n"
        ));
    }
    s
}

#[test]
#[ignore]
fn probe() {
    let width = 100usize;

    // 1. Single re-render of one large streaming assistant block
    //    (this is the per-stream-event cost).
    for paras in [40usize, 160, 640] {
        let text = big_markdown(paras);
        let mut b = Block {
            kind: BlockKind::Assistant,
            text: text.clone(),
            ..Default::default()
        };
        let t = Instant::now();
        let rendered = render_block_linked(&mut b, width, false, "•", ".");
        let dt = t.elapsed();
        println!(
            "render_block_linked {:>6} chars -> {:>4} rows: {:>10.2?} ({:.1} µs/KB)",
            text.len(),
            rendered.lines.len(),
            dt,
            dt.as_micros() as f64 / (text.len() as f64 / 1024.0)
        );
    }

    // 2. Full streaming simulation: a long assistant answer arriving in
    //    256-byte chunks, re-rendering the whole block each event
    //    (what happens today on every stream chunk).
    let full = big_markdown(320);
    let chunks: Vec<&str> = {
        let mut v = Vec::new();
        let bytes = full.as_bytes();
        let step = 256;
        let mut i = 0;
        while i < bytes.len() {
            let end = (i + step).min(bytes.len());
            while !full.is_char_boundary(end.min(bytes.len())) && end < bytes.len() {
                let e = end + 1;
                let _ = e;
                break;
            }
            v.push(&full[..end.min(full.len())]);
            i = end;
        }
        v
    };
    let t = Instant::now();
    let mut b = Block {
        kind: BlockKind::Assistant,
        text: String::new(),
        ..Default::default()
    };
    for c in &chunks {
        b.text = (*c).to_string();
        let _ = render_block_linked(&mut b, width, false, "•", ".");
    }
    println!(
        "stream {} KB in {} chunks w/ full re-render per chunk: {:>10.2?} total",
        full.len() / 1024,
        chunks.len(),
        t.elapsed()
    );

    // 3. Full-frame draw cost on a big session, then again with a
    //    content-only change (worst case today: full redraw per event).
    let mut app = App::new_test(120, 40);
    for i in 0..300 {
        let kind = if i % 2 == 0 {
            BlockKind::User
        } else {
            BlockKind::Assistant
        };
        app.blocks.push(Block {
            kind,
            text: big_markdown(3),
            ..Default::default()
        });
    }
    let area = ratatui::layout::Rect::new(0, 0, 120, 40);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    let t = Instant::now();
    app.refresh_viewport();
    println!(
        "load {} blocks ({}k chars, {} lines): {:>10.2?}",
        app.blocks.len(),
        300 * big_markdown(3).len() / 1024,
        app.content_lines.len(),
        t.elapsed()
    );
    let t = Instant::now();
    let frames = 100;
    for _ in 0..frames {
        let _ = atom_tui::view::draw(&mut app, area, &mut buf);
    }
    println!(
        "view::draw full frame x{}: {:>10.2?} ({:.2} ms/frame)",
        frames,
        t.elapsed(),
        t.elapsed().as_secs_f64() * 1000.0 / frames as f64
    );

    // 4. Same frame through Terminal::draw (adds ratatui diff + backend).
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let t = Instant::now();
    for _ in 0..frames {
        term.draw(|f| {
            let _ = atom_tui::view::draw(&mut app, f.area(), f.buffer_mut());
        })
        .unwrap();
    }
    println!(
        "Terminal::draw (with cell diff) x{}: {:>10.2?} ({:.2} ms/frame)",
        frames,
        t.elapsed(),
        t.elapsed().as_secs_f64() * 1000.0 / frames as f64
    );
}
