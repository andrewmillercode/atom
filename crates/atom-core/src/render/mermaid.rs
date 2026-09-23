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
    if rounded == 0.0 {
        return "0".to_string();
    }
    let s = format!("{rounded}");
    s
}

/// Extra theme knobs for what mermaid's themeVariables can't express.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagramTheme {
    pub node_fill: String,
    pub label_border: String,
    pub label_text: String,
    /// Edge-label box fill (the app background, so boxes cut the edge).
    pub label_fill: String,
    /// Px to pull arrowhead-tipped path ends back off the target node.
    pub arrow_gap: f64,
}

/// Maps the active palette onto the diagram theme: nodes get the muted
/// card fill, edge-label boxes get the primary accent border on the app
/// background, text the theme foreground.
pub fn diagram_theme_from(theme: &crate::render::colors::Theme) -> DiagramTheme {
    DiagramTheme {
        node_fill: theme.muted_extra.clone(),
        label_border: theme.primary.clone(),
        label_text: theme.foreground.clone(),
        label_fill: theme.background.clone(),
        arrow_gap: 4.0,
    }
}

/// The diagram theme for the running process's active palette. The TUI
/// client keeps the registry live across theme reloads, so rasterizing
/// at paint time follows the selected theme.
pub fn active_diagram_theme() -> DiagramTheme {
    diagram_theme_from(&crate::render::colors::active_theme())
}

const LABEL_PADDING: f64 = 8.0;

/// Injects the edge-token color map into the raw SVG's root tag as
/// `data-atom-edge-colors='{"label":"#hex",…}'`, so the TUI rasterizer
/// can re-apply token colors when it re-themes the raw artifact at
/// paint time. Deterministic ordering keeps hashes stable.
pub fn inject_edge_colors(svg: &str, edge_colors: &[(String, String)]) -> String {
    if edge_colors.is_empty() {
        return svg.to_string();
    }
    let mut map = std::collections::BTreeMap::new();
    for (label, color) in edge_colors {
        map.insert(label.clone(), color.clone());
    }
    let Ok(json) = serde_json::to_string(&map) else {
        return svg.to_string();
    };
    let open_end = match svg.find('>') {
        Some(at) => at,
        None => return svg.to_string(),
    };
    if svg.contains("data-atom-edge-colors") {
        return svg.to_string();
    }
    format!(
        "{} data-atom-edge-colors='{}'{}",
        &svg[..open_end],
        json.replace('\'', "&#39;"),
        &svg[open_end..]
    )
}

/// Reads back a `data-atom-edge-colors` map injected by
/// [`inject_edge_colors`]. Empty when absent or malformed.
pub fn edge_colors_attribute(svg: &str) -> Vec<(String, String)> {
    let needle = "data-atom-edge-colors='";
    let Some(start) = svg.find(needle) else {
        return Vec::new();
    };
    let rest = &svg[start + needle.len()..];
    let Some(end) = rest.find('\'') else {
        return Vec::new();
    };
    let parsed: std::collections::BTreeMap<String, String> =
        serde_json::from_str(rest[..end].replace("&#39;", "'").as_str()).unwrap_or_default();
    parsed.into_iter().collect()
}

/// Applies the theme, idempotently. `edge_colors` maps label text to a
/// border color; unmatched labels get the default.
pub fn apply_diagram_theme(
    svg: &str,
    theme: &DiagramTheme,
    edge_colors: &[(String, String)],
) -> String {
    if svg.contains("id=\"atom-diagram-theme\"") {
        return svg.to_string();
    }
    let mut out = restyle_edge_labels(svg, theme, edge_colors);
    out = foreign_objects_to_text(&out, theme);
    out = square_label_container_paths(&out);
    out = gap_edge_ends(&out, theme.arrow_gap);
    match out.rfind("</svg>") {
        Some(at) => out.insert_str(at, &override_css(theme)),
        None => out.push_str(&override_css(theme)),
    }
    out
}

/// Restyles every `<g class="edgeLabel">` group: padded, border-only
/// rect colored via the token map; foreignObject labels become text.
fn restyle_edge_labels(
    svg: &str,
    theme: &DiagramTheme,
    edge_colors: &[(String, String)],
) -> String {
    let mut out = String::with_capacity(svg.len() + 512);
    let mut rest = svg;
    while let Some(at) = rest.find(r#"<g class="edgeLabel""#) {
        out.push_str(&rest[..at]);
        let Some(end) = find_group_end(&rest[at..]) else {
            out.push_str(rest);
            return out;
        };
        let group = &rest[at..at + end];
        out.push_str(&restyle_edge_label_group(group, theme, edge_colors));
        rest = &rest[at + end..];
    }
    out.push_str(rest);
    out
}

