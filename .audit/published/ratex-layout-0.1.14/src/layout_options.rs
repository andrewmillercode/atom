use ratex_font::{get_global_metrics, MathConstants};
use ratex_types::color::Color;
use ratex_types::math_style::MathStyle;

/// Layout options passed through the layout tree.
#[derive(Debug, Clone)]
pub struct LayoutOptions {
    pub style: MathStyle,
    pub color: Color,
    /// When set (e.g. in align/aligned), cap relation spacing to this many mu for consistency.
    pub align_relation_spacing: Option<f64>,
    /// When inside \\left...\\right, the stretch height for \\middle delimiters (second pass only).
    pub leftright_delim_height: Option<f64>,
    /// Extra horizontal kern between glyphs (em), e.g. for custom text tracking.
    pub inter_glyph_kern_em: f64,
    /// Explicit TeX sizing multiplier currently in force (`1.0` for `\normalsize`).
    ///
    /// This is layout state propagated through `\tiny` ... `\Huge`; callers
    /// should normally leave it at its default.
    pub explicit_size_multiplier: f64,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            style: MathStyle::Display,
            color: Color::BLACK,
            align_relation_spacing: None,
            leftright_delim_height: None,
            inter_glyph_kern_em: 0.0,
            explicit_size_multiplier: 1.0,
        }
    }
}

impl LayoutOptions {
    pub fn metrics(&self) -> &'static MathConstants {
        get_global_metrics(self.style.size_index())
    }

    pub fn size_multiplier(&self) -> f64 {
        self.style.size_multiplier()
    }

    pub fn with_style(&self, style: MathStyle) -> Self {
        Self {
            style,
            color: self.color,
            align_relation_spacing: self.align_relation_spacing,
            leftright_delim_height: self.leftright_delim_height,
            inter_glyph_kern_em: self.inter_glyph_kern_em,
            explicit_size_multiplier: self.explicit_size_multiplier,
        }
    }

    pub fn with_color(&self, color: Color) -> Self {
        Self {
            style: self.style,
            color,
            align_relation_spacing: self.align_relation_spacing,
            leftright_delim_height: self.leftright_delim_height,
            inter_glyph_kern_em: self.inter_glyph_kern_em,
            explicit_size_multiplier: self.explicit_size_multiplier,
        }
    }

    pub fn with_inter_glyph_kern(&self, em: f64) -> Self {
        Self {
            inter_glyph_kern_em: em,
            ..self.clone()
        }
    }

    pub(crate) fn with_explicit_size_multiplier(&self, multiplier: f64) -> Self {
        Self {
            explicit_size_multiplier: multiplier,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_inter_glyph_kern_option_survives_option_derivation() {
        let options = LayoutOptions::default()
            .with_inter_glyph_kern(0.02)
            .with_style(MathStyle::Script)
            .with_color(Color::new(0.1, 0.2, 0.3, 1.0));

        assert_eq!(options.inter_glyph_kern_em, 0.02);
        assert_eq!(options.explicit_size_multiplier, 1.0);
    }
}
