//! TeX-quality display math inside scrolling Ratatui interfaces.
//!
//! `ratatex` turns completed Markdown display-math regions into cell-aligned
//! PNGs on a bounded background worker. It parses, lays out, and rasterizes math
//! in-process with embedded fonts—without a system TeX installation or
//! rendering subprocess. The PNGs are cached on disk, uploaded through the
//! Kitty graphics protocol, and placed with Unicode-placeholder cells. Because
//! the placeholders are ordinary terminal cells, equations naturally scroll,
//! clip, and redraw with the rest of a Ratatui buffer.
//!
//! The crate deliberately leaves surrounding Markdown rendering to the
//! application. Use [`display_math`] to find renderable regions, call
//! [`Ratatex::request`] for each expression, reserve [`Formula::rows`] lines,
//! flush terminal commands when the update callback fires, and render a
//! [`FormulaWidget`] over the reserved cells.
//!
//! For a streaming document, pass a temporary
//! [`heal_streaming_display_math`] view to [`display_math`] and retain the
//! latest ready [`Formula`] while a newer prefix renders. When a formula is
//! partially outside the viewport, use [`SignedPosition`] rather than slicing
//! its placeholder grid; this preserves the Kitty source-tile coordinates.
//! [`FormulaWidget::compact_source_fallback`] swaps the same reserved cells
//! back to selectable LaTeX for a native terminal-selection pass.

mod config;
mod engine;
mod kitty;
mod markdown;
mod pipeline;
mod widget;

pub use config::{
    Antialiasing, ColorScheme, GraphicsSupport, Limits, PixelPadding, PixelSize, Rgb,
    TerminalProfile,
};
pub use engine::{
    Availability, Formula, FormulaState, Ratatex, RatatexBuilder, RenderFailure, RenderFailureKind,
    TerminalCommand,
};
pub use kitty::is_formula_placeholder;
pub use markdown::{
    DisplayMath, DisplayMathDelimiter, MarkdownSegment, display_math, heal_streaming_display_math,
    markdown_segments,
};
pub use widget::{FormulaWidget, SignedPosition, compact_latex};