fn find_group_end(s: &str) -> Option<usize> {
    let open_end = s.find('>')? + 1;
    let mut depth = 1usize;
    let mut i = open_end;
    while i < s.len() {
        let next_lt = s[i..].find('<')? + i;
        if s[next_lt..].starts_with("<g") && s[next_lt + 2..].starts_with([' ', '>']) {
            depth += 1;
            i = next_lt + 2;
        } else if s[next_lt..].starts_with("</g>") {
            depth -= 1;
            i = next_lt + 4;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i = next_lt + 1;
        }
    }
    None
}

fn restyle_edge_label_group(
    group: &str,
    theme: &DiagramTheme,
    edge_colors: &[(String, String)],
) -> String {
    let label_text = label_text_of(group);
    let stroke = edge_colors
        .iter()
        .find(|(name, _)| {
            let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
            let text: String = label_text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            name.to_lowercase() == text
        })
        .map(|(_, color)| color.clone())
        .unwrap_or_else(|| theme.label_border.to_string());

    // Existing rect shape (merman flowchart): rebuild as a centered,
    // background-filled box with optically centered text.
    if let Some(caps) = edge_rect_re()
        .captures(group)
        .filter(|_| !group.contains("<foreignObject"))
    {
        let rect = &caps[0];
        let w = attr_num(rect, "width").unwrap_or(0.0);
        let h = attr_num(rect, "height").unwrap_or(0.0);
        if w <= 0.0 || h <= 0.0 {
            return group.to_string();
        }
        let rows = label_rows(group);
        if rows.is_empty() {
            return group.to_string();
        }
        let outer_end = match group.find('>') {
            Some(at) => at + 1,
            None => return group.to_string(),
        };
        // Without an outer translate there is no edge midpoint to
        // center on; leave the geometry alone rather than guess.
        if extract_translate(&group[..outer_end]).is_none() {
            return group.to_string();
        }
        let (nw, nh) = (w + 2.0 * LABEL_PADDING, h + 2.0 * LABEL_PADDING);
        let font_size = ((h / 1.5).round().clamp(10.0, 16.0)) as i32;
        let line_height = font_size as f64 * 1.5;
        let mut tspans = String::new();
        for (i, line) in rows.iter().enumerate() {
            let dy = if i == 0 {
                -((rows.len() - 1) as f64) * line_height / 2.0
            } else {
                line_height
            };
            tspans.push_str(&format!(
                r#"<tspan x="0" dy="{}">{}</tspan>"#,
                format_num(dy),
                xml_escape(line)
            ));
        }
        let new_rect = format!(
            r#"<rect class="background" x="{}" y="{}" width="{}" height="{}" rx="4" style="fill:{};stroke:{};opacity:1"/>"#,
            format_num(-nw / 2.0),
            format_num(-nh / 2.0),
            format_num(nw),
            format_num(nh),
            theme.label_fill,
            stroke,
        );
        let text = format!(
            r#"<text text-anchor="middle" dominant-baseline="central" font-size="{font_size}" fill="{}">{tspans}</text>"#,
            theme.label_text
        );
        let mut out = String::with_capacity(group.len() + 32);
        out.push_str(&group[..outer_end]);
        out.push_str(r#"<g class="label""#);
        if let Some(id) = group_data_id(group) {
            out.push_str(&format!(r#" data-id="{id}""#));
        }
        out.push_str(r#" transform="translate(0,0)">"#);
        out.push_str(&new_rect);
        out.push_str(&text);
        out.push_str("</g></g>");
        return out;
    }

    // foreignObject shape (state, ER, …): convert to text + rect.
    if let Some(caps) = fo_re().captures(group) {
        let fo = &caps[0];
        let (x0, y0, w, h) = fo_box(&caps[2]);
        let lines = fo_text_lines(fo);
        if w <= 0.0 || h <= 0.0 || lines.is_empty() {
            return group.to_string();
        }
        let (nw, nh) = (w + 2.0 * LABEL_PADDING, h + 2.0 * LABEL_PADDING);
        let font_size = ((h / 1.5).round().clamp(10.0, 16.0)) as i32;
        let line_height = font_size as f64 * 1.5;
        let mut tspans = String::new();
        for (i, line) in lines.iter().enumerate() {
            let dy = if i == 0 {
                -((lines.len() - 1) as f64) * line_height / 2.0
            } else {
                line_height
            };
            tspans.push_str(&format!(
                r#"<tspan x="0" dy="{}">{}</tspan>"#,
                format_num(dy),
                line
            ));
        }
        let text = format!(
            r#"<text x="0" y="0" font-size="{font_size}" text-anchor="middle" dominant-baseline="central" fill="{}">{tspans}</text>"#,
            theme.label_text
        );
        let rect = format!(
            r#"<rect class="background" x="{}" y="{}" width="{}" height="{}" rx="4" style="fill:{};stroke:{};opacity:1"/>"#,
            format_num(-(nw / 2.0)),
            format_num(-(nh / 2.0)),
            format_num(nw),
            format_num(nh),
            theme.label_fill,
            stroke,
        );
        // The FO box center (x0+w/2, y0+h/2 local to the g) becomes
        // the text/rect origin.
        let label_open = &caps[1];
        let recentered_open =
            match recentered_group_transform(label_open, x0 + w / 2.0, y0 + h / 2.0) {
                Some(new_t) => match extract_translate(label_open) {
                    Some(old_t) => label_open.replace(&old_t, &new_t),
                    None => label_open.to_string(),
                },
                None => label_open.to_string(),
            };
        group.replace(fo, &format!("{recentered_open}{rect}{text}"))
    } else {
        group.to_string()
    }
}

fn edge_rect_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"<rect class="background"[^>]*>"#).unwrap())
}

fn fo_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?s)(<g class="label"[^>]*>)\s*(<foreignObject[^>]*>)(.*?)</foreignObject>"#,
        )
        .unwrap()
    })
}

