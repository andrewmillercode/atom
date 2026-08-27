use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::Formula;

/// Collapses runs of source whitespace into single spaces.
///
/// This keeps display delimiters and TeX tokens intact while making a source
/// fallback fit into roughly the same terminal rows as its rendered formula.
#[must_use]
pub fn compact_latex(source: &str) -> String {
    let mut compact = String::with_capacity(source.len());
    for part in source.split_whitespace() {
        if !compact.is_empty() {
            compact.push(' ');
        }
        compact.push_str(part);
    }
    compact
}

/// Signed formula origin relative to a widget viewport.
///
/// Negative values clip rows or columns that have scrolled above or left of
/// the viewport while preserving their original Kitty tile coordinates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SignedPosition {
    /// Horizontal cell offset.
    pub x: i32,
    /// Vertical cell offset.
    pub y: i32,
}

impl SignedPosition {
    /// Creates a signed position.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl From<(i32, i32)> for SignedPosition {
    fn from((x, y): (i32, i32)) -> Self {
        Self::new(x, y)
    }
}

/// A clipping Ratatui widget for one prepared formula.
#[must_use]
pub struct FormulaWidget<'a> {
    formula: &'a Formula,
    position: SignedPosition,
    source_fallback: Option<&'a str>,
    compact_source_fallback: bool,
    source_style: Style,
}

impl<'a> FormulaWidget<'a> {
    /// Creates a widget at the top-left of its render area.
    pub const fn new(formula: &'a Formula) -> Self {
        Self {
            formula,
            position: SignedPosition::new(0, 0),
            source_fallback: None,
            compact_source_fallback: false,
            source_style: Style::new(),
        }
    }

    /// Sets the signed formula origin relative to the render area.
    pub const fn position(mut self, position: SignedPosition) -> Self {
        self.position = position;
        self
    }

    /// Renders source text instead of graphics inside the formula's reserved
    /// rows.
    ///
    /// This is intended for a selection/copy render pass: pass the original
    /// Markdown display region, including delimiters, so a terminal selection
    /// copies useful LaTeX. Omitting this method keeps the normal graphical
    /// rendering.
    pub const fn source_fallback(mut self, source: &'a str) -> Self {
        self.source_fallback = Some(source);
        self.compact_source_fallback = false;
        self
    }

    /// Renders a whitespace-normalized source fallback instead of graphics.
    ///
    /// This is useful when the original display region has more source lines
    /// than the rendered formula has terminal rows. Delimiters and TeX tokens
    /// remain selectable, while runs of whitespace become one space so the
    /// closing delimiter is much less likely to be clipped.
    pub const fn compact_source_fallback(mut self, source: &'a str) -> Self {
        self.source_fallback = Some(source);
        self.compact_source_fallback = true;
        self
    }

    /// Sets the style used by [`Self::source_fallback`].
    pub const fn source_fallback_style(mut self, style: Style) -> Self {
        self.source_style = style;
        self
    }
}

impl Widget for FormulaWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        if let Some(source) = self.source_fallback {
            self.render_source_fallback(source, area, buffer);
            return;
        }
        let [_, red, green, blue] = self.formula.image_id().to_be_bytes();
        let image_color = Color::Rgb(red, green, blue);

        for row in 0..self.formula.rows() {
            let relative_y = self.position.y.saturating_add(i32::from(row));
            if relative_y < 0 || relative_y >= i32::from(area.height) {
                continue;
            }
            for column in 0..self.formula.columns() {
                let relative_x = self.position.x.saturating_add(i32::from(column));
                if relative_x < 0 || relative_x >= i32::from(area.width) {
                    continue;
                }
                let Ok(x) = u16::try_from(relative_x) else {
                    continue;
                };
                let Ok(y) = u16::try_from(relative_y) else {
                    continue;
                };
                let screen_x = area.x.saturating_add(x);
                let screen_y = area.y.saturating_add(y);
                if let Some(cell) = buffer.cell_mut((screen_x, screen_y)) {
                    cell.set_symbol(self.formula.placeholder(row, column));
                    cell.set_fg(image_color);
                    cell.modifier = Modifier::empty();
                    cell.set_skip(false);
                }
            }
        }
    }
}

