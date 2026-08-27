//! Mermaid SVG post-processing for faithful rasterization.
//!
//! atom renders Mermaid diagrams headlessly (`merman-cli`, or `mmdc`)
//! and paints them in the TUI via resvg. Mermaid's edge-label groups
//! come in two variants, and at least one renderer gets the geometry
//! wrong, so both are normalized to a single correct shape before the
//! SVG is written to disk or rasterized:
//!
//! ```text
//! <g class="edgeLabel" transform="translate(CX,CY)">  <- edge midpoint
//!   <g class="label" ... transform="translate(TX,TY)">
//!     <g>
//!       <rect class="background" x="XR" ... width="W"/>
//!       <text text-anchor="middle"><tspan x="0" ...
//! ```
//!
//! The label text rows anchor at x=0 of the inner `g`, so the text
//! center lands at TX; the rect center lands at TX + XR + W/2. Both
//! should be 0, i.e. centered on the edge midpoint. Two observed
//! shapes:
//!
//! - merman-cli 0.7.0: TX = -W/2, XR = -2 → rect correct, text off by
//!   -W/2. A 204px edge label lands 102px left of the edge midpoint,
//!   its head sliding underneath the source node (nodes paint after
//!   edge labels).
//! - real Mermaid: TX = 0, XR = -2 → text correct, rect off by
//!   +W/2 - 2 (the fill drifts right over the label text).
//!
//! The normalization rewrites TX → 0 and XR → -(W/2 + 2), centering
//! both text and rect on the edge midpoint while keeping Mermaid's
//! padding convention (rect center 2px left of the text anchor, the
//! `x="-2"` both shapes emit). Vertical placement is untouched: TY and
//! the rect's y/height already optically center the rows.
//!
//! Anything that does not match the edge-label shape passes through
//! untouched: unsized node-label rects (`style="stroke: none"`), empty
//! labels (no rect), node-label groups (a bare `<rect/>` sits between
//! the label g and the background rect), and labels whose text carries
//! its own x or transform. Exotic diagram types are therefore never
//! corrupted by a pattern they do not use.