fn extract_translate(tag: &str) -> Option<String> {
    let start = tag.find("translate(")?;
    let rest = &tag[start..];
    let end = rest.find(')')? + 1;
    Some(format!("transform=\"{}\"", &rest[..end]))
}

/// Builds the translate that moves the box center (cx,cy) onto the origin.
fn recentered_group_transform(tag: &str, cx: f64, cy: f64) -> Option<String> {
    let t = extract_translate(tag)?;
    let inner = t.strip_prefix("transform=\"translate(")?;
    let comma = inner.find(',')?;
    let close = inner.find(')')?;
    let tx: f64 = inner[..comma].trim().parse().ok()?;
    let ty: f64 = inner[comma + 1..close].trim().parse().ok()?;
    Some(format!(
        "transform=\"translate({},{})\"",
        format_num(tx + cx),
        format_num(ty + cy)
    ))
}

/// Visible text of a label group, for token lookup.
fn label_text_of(group: &str) -> String {
    strip_tags(group)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// data-id attribute of a label group, if any.
fn group_data_id(group: &str) -> Option<String> {
    let needle = r#"data-id=""#;
    let start = group.find(needle)? + needle.len();
    let rest = &group[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Text rows of a rect-shape label group: one entry per row tspan,
/// tag-stripped. Empty for shapes we don't rebuild.
fn label_rows(group: &str) -> Vec<String> {
    let needle = r#"<tspan class="row"#;
    group
        .split(needle)
        .skip(1)
        .filter_map(|chunk| chunk.find('>').map(|at| &chunk[at + 1..]))
        .map(strip_tags)
        .map(|row| row.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|row| !row.is_empty())
        .collect()
}

/// xml_escape re-escapes text that strip_tags has unescaped.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Pulls the end of every edge path that carries an End-arrow marker
/// back by `gap` px, so the arrowhead stops short of the target node.
fn gap_edge_ends(svg: &str, gap: f64) -> String {
    if gap <= 0.0 {
        return svg.to_string();
    }
    edge_path_re()
        .replace_all(svg, |caps: &regex::Captures| {
            if !has_end_marker(&caps[3]) {
                return caps[0].to_string();
            }
            let Some(new_d) = shorten_path(&caps[2], gap) else {
                return caps[0].to_string();
            };
            format!("{}{}{}", &caps[1], new_d, &caps[3])
        })
        .into_owned()
}

fn edge_path_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"(<path[^>]*?\sd=")([^"]+)("[^>]*>)"#).unwrap())
}

/// True when the path tag's marker-end references an End-arrow marker.
fn has_end_marker(tag: &str) -> bool {
    let needle = r#"marker-end="url(#"#;
    let Some(start) = tag.find(needle) else {
        return false;
    };
    let rest = &tag[start + needle.len()..];
    match rest.find(')') {
        Some(end) => rest[..end].contains("End"),
        None => false,
    }
}

/// Pulls the end of an absolute M/L/C/H/V path back by `gap` along the
/// end tangent, walking backwards over trailing segments: a segment is
/// shortened along its tangent while it is longer than the remainder,
/// otherwise dropped entirely (merman ends every edge with a ~3.5px
/// stub, and moving its endpoint past its start would reverse the
/// tangent and flip the arrowhead). None for relative commands, arcs,
/// multi-subpath paths, or paths shorter than `gap`.
fn shorten_path(d: &str, gap: f64) -> Option<String> {
    use std::sync::OnceLock;
    static TOKEN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let token = TOKEN_RE.get_or_init(|| {
        regex::Regex::new(r#"([A-Za-z])|(-?(?:\d+\.?\d*|\.\d+)(?:[eE]-?\d+)?)"#).unwrap()
    });

    // (command, value, byte range of the number in d)
    let mut nums: Vec<(char, f64, usize, usize)> = Vec::new();
    let mut cmd = '\0';
    let mut letter_span = (0usize, 0usize);
    for cap in token.captures_iter(d) {
        if let Some(letter) = cap.get(1) {
            cmd = letter.as_str().chars().next()?;
            letter_span = (letter.start(), letter.end());
            // Anything else (relative, arcs): tail not trustworthy.
            match cmd {
                'M' | 'L' | 'C' | 'H' | 'V' => {}
                _ => return None,
            }
        } else {
            let m = cap.get(0)?;
            nums.push((cmd, cap[0].parse().ok()?, m.start(), m.end()));
        }
    }

    // One entry per drawn segment: tangent reference point, endpoint,
    // byte range of the whole segment, byte range of its final point.
    struct Seg {
        reference: (f64, f64),
        end: (f64, f64),
        seg_span: (usize, usize),
        point_span: (usize, usize, bool),
    }
    let mut segs: Vec<Seg> = Vec::new();
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let mut started = false;
    let mut i = 0usize;
    while i < nums.len() {
        match nums[i].0 {
            'M' => {
                if started {
                    return None;
                }
                let (x, y) = (nums.get(i)?, nums.get(i + 1)?);
                (cx, cy) = (x.1, y.1);
                started = true;
                i += 2;
            }
            'L' => {
                let (x, y) = (nums.get(i)?, nums.get(i + 1)?);
                segs.push(Seg {
                    reference: (cx, cy),
                    end: (x.1, y.1),
                    seg_span: (letter_span.0, y.3),
                    point_span: (x.2, y.3, true),
                });
                (cx, cy) = (x.1, y.1);
                i += 2;
            }
            'C' => {
                let (c2x, c2y) = (nums.get(i + 2)?, nums.get(i + 3)?);
                let (x, y) = (nums.get(i + 4)?, nums.get(i + 5)?);
                segs.push(Seg {
                    reference: (c2x.1, c2y.1),
                    end: (x.1, y.1),
                    seg_span: (letter_span.0, y.3),
                    point_span: (x.2, y.3, true),
                });
                (cx, cy) = (x.1, y.1);
                i += 6;
            }
            'H' => {
                let x = nums.get(i)?;
                segs.push(Seg {
                    reference: (cx, cy),
                    end: (x.1, cy),
                    seg_span: (letter_span.0, x.3),
                    point_span: (x.2, x.3, false),
                });
                cx = x.1;
                i += 1;
            }
            'V' => {
                let y = nums.get(i)?;
                segs.push(Seg {
                    reference: (cx, cy),
                    end: (cx, y.1),
                    seg_span: (letter_span.0, y.3),
                    point_span: (y.2, y.3, false),
                });
                cy = y.1;
                i += 1;
            }
            _ => return None,
        }
    }

    // Walk backwards: shorten the last segment that still has room,
    // drop the ones shorter than the remainder.
    let mut cut: Option<(usize, usize)> = None;
    let mut moved: Option<((usize, usize, bool), String)> = None;
    let mut remaining = gap;
    for seg in segs.iter().rev() {
        let (dx, dy) = (seg.end.0 - seg.reference.0, seg.end.1 - seg.reference.1);
        let chord = (dx * dx + dy * dy).sqrt();
        if chord <= 0.0 {
            return None;
        }
        if remaining < chord - 1e-6 {
            let (nx, ny) = (
                seg.end.0 - remaining * dx / chord,
                seg.end.1 - remaining * dy / chord,
            );
            let tail = if seg.point_span.2 {
                format!("{},{}", format_num(nx), format_num(ny))
            } else {
                format_num(nx)
            };
            moved = Some((seg.point_span, tail));
            break;
        }
        remaining -= chord;
        cut = Some(seg.seg_span);
    }
    let (span, tail) = moved?;
    match cut {
        // Deleted tail segments sit after the shortened one.
        Some((cut_start, cut_end)) if cut_start >= span.1 => Some(format!(
            "{}{}{}{}",
            &d[..span.0],
            tail,
            &d[span.1..cut_start],
            &d[cut_end..]
        )),
        Some(_) => None,
        None => Some(format!("{}{}{}", &d[..span.0], tail, &d[span.1..])),
    }
}

fn fo_box(fo: &str) -> (f64, f64, f64, f64) {
    let Some((open, _)) = fo.split_once('>') else {
        return (0.0, 0.0, 0.0, 0.0);
    };
    (
        attr_num(open, "x").unwrap_or(0.0),
        attr_num(open, "y").unwrap_or(0.0),
        attr_num(open, "width").unwrap_or(0.0),
        attr_num(open, "height").unwrap_or(0.0),
    )
}

/// Visible text lines of a foreignObject body; `<br/>` splits.
fn fo_text_lines(fo: &str) -> Vec<String> {
    let body = match fo.split_once('>') {
        Some((_, tail)) => match tail.split_once("</foreignObject>") {
            Some((body, _)) => body,
            None => return Vec::new(),
        },
        None => return Vec::new(),
    };
    body.replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .split('\n')
        .map(|line| strip_tags(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Strips `<...>` markup and unescapes the entities Mermaid emits.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Node-label foreignObjects (state/ER) become centered `<text>`;
/// resvg cannot paint foreignObject.
fn foreign_objects_to_text(svg: &str, theme: &DiagramTheme) -> String {
    fo_any_re()
        .replace_all(svg, |caps: &regex::Captures| {
            let fo = &caps[0];
            let (x0, y0, w, h) = fo_box(fo);
            let lines = fo_text_lines(fo);
            if w <= 0.0 || h <= 0.0 || lines.is_empty() {
                return fo.to_string();
            }
            let font_size = ((h / 1.5).round().clamp(10.0, 16.0)) as i32;
            let line_height = font_size as f64 * 1.5;
            let cx = x0 + w / 2.0;
            let cy = y0 + h / 2.0;
            let mut tspans = String::new();
            for (i, line) in lines.iter().enumerate() {
                let dy = if i == 0 {
                    -((lines.len() - 1) as f64) * line_height / 2.0
                } else {
                    line_height
                };
                tspans.push_str(&format!(
                    r#"<tspan x="{}" dy="{}">{}</tspan>"#,
                    format_num(cx),
                    format_num(dy),
                    line
                ));
            }
            format!(
                r#"<text x="{}" y="{}" font-size="{}" text-anchor="middle" dominant-baseline="central" fill="{}">{}</text>"#,
                format_num(cx),
                format_num(cy),
                font_size,
                theme.label_text,
                tspans
            )
        })
        .into_owned()
}

fn fo_any_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"(?s)<foreignObject[^>]*>.*?</foreignObject>"#).unwrap())
}

/// State-diagram nodes are rounded paths (label-container class on the
/// parent g); rewrite them as square bounds. Non-parsing paths pass.
fn square_label_container_paths(svg: &str) -> String {
    container_path_re()
        .replace_all(svg, |caps: &regex::Captures| {
            let Some(square) = square_path_d(&caps[2]) else {
                return caps[0].to_string();
            };
            format!("{}{}\"", &caps[1], square)
        })
        .into_owned()
}

fn container_path_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?s)(<g class="basic label-container[^>]*>\s*<path[^>]*?d=")([^"]*)""#)
            .unwrap()
    })
}

/// Absolute M/C/L/H/V path -> square rect path over its bounds.
/// None for relative commands or arcs (bounds not trustworthy).
fn square_path_d(d: &str) -> Option<String> {
    use std::sync::OnceLock;
    static TOKEN_RE: OnceLock<regex::Regex> = OnceLock::new();
    let token = TOKEN_RE.get_or_init(|| {
        regex::Regex::new(r#"([A-Za-z])|(-?(?:\d+\.?\d*|\.\d+)(?:[eE]-?\d+)?)"#).unwrap()
    });

    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut cmd = '\0';
    let mut pair_slot = 0usize;
    for cap in token.captures_iter(d) {
        if let Some(letter) = cap.get(1) {
            cmd = letter.as_str().chars().next()?;
            // Anything else (relative, arcs): bounds not trustworthy.
            match cmd {
                'M' | 'L' | 'C' | 'H' | 'V' | 'Z' => pair_slot = 0,
                _ => return None,
            }
        } else {
            let n: f64 = cap[0].parse().ok()?;
            match cmd {
                'M' | 'L' => {
                    pair_slot += 1;
                    if pair_slot % 2 == 1 {
                        min_x = min_x.min(n);
                        max_x = max_x.max(n);
                    } else {
                        min_y = min_y.min(n);
                        max_y = max_y.max(n);
                    }
                }
                'C' => {
                    pair_slot += 1;
                    if pair_slot % 2 == 1 {
                        min_x = min_x.min(n);
                        max_x = max_x.max(n);
                    } else {
                        min_y = min_y.min(n);
                        max_y = max_y.max(n);
                    }
                }
                'H' => {
                    min_x = min_x.min(n);
                    max_x = max_x.max(n);
                }
                'V' => {
                    min_y = min_y.min(n);
                    max_y = max_y.max(n);
                }
                _ => {}
            }
        }
    }
    if min_x > max_x || min_y > max_y {
        return None;
    }
    Some(format!(
        "M{} {}L{} {}L{} {}L{} {}Z",
        format_num(min_x),
        format_num(min_y),
        format_num(max_x),
        format_num(min_y),
        format_num(max_x),
        format_num(max_y),
        format_num(min_x),
        format_num(max_y)
    ))
}

/// Appended after the renderer's styles so ours win ties; inline
/// styles (classDefs, token colors) still win over this.
fn override_css(theme: &DiagramTheme) -> String {
    format!(
        r#"<style id="atom-diagram-theme">#merman .node rect,#merman .node path,#merman .node polygon,#merman .node circle,#merman .node ellipse,#merman g.stateGroup rect,#merman g.classGroup rect,#merman .entityBox,#merman .actor{{fill:{};stroke:none;}}#merman .node rect,#merman .actor{{rx:0;ry:0;}}#merman g.stateGroup text,#merman g.classGroup text,#merman text.actor>tspan,#merman .messageText,#merman .node text{{fill:{};}}#merman .edgeLabel rect{{fill:{};stroke:{};opacity:1;}}#merman .edgeLabel{{background-color:transparent;}}#merman .edgeLabel .label text{{fill:{};}}</style>"#,
        theme.node_fill, theme.label_text, theme.label_fill, theme.label_border, theme.label_text
    )
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

    fn theme() -> DiagramTheme {
        DiagramTheme {
            node_fill: "#3d3d3d".into(),
            label_border: "#8cadd1".into(),
            label_text: "#ffffff".into(),
            label_fill: "#111112".into(),
            arrow_gap: 4.0,
        }
    }

    fn THEME() -> DiagramTheme {
        theme()
    }

    /// merman state-diagram edge label: foreignObject instead of
    /// rect+text (state/ER ignore htmlLabels:false).
    const FO_LABEL: &str = r#"<g class="edgeLabel" transform="translate(95.4,251.5)"><g class="label" data-id="edge2" transform="translate(-22.6367,-11.5)"><foreignObject width="45.2734" height="23"><div xmlns="http://www.w3.org/1999/xhtml" class="labelBkg" style="line-height: 1.5;"><span class="edgeLabel"><p>pause</p></span></div></foreignObject></g></g>"#;

    #[test]
    fn edge_label_fo_becomes_bordered_box() {
        let out = apply_diagram_theme(FO_LABEL, &THEME(), &[]);
        // Label g re-centered, rect and text anchored at the origin.
        assert!(out.contains(r#"transform="translate(0,0)""#), "{out}");
        assert!(out.contains(r#"fill:#111112;stroke:#8cadd1"#), "{out}");
        // 45.27 + 2*8 padding, centered on the origin.
        assert!(out.contains(r#"x="-30.6367""#), "{out}");
        assert!(out.contains(r#"width="61.2734""#), "{out}");
        assert!(out.contains("font-size=\"15\""), "{out}");
        assert!(out.contains(">pause</tspan>"), "{out}");
        assert!(!out.contains("foreignObject"), "{out}");
        // The label g open tag survives (balanced XML).
        assert_eq!(out.matches("<g").count(), out.matches("</g>").count());
    }

    #[test]
    fn edge_label_token_color_wins() {
        let colors = vec![("Pause".to_string(), "#e8a07a".to_string())];
        let out = apply_diagram_theme(FO_LABEL, &THEME(), &colors);
        // Inline token style beats the stylesheet's default blue border.
        assert!(
            out.contains(r#"style="fill:#111112;stroke:#e8a07a"#),
            "{out}"
        );
    }

    #[test]
    fn state_node_path_is_squared() {
        let svg = r##"<g class="node statediagram-state" transform="translate(5,5)"><g class="basic label-container outer-path"><path d="M-21.41 -17.5 C-5 -17.6, 5 -17.4, 21.41 -17.5 C21.5 -5, 21.3 5, 21.41 17.5 C5 17.6, -5 17.4, -21.41 17.5 C-21.5 5, -21.3 -5, -21.41 -17.5Z" fill="#1f2020"/></g></g>"##;
        let out = apply_diagram_theme(svg, &THEME(), &[]);
        // Bounds include the curve control points of the input path.
        assert!(
            out.contains(r#"d="M-21.5 -17.6L21.5 -17.6L21.5 17.6L-21.5 17.6Z""#),
            "{out}"
        );
        // Fill is overridden via CSS (attribute loses to stylesheet).
        assert!(
            out.contains(r#"#merman .node path,#merman .node polygon"#),
            "{out}"
        );
    }

    #[test]
    fn theme_pass_is_idempotent() {
        let once = apply_diagram_theme(FO_LABEL, &THEME(), &[]);
        let twice = apply_diagram_theme(&once, &THEME(), &[]);
        assert_eq!(once, twice);
    }

    #[test]
    fn flowchart_edge_label_rect_gets_padding() {
        let svg = r#"<g class="edgeLabel" transform="translate(10,20)"><g class="label" data-id="L_A_B_0" transform="translate(0,-11.5)"><g><rect class="background" x="-13.3945" y="-11.5" width="22.789" height="23"/><text y="-10.1" text-anchor="middle"><tspan class="row" x="0" dy="1.1em"><tspan>No</tspan></tspan></text></g></g></g>"#;
        let out = apply_diagram_theme(svg, &THEME(), &[]);
        // Box rebuilt centered on the edge midpoint, background-filled.
        assert!(out.contains(r#"transform="translate(0,0)""#), "{out}");
        assert!(
            out.contains(r#"x="-19.3945" y="-19.5" width="38.789" height="39""#),
            "{out}"
        );
        assert!(out.contains(r#"fill:#111112;stroke:#8cadd1"#), "{out}");
        // Text centered with the central baseline.
        assert!(
            out.contains(r#"text-anchor="middle" dominant-baseline="central""#),
            "{out}"
        );
        assert!(out.contains("font-size=\"15\""), "{out}");
        assert!(out.contains(">No</tspan>"), "{out}");
        assert!(out.contains(r#"data-id="L_A_B_0""#), "{out}");
        // Balanced XML.
        assert_eq!(out.matches("<g").count(), out.matches("</g>").count());
    }

    #[test]
    fn edge_label_rows_keep_entities_escaped() {
        let svg = r#"<g class="edgeLabel" transform="translate(10,20)"><g class="label" transform="translate(0,-11.5)"><g><rect class="background" x="-13.3945" y="-11.5" width="22.789" height="23"/><text y="-10.1" text-anchor="middle"><tspan class="row" x="0" dy="1.1em"><tspan>a &amp; b</tspan></tspan></text></g></g></g>"#;
        let out = apply_diagram_theme(svg, &THEME(), &[]);
        assert!(out.contains(">a &amp; b</tspan>"), "{out}");
    }

    #[test]
    fn arrow_gap_pulls_path_end_off_target_node() {
        // Real merman edge: curve then a 3.5px straight stub to the node.
        let path = r#"<path d="M103.016,82L107.182,82C111.349,82,119.682,82,127.349,82C135.016,82,142.016,82,145.516,82L149.016,82" class="flowchart-link" marker-end="url(#merman_flowchart-v2-pointEnd)" />"#;
        let out = apply_diagram_theme(path, &THEME(), &[]);
        // Stub dropped; curve endpoint pulled back the remaining 0.5px.
        assert!(
            out.contains(r#"C135.016,82,142.016,82,145.016,82"#),
            "{out}"
        );
        assert!(!out.contains("L149"), "{out}");
    }

    #[test]
    fn arrow_gap_long_stub_is_only_shortened() {
        let path = r#"<path d="M0,0L10,0L30,0" class="flowchart-link" marker-end="url(#merman_flowchart-v2-pointEnd)" />"#;
        let out = apply_diagram_theme(path, &THEME(), &[]);
        // Final L is 20px long: endpoint pulled back 4px, no deletion.
        assert!(out.contains(r#"L10,0L26,0"#), "{out}");
    }

    #[test]
    fn arrow_gap_reversal_is_avoided_on_short_stub() {
        // Regression: pulling the 3.5px stub's endpoint back 4px used to
        // reverse the tangent, flipping the arrowhead 180°.
        let path = r#"<path d="M0,0C1,0 2,0 3,0L6.5,0" class="flowchart-link" marker-end="url(#merman_flowchart-v2-pointEnd)" />"#;
        let out = apply_diagram_theme(path, &THEME(), &[]);
        assert!(out.contains(r#"C1,0 2,0 2.5,0"#), "{out}");
        assert!(!out.contains("L6.5"), "{out}");
    }

    #[test]
    fn shorten_path_direct() {
        let d = "M0,0C1,0 2,0 3,0L6.5,0";
        let out = shorten_path(d, 4.0);
        assert_eq!(out.as_deref(), Some("M0,0C1,0 2,0 2.5,0"), "{out:?}");
        let d = "M103.016,82L107.182,82C135.016,82,142.016,82,145.516,82L149.016,82";
        let out = shorten_path(d, 4.0);
        assert_eq!(
            out.as_deref(),
            Some("M103.016,82L107.182,82C135.016,82,142.016,82,145.016,82"),
            "{out:?}"
        );
    }

    #[test]
    fn arrow_gap_skips_multi_subpath_paths() {
        let path = r#"<path d="M0,0L10,0M20,0L30,0" class="flowchart-link" marker-end="url(#merman_flowchart-v2-pointEnd)" />"#;
        assert!(
            apply_diagram_theme(path, &THEME(), &[]).starts_with(path),
            "path must be untouched"
        );
    }

    #[test]
    fn arrow_gap_skips_paths_without_end_marker() {
        let path = r#"<path d="M0,0L10,0" class="flowchart-link" />"#;
        assert!(
            apply_diagram_theme(path, &THEME(), &[]).starts_with(path),
            "path must be untouched"
        );
    }

    #[test]
    fn arrow_gap_follows_curve_tangent() {
        let path = r#"<path d="M0,0C10,0 20,4 30,10" class="flowchart-link" marker-end="url(#merman_flowchart-v2-pointEnd)" />"#;
        let out = apply_diagram_theme(path, &THEME(), &[]);
        // Direction end−c2 = (10,6), unit ≈ (0.8575, 0.5145); end moves
        // back 4px to ≈ (26.57, 7.942).
        assert!(out.contains(r#"20,4 26.57,7.942"#), "{out}");
    }

    #[test]
    fn square_path_rejects_relative_commands() {
        assert!(square_path_d("M10 10l5 5L20 20Z").is_none());
        assert!(square_path_d("M10 10A5 5 0 0 1 20 20").is_none());
        assert_eq!(
            square_path_d("M 19,7 L9,13 L14,7 L9,1 Z").map(|_| ()),
            Some(())
        );
    }

    #[test]
    fn edge_colors_roundtrip_through_the_root_attribute() {
        let colors = vec![
            ("allow".to_string(), "#96d1ae".to_string()),
            ("deny\"x".to_string(), "#8cadd1".to_string()),
        ];
        let raw = inject_edge_colors(r#"<svg xmlns="…"><g/></svg>"#, &colors);
        assert!(
            raw.contains(r#"<svg xmlns="…" data-atom-edge-colors='"#),
            "{raw}"
        );
        assert_eq!(edge_colors_attribute(&raw), colors);
        // No tokens: nothing injected, nothing parsed.
        assert_eq!(
            inject_edge_colors("<svg><g/></svg>", &[]),
            "<svg><g/></svg>"
        );
        assert!(edge_colors_attribute("<svg><g/></svg>").is_empty());
        // Injection is idempotent.
        assert_eq!(inject_edge_colors(&raw, &colors), raw);
    }

    #[test]
    fn diagram_theme_follows_the_palette() {
        let mut t = crate::render::colors::Theme::default();
        t.muted_extra = "#010203".into();
        t.primary = "#040506".into();
        t.foreground = "#070809".into();
        t.background = "#0a0b0c".into();
        let dt = diagram_theme_from(&t);
        assert_eq!(dt.node_fill, "#010203");
        assert_eq!(dt.label_border, "#040506");
        assert_eq!(dt.label_text, "#070809");
        assert_eq!(dt.label_fill, "#0a0b0c");
        assert_eq!(dt.arrow_gap, 4.0);
    }
}