impl FormulaWidget<'_> {
    fn render_source_fallback(self, source: &str, area: Rect, buffer: &mut Buffer) {
        let compact_source = self.compact_source_fallback.then(|| compact_latex(source));
        let source = compact_source.as_deref().unwrap_or(source);
        let top = self.position.y;
        let bottom = top.saturating_add(i32::from(self.formula.rows()));
        let visible_top = top.max(0);
        let visible_bottom = bottom.min(i32::from(area.height));
        let left = self.position.x;
        let visible_left = left.max(0);
        if visible_top >= visible_bottom || visible_left >= i32::from(area.width) {
            return;
        }
        let Ok(x) = u16::try_from(visible_left) else {
            return;
        };
        let Ok(y) = u16::try_from(visible_top) else {
            return;
        };
        let Ok(height) = u16::try_from(visible_bottom.saturating_sub(visible_top)) else {
            return;
        };
        let vertical_scroll = u16::try_from(visible_top.saturating_sub(top)).unwrap_or(u16::MAX);
        let horizontal_scroll =
            u16::try_from(visible_left.saturating_sub(left)).unwrap_or(u16::MAX);
        Paragraph::new(source)
            .style(self.source_style)
            .wrap(Wrap { trim: false })
            .scroll((vertical_scroll, horizontal_scroll))
            .render(
                Rect::new(
                    area.x.saturating_add(x),
                    area.y.saturating_add(y),
                    area.width.saturating_sub(x),
                    height,
                ),
                buffer,
            );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

    use super::{FormulaWidget, SignedPosition, compact_latex};
    use crate::Formula;

    #[test]
    fn clipping_preserves_source_tile_coordinates() {
        let formula = Formula::for_widget_test(0x01_02_03, 3, 2);
        let area = Rect::new(5, 7, 2, 1);
        let mut buffer = Buffer::empty(area);

        FormulaWidget::new(&formula)
            .position(SignedPosition::new(-1, -1))
            .render(area, &mut buffer);

        assert_eq!(buffer[(5, 7)].symbol(), formula.placeholder(1, 1));
        assert_eq!(buffer[(6, 7)].symbol(), formula.placeholder(1, 2));
        assert_eq!(buffer[(5, 7)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(6, 7)].fg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn source_fallback_preserves_copyable_latex_without_placeholder_cells() {
        let formula = Formula::for_widget_test(0x01_02_03, 6, 2);
        let area = Rect::new(3, 4, 14, 2);
        let mut buffer = Buffer::empty(area);

        FormulaWidget::new(&formula)
            .position(SignedPosition::new(2, 0))
            .source_fallback(r"$$x^2+y^2$$")
            .render(area, &mut buffer);

        let first_row = (area.x..area.right())
            .map(|x| buffer[(x, area.y)].symbol())
            .collect::<String>();
        assert!(first_row.contains(r"$$x^2+y^2$$"));
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().starts_with('\u{10eeee}'))
        );
    }

    #[test]
    fn clipped_source_fallback_scrolls_with_the_formula() {
        let formula = Formula::for_widget_test(0x01_02_03, 8, 3);
        let area = Rect::new(0, 0, 12, 2);
        let mut buffer = Buffer::empty(area);

        FormulaWidget::new(&formula)
            .position(SignedPosition::new(0, -1))
            .source_fallback("first\nsecond\nthird")
            .render(area, &mut buffer);

        let rows = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows[0].starts_with("second"));
        assert!(rows[1].starts_with("third"));
    }

    #[test]
    fn compact_source_fallback_keeps_both_display_delimiters_visible() {
        let formula = Formula::for_widget_test(0x01_02_03, 24, 2);
        let area = Rect::new(0, 0, 24, 2);
        let mut buffer = Buffer::empty(area);

        FormulaWidget::new(&formula)
            .compact_source_fallback("$$\n\\nabla\\cdot u=0\n$$")
            .render(area, &mut buffer);

        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(r"$$ \nabla\cdot u=0 $$"));
        assert_eq!(compact_latex("$$\n  x +  y\n$$"), "$$ x + y $$");
    }
}