/// normalize_edge_labels centers every edge-label background rect and
/// its text on the edge midpoint. Idempotent: normalizing already
/// normalized output is a no-op. See the module docs for the geometry.
pub fn normalize_edge_labels(svg: &str) -> String {
    use std::sync::OnceLock;
    static LABEL_RE: OnceLock<regex::Regex> = OnceLock::new();
    // The label group: <g class="label" ...> — the open tag is captured
    // whole (its transform is parsed out separately, so a lazy optional
    // group cannot swallow it), then an optional bare inner <g>, a sized
    // background rect, and the label <text> whose first row anchors at
    // x="0".
    let re = LABEL_RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r#"(?s)(<g class="label"[^>]*>)((?:\s*<g>\s*)?)"#,
            r#"<rect class="background"([^>]*?)/>"#,
            r#"(\s*<text[^>]*>)(\s*<tspan[^>]*?x="0")"#
        ))
        .unwrap()
    });

    re.replace_all(svg, |caps: &regex::Captures| {
        let open = &caps[1];
        let middle = &caps[2];
        let rect_attrs = &caps[3];
        let text_el = &caps[4];
        let tspan_head = &caps[5];

        // Text with explicit positioning is not a shape we understand;
        // rewriting the group would corrupt it. Leave it alone. (Only the
        // <text> element is checked — the row tspan's own x="0" is part of
        // the shape.)
        if text_el.contains(" x=\"") || text_el.contains("transform=") {
            return caps[0].to_string();
        }
        // Only sized background rects are edge labels; the node-label
        // rect (`style="stroke: none"`) has no width and stays put.
        let Some(width) = attr_num(rect_attrs, "width") else {
            return caps[0].to_string();
        };

        // Text rows anchor at x=0, so the label g's translate x must be 0
        // for them to center on the edge midpoint. Vertical placement is
        // preserved. No transform at all: none is added.
        let open = recentre_translate(open).unwrap_or_else(|| open.to_string());
        let new_x = format_num(-(width / 2.0 + 2.0));
        let mut out = String::with_capacity(caps[0].len() + 16);
        out.push_str(&open);
        out.push_str(middle);
        out.push_str(r#"<rect class="background""#);
        out.push_str(&format!(r#" x="{new_x}""#));
        out.push_str(&strip_attr(rect_attrs, "x"));
        out.push_str("/>");
        out.push_str(text_el);
        out.push_str(tspan_head);
        out
    })
    .into_owned()
}

/// recentre_translate rewrites the first `transform="translate(TX,TY)"`
/// inside a tag's open so TX becomes 0, keeping TY and every other
/// attribute (and any transforms after the translate) byte-identical.
/// Returns None when the tag carries no translate we understand.
fn recentre_translate(tag: &str) -> Option<String> {
    let key = r#"transform="translate("#;
    let at = tag.find(key)?;
    let rest = &tag[at + key.len()..];
    let comma = rest.find(',')?;
    // TX must be numeric — anything else is a shape we don't touch.
    rest[..comma].trim().parse::<f64>().ok()?;
    let close = rest.find(')')?;
    let ty = &rest[comma + 1..close];
    let end = rest.find('"')?;
    Some(format!(
        "{}transform=\"translate(0,{})\"{}",
        &tag[..at],
        ty,
        &rest[end + 1..]
    ))
}

/// attr_num reads a numeric attribute out of an attribute string.
fn attr_num(attrs: &str, name: &str) -> Option<f64> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    rest[..end].trim().parse::<f64>().ok()
}

/// strip_attr removes one attribute (` name="..."`) from an attribute
/// string, leaving everything else — including attribute order — alone.
fn strip_attr(attrs: &str, name: &str) -> String {
    let needle = format!(" {name}=\"");
    let Some(start) = attrs.find(&needle) else {
        return attrs.to_string();
    };
    let rest = &attrs[start + needle.len()..];
    match rest.find('"') {
        Some(end) => format!("{}{}", &attrs[..start], &rest[end + 1..]),
        None => attrs.to_string(),
    }
}

/// format_num renders a coordinate the way Mermaid emits them: no
/// trailing zeros, no float noise beyond 4 decimal places.
fn format_num(v: f64) -> String {
    let rounded = (v * 10_000.0).round() / 10_000.0;
    let s = format!("{rounded}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// merman-cli 0.7.0 shape (trimmed from a real render of an
    /// `A -- "a hidden socket file:<br/>~/.local/share/atom/atom.sock"
    /// --> B` edge): inner g translated by -W/2, rect at x=-2 — the
    /// text lands W/2 left of the edge midpoint, under the source node.
    const MERMAN_SHAPE: &str = r#"<g class="edgeLabel" transform="translate(518.32421875,193.1)"><g class="label" data-id="L_TUI_SRV_0" transform="translate(-101.94140625,-29.1)"><g><rect class="background" style="" x="-2" y="-1" width="203.8828125" height="58.2"/><text y="-10.1" style="" text-anchor="middle"><tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em" text-anchor="middle"><tspan font-style="normal" class="text-inner-tspan" font-weight="normal">a</tspan><tspan font-style="normal" class="text-inner-tspan" font-weight="normal"> hidden</tspan></tspan></text></g></g></g>"#;

    /// Real-Mermaid shape: inner g at (0,0), rect at x=-2 — the rect
    /// drifts W/2 right of the text anchor.
    const MERMAID_SHAPE: &str = r#"<g class="edgeLabel" transform="translate(372.71,103.5)"><g class="label" data-id="L_D_B_0" transform="translate(0,-11.5)"><g><rect class="background" style="" x="-2" y="-1" width="22.789" height="23"/><text y="-10.1" style="" text-anchor="middle"><tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em" text-anchor="middle"><tspan font-style="normal" class="text-inner-tspan" font-weight="normal">No</tspan></tspan></text></g></g></g>"#;

    #[test]
    fn merman_label_text_moves_to_edge_midpoint() {
        let fixed = normalize_edge_labels(MERMAN_SHAPE);
        // Inner g loses its -W/2 x-offset (vertical -29.1 preserved).
        assert!(
            fixed.contains(r#"transform="translate(0,-29.1)""#),
            "{fixed}"
        );
        // Rect re-centered with Mermaid's 2px padding: -(203.8828125/2+2).
        assert!(fixed.contains(r#"x="-103.9414""#), "{fixed}");
        // Rect geometry besides x is untouched.
        assert!(
            fixed.contains(r#"width="203.8828125" height="58.2""#),
            "{fixed}"
        );
        // Text untouched.
        assert!(
            fixed.contains(r#"<text y="-10.1" style="" text-anchor="middle">"#),
            "{fixed}"
        );
        assert!(fixed.contains(">a</tspan>"), "{fixed}");
        // data-id survives.
        assert!(fixed.contains(r#"data-id="L_TUI_SRV_0""#), "{fixed}");
    }

    #[test]
    fn mermaid_label_rect_moves_onto_text() {
        let fixed = normalize_edge_labels(MERMAID_SHAPE);
        // Inner g was already correct: translate(0,-11.5) preserved.
        assert!(
            fixed.contains(r#"transform="translate(0,-11.5)""#),
            "{fixed}"
        );
        // Rect: -(22.789/2 + 2) = -13.3945.
        assert!(fixed.contains(r#"x="-13.3945""#), "{fixed}");
        assert!(fixed.contains(r#"width="22.789""#), "{fixed}");
        assert!(fixed.contains(">No</tspan>"), "{fixed}");
    }

    #[test]
    fn label_without_transform_only_fixes_rect() {
        let svg = r#"<g class="label"><rect class="background" x="-2" y="-1" width="20" height="23"/><text y="-10.1" text-anchor="middle"><tspan x="0">No</tspan></text></g>"#;
        let fixed = normalize_edge_labels(svg);
        // No transform existed, so none is added; rect centered on the
        // text anchor at x=0: -(20/2+2) = -12.
        assert!(!fixed.contains("transform="), "{fixed}");
        assert!(fixed.contains(r#"x="-12""#), "{fixed}");
        assert!(fixed.contains(r#"width="20""#), "{fixed}");
    }

    #[test]
    fn label_with_width_before_x_is_handled() {
        let svg = r#"<g class="label" transform="translate(-10,-11.5)"><g><rect class="background" height="23" y="-1" width="20" x="-2"/><text y="-10.1" text-anchor="middle"><tspan x="0">No</tspan></text></g></g>"#;
        let fixed = normalize_edge_labels(svg);
        assert!(
            fixed.contains(r#"transform="translate(0,-11.5)""#),
            "{fixed}"
        );
        assert!(fixed.contains(r#"x="-12""#), "{fixed}");
        // Attribute order of the untouched attrs survives.
        assert!(
            fixed.contains(r#"height="23" y="-1" width="20""#),
            "{fixed}"
        );
    }

    #[test]
    fn unsized_node_label_rects_pass_through() {
        let svg = r#"<g class="label" style="" transform="translate(0,-44.7)"><rect/><g><rect class="background" style="stroke: none"/><text y="-10.1" style=""><tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em"><tspan>atom</tspan></tspan></text></g></g>"#;
        assert_eq!(normalize_edge_labels(svg), svg);
    }

    #[test]
    fn empty_edge_labels_without_rect_pass_through() {
        let svg = r#"<g class="edgeLabel"><g class="label" data-id="L_A_B_0" transform="translate(0,0)"><text y="-10.1" text-anchor="middle"><tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em" text-anchor="middle"/></text></g></g>"#;
        assert_eq!(normalize_edge_labels(svg), svg);
    }

    #[test]
    fn text_with_explicit_positioning_passes_through() {
        // Not a shape we emit normalization for: the text positions
        // itself, so any rewrite could only corrupt it.
        let svg = r#"<g class="label" transform="translate(-10,-11.5)"><g><rect class="background" x="-2" y="-1" width="20" height="23"/><text x="7" y="-10.1" text-anchor="middle"><tspan x="0">No</tspan></text></g></g>"#;
        assert_eq!(normalize_edge_labels(svg), svg);
    }

    #[test]
    fn mixed_document_normalizes_only_edge_labels() {
        let svg = format!(
            "{MERMAN_SHAPE}<g class=\"node default\" id=\"n1\" transform=\"translate(50.6,193.1)\"><rect class=\"basic label-container\" x=\"-42.6\" y=\"-24.5\" width=\"85.2\" height=\"49\"/><g class=\"label\" style=\"\" transform=\"translate(0,-9.5)\"><rect/><g><rect class=\"background\" style=\"stroke: none\"/><text y=\"-10.1\"><tspan class=\"row text-outer-tspan\" x=\"0\" y=\"-0.1em\" dy=\"1.1em\"><tspan>you</tspan></tspan></text></g></g></g>"
        );
        let fixed = normalize_edge_labels(&svg);
        // Edge label fixed...
        assert!(
            fixed.contains(r#"transform="translate(0,-29.1)""#),
            "{fixed}"
        );
        // ...node label untouched.
        assert!(
            fixed.contains(r#"<g class="label" style="" transform="translate(0,-9.5)">"#),
            "{fixed}"
        );
        assert!(
            fixed.contains(r#"<rect class="background" style="stroke: none"/>"#),
            "{fixed}"
        );
    }

    #[test]
    fn fix_is_idempotent() {
        let once = normalize_edge_labels(MERMAN_SHAPE);
        let twice = normalize_edge_labels(&once);
        assert_eq!(once, twice, "must be idempotent");
        let once = normalize_edge_labels(MERMAID_SHAPE);
        let twice = normalize_edge_labels(&once);
        assert_eq!(once, twice, "must be idempotent");
    }

    #[test]
    fn format_num_trims_noise() {
        assert_eq!(format_num(-103.94140625), "-103.9414");
        assert_eq!(format_num(-12.0), "-12");
        assert_eq!(format_num(-13.3945), "-13.3945");
    }
}
