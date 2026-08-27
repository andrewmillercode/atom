<div align="center">

<h1>Ratatex</h1>

<p><strong>TeX-quality display math that scrolls with your Ratatui UI.</strong></p>

[![CI](https://img.shields.io/github/actions/workflow/status/gakonst/ratatex/ci.yml?branch=main)][ci]
[![Crates.io](https://img.shields.io/crates/v/ratatex.svg)][crates]
[![Docs.rs](https://img.shields.io/docsrs/ratatex)][docs]
[![License](https://img.shields.io/badge/license-MIT-blue.svg)][license]

**[Try it](#try-it)** · **[Embed it](#embed-it)** ·
**[Streaming](#streaming-markdown)** · **[Selection](#copyable-latex)** ·
**[Scrolling](#scrolling-and-clipping)**

[ci]: https://github.com/gakonst/ratatex/actions/workflows/ci.yml
[crates]: https://crates.io/crates/ratatex
[docs]: https://docs.rs/ratatex
[license]: LICENSE

</div>

Ratatex turns Markdown display math into cell-aligned PNGs on bounded
background workers:

```text
Markdown display math
    → RaTeX parse + TeX layout in-process
    → embedded KaTeX fonts + tiny-skia at 3× resolution
    → antialiasing, padding, and cell alignment
    → content-addressed PNG cache
    → Kitty virtual placement + Unicode-placeholder cells
```

Rendering uses the open-source [RaTeX](https://github.com/erweixin/RaTeX)
parser, layout engine, embedded fonts, and rasterizer entirely in-process.
Ratatex never invokes a system TeX installation, writes DVI, or starts a
rendering subprocess. Unsupported expressions return a typed failure so the
host application can keep its text representation.

The placeholder cells live in the normal Ratatui buffer. Equations therefore
scroll and clip with the surrounding text instead of behaving like
cursor-positioned overlays.

## Install

```sh
cargo add ratatex
```

## Requirements

- A terminal implementing Kitty graphics Unicode placeholders.
- Ratatui 0.29.
- No TeX distribution or external rendering programs.

Ghostty, Kitty, WezTerm, and recent Konsole releases implement the graphics
protocol. When running inside tmux, enable graphics passthrough:

```tmux
set -g allow-passthrough on
```

## Try it

From a compatible terminal:

```sh
cargo run --example demo
```

Press `q`, `Esc`, or `Ctrl-C` to leave the demo.

## Embed it

Query the terminal after entering the alternate screen and before starting a
competing input reader. Build one renderer for the application, not one per
formula:

```rust,no_run
use std::{io::Write, time::Duration};

use ratatex::{FormulaState, FormulaWidget, Ratatex, TerminalProfile};
use ratatui::{Frame, layout::Rect};

let profile = TerminalProfile::query(Duration::from_millis(750));
let renderer = Ratatex::builder(profile)
    .on_update(|| {
        // Wake your application's event loop.
    })
    .build()?;

fn draw_formula(
    frame: &mut Frame<'_>,
    area: Rect,
    renderer: &Ratatex,
    tex: &str,
) {
    if let FormulaState::Ready(formula) = renderer.request(tex, area.width) {
        frame.render_widget(FormulaWidget::new(&formula), area);
    } else {
        // Keep a text fallback or a previously ready streaming frame.
    }
}

// Before the next Ratatui draw:
for command in renderer.drain_terminal_commands() {
    std::io::stdout().write_all(command.as_bytes())?;
}

# Ok::<(), std::io::Error>(())
```

`request()` is non-blocking. A cache miss returns `Pending`, renders in-process
on a worker, and invokes `on_update` when the result is ready. Subsequent runs
can load the content-addressed PNG directly from disk.

When the callback wakes your event loop:

1. Invalidate cached layout if `generation()` changed.
2. Drain terminal commands and write them before drawing their placeholders.
3. Draw the next Ratatui frame.

See [`examples/demo.rs`](examples/demo.rs) for a complete crossterm loop.

## Streaming Markdown

You do not need to wait for the model to close `\]`, `$$`, `aligned`, braces,
or `\left` before beginning a render. `heal_streaming_display_math()` returns a
temporary view with only the missing closing tokens appended:

```rust
use ratatex::{
    Formula, FormulaState, Ratatex, display_math, heal_streaming_display_math,
};
use std::sync::Arc;

fn update_streaming_formula(
    renderer: &Ratatex,
    markdown_prefix: &str,
    width: u16,
    last_ready: &mut Option<Arc<Formula>>,
) {
    let preview = heal_streaming_display_math(markdown_prefix);
    let Some(region) = display_math(&preview).last() else {
        return;
    };

    if let FormulaState::Ready(formula) = renderer.request(region.source(), width) {
        *last_ready = Some(formula);
    }
}
```

The helper never alters existing stream bytes. Keep the real Markdown for
copying, and keep displaying `last_ready` while a newer prefix is `Pending` or
temporarily unparsable. This makes a formula refine in place instead of showing
raw TeX and flashing into an image only at the end.

For fast streams, retain a short history of submitted prefixes. A slightly
older request may finish after a newer prefix has already arrived; accepting
that completed frame gives the user useful progressive output while the newest
request catches up.

`display_math()` and `markdown_segments()` recognize `$$…$$`, `\[…\]`, and
common AMS display environments while ignoring fenced and inline code. Ratatex
intentionally leaves prose, inline math, and Markdown styling to the host
application.

## Copyable LaTeX

For native terminal selection and copying, render the same widget with its
original display-math region during your application's selection pass:

```rust,ignore
let clicked_formula =
    ratatex::is_formula_placeholder(frame.buffer()[mouse_position].symbol());
let widget = FormulaWidget::new(&formula);
let widget = if clicked_formula || selection_intersects_formula {
    widget.compact_source_fallback(display_math.full_source())
} else {
    widget
};
frame.render_widget(widget, area);
```

Keep the fallback enabled after the first click while the user starts their
selection. The compact fallback keeps the formula's existing rows and folds
source newlines into spaces, so enabling it does not reflow the surrounding
transcript or clip a closing delimiter in common display blocks. Your normal
selection highlight can then operate on ordinary LaTeX cells instead of Kitty
placeholder characters. Use `source_fallback` when exact source whitespace is
more important and the application reserves enough rows for it.

## Scrolling and clipping

Reserve exactly `Formula::rows()` transcript rows. When part of the formula is
above the viewport, keep the formula's logical origin and pass its signed
offset to the widget:

```rust,ignore
use ratatex::{FormulaWidget, SignedPosition};

FormulaWidget::new(&formula)
    .position(SignedPosition::new(0, -rows_scrolled_off_top))
    .render(visible_area, frame.buffer_mut());
```

Do not slice the placeholder grid and redraw it from tile `(0, 0)`. Kitty
Unicode placeholders encode each tile's source row and column; preserving
those coordinates is what keeps partially visible equations intact while
scrolling up and down.

Call `reupload_all()` after recreating the alternate screen and when a tmux
pane regains focus. tmux retains the placeholder cells for hidden panes but
normally drops graphics passthrough sent while the pane is not visible.

## Rendering choices

The default is a transparent grayscale alpha mask with a neutral off-white
foreground. It works across terminal themes without guessing the background.
`ColorScheme` and `Antialiasing::{LcdRgb,LcdBgr}` provide LCD subpixel
rendering when the application knows the exact background color.

TeX input, decoded images, queues, and caches are bounded, and unsupported or
unsafe primitives are rejected before layout. Formula input is parsed as math;
it is never passed to a shell or external TeX process. Run untrusted rendering
in an OS sandbox when it requires a hard security boundary.

## Scope

Ratatex renders display math. It does not own your Markdown parser, transcript
model, scrolling state, input loop, or clipboard. The API is deliberately a
small composable layer: detection and streaming healing, an asynchronous
renderer/cache, terminal commands, and a clipping Ratatui widget.

The supported formula language is the KaTeX/AMS-style subset implemented by
RaTeX. Ratatex intentionally does not load arbitrary TeX packages or fall back
to a system TeX toolchain.

## License

MIT
