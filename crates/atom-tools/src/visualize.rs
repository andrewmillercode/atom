//! visualize tool: renders Mermaid diagrams to SVG for inline display
//! in the TUI (rasterized on-demand via resvg at the exact terminal
//! dimensions) plus a self-contained, pan/zoom HTML viewer for the
//! browser.
//!
//! Rendering shells out to a browserless Mermaid CLI — `merman-cli`
//! (native Rust, preferred) or the official `mmdc` (Node) — producing
//! one raw SVG plus its themed form (`atom_core::render::mermaid`),
//! both derived from the palette selected in config.json. The TUI
//! rasterizes the raw SVG at paint time, re-applying the theme from the
//! live palette so the preview follows theme changes; the browser
//! viewer shows the themed SVG baked at render time.
//!
//! Artifacts are content-addressed under the app data dir
//! (`<data>/atom/diagrams/<slug>-<hash8>-raw.svg` and
//! `<slug>-<hash8>.{svg,html}`): identical Mermaid sources under the
//! same theme reuse the same files across calls and sessions.
//!
//! The tool result embeds a single machine-readable marker line
//! (`[atom-diagram] svg="…" html="…" width=… height=…`)
//! that the TUI parses to paint the inline image; it is stripped from
//! the rendered summary. Paths are quoted because the artifacts dir
//! contains a space on macOS.

use crate::{ToolCtx, ToolOutcome};
use atom_core::render::colors::Theme;
use std::path::PathBuf;

// All diagram styling; edit here.
mod style {
    use atom_core::render::colors::{theme_snapshot, Theme};

    pub const DOT_OPACITY: f64 = 0.5;

    /// The palette selected in config.json, resolved for this process.
    /// The visualize tool runs in the atoms server, where the TUI's live
    /// theme registry is not updated; config carries the selected id.
    pub fn theme() -> Theme {
        let id = atom_core::config::load().theme;
        id.as_deref().and_then(theme_snapshot).unwrap_or_default()
    }

    pub fn dot(theme: &Theme) -> String {
        with_alpha(&theme.muted_extra, DOT_OPACITY)
    }

    /// Edge-label color tokens; palette accents (green has no theme role).
    pub fn label_token_color(theme: &Theme, token: &str) -> Option<String> {
        match token {
            "blue" => Some(theme.primary.clone()),
            "green" => Some("#96d1ae".to_string()),
            "orange" => Some(theme.syntax_type.clone()),
            "pink" => Some(theme.secondary.clone()),
            _ => None,
        }
    }

    /// Renderer themeVariables: everything mermaid bakes into its SVG.
    pub fn mermaid_theme_variables(theme: &Theme) -> String {
        format!(
            r#"{{"clusterBkg":"transparent","clusterBorder":"{border}","mainBkg":"{fill}","nodeBorder":"{fill}","nodeTextColor":"{fg}","primaryTextColor":"{fg}","edgeLabelBackground":"transparent"}}"#,
            border = theme.border,
            fill = theme.muted_extra,
            fg = theme.foreground,
        )
    }

    /// The diagram-relevant palette slice, hashed into the artifact key
    /// so a theme switch produces fresh artifacts.
    pub fn theme_fingerprint(theme: &Theme) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            theme.background,
            theme.foreground,
            theme.muted_extra,
            theme.primary,
            theme.border,
            theme.card_dark
        )
    }

    pub fn css_vars(theme: &Theme) -> String {
        format!(
            ":root {{ --bg: {BG}; --card: {CARD}; --border: {BORDER}; \
             --fg: {FG}; --muted: {MUTED}; --dot: {DOT}; }}",
            BG = theme.background,
            CARD = theme.card_dark,
            BORDER = theme.border,
            FG = theme.foreground,
            MUTED = theme.muted,
            DOT = dot(theme)
        )
    }

    fn with_alpha(hex: &str, alpha: f64) -> String {
        let h = hex.trim_start_matches('#');
        if h.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&h[0..2], 16),
                u8::from_str_radix(&h[2..4], 16),
                u8::from_str_radix(&h[4..6], 16),
            ) {
                return format!("rgba({r},{g},{b},{alpha})");
            }
        }
        hex.to_string()
    }
}

/// Which renderer binary is available ("merman-cli" or "mmdc").
fn renderer() -> Option<PathBuf> {
    atom_core::deps::find_tool("merman-cli").or_else(|| atom_core::deps::find_tool("mmdc"))
}

/// Directory holding rendered diagram artifacts.
pub fn diagram_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(atom_core::build::dir_leaf())
        .join("diagrams")
}

/// Filesystem-safe slug from the (optional) title, falling back to
/// "diagram". Empty after sanitizing falls back too.
fn slugify(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "diagram".to_string()
    } else {
        slug.chars().take(40).collect()
    }
}

/// CLI config: native SVG text (resvg can't render foreignObject) plus
/// the shared themeVariables. Diagram-level `style ... fill:` still wins.
fn mermaid_config(theme: &atom_core::render::colors::Theme) -> String {
    format!(
        r#"{{"flowchart":{{"htmlLabels":false}},"sequence":{{"htmlLabels":false}},"htmlLabels":false,"themeVariables":{}}}"#,
        style::mermaid_theme_variables(theme)
    )
}

/// Renders Mermaid `code` to SVG via the external CLI.
async fn render_mermaid(code: &str, bin: &std::path::Path) -> Result<Vec<u8>, String> {
    let theme = style::theme();
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let src = dir.path().join("diagram.mmd");
    std::fs::write(&src, code).map_err(|e| format!("write source: {e}"))?;
    let svg_path = dir.path().join("diagram.svg");

    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, mermaid_config(&theme)).map_err(|e| format!("write config: {e}"))?;

    let args: Vec<String> = vec![
        "-i".to_string(),
        src.display().to_string(),
        "-o".to_string(),
        svg_path.display().to_string(),
        "-t".to_string(),
        "dark".to_string(),
        "-b".to_string(),
        "transparent".to_string(),
        "-c".to_string(),
        cfg_path.display().to_string(),
    ];
    renderer_run(bin, &args).await?;

    let svg = std::fs::read(&svg_path).map_err(|e| format!("read svg: {e}"))?;
    if svg.is_empty() {
        return Err("renderer produced empty output".to_string());
    }
    Ok(svg)
}

async fn renderer_run(bin: &std::path::Path, args: &[String]) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| "render timed out after 120s".to_string())?
        .map_err(|e| format!("render failed: {e}"))?;
    if !out.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(format!(
            "renderer exited {}: {}",
            out.status,
            combined.trim()
        ));
    }
    Ok(())
}

/// Extract SVG dimensions from the viewBox or width/height attributes.
/// Returns (width, height) in logical pixels.
fn svg_size(data: &[u8]) -> Option<(u32, u32)> {
    let s = std::str::from_utf8(data).ok()?;
    // Try viewBox first: viewBox="0 0 W H"
    if let Some(vb_start) = s.find("viewBox=\"") {
        let rest = &s[vb_start + 9..];
        if let Some(end) = rest.find('"') {
            let parts: Vec<&str> = rest[..end].split_whitespace().collect();
            if parts.len() == 4 {
                let w: f64 = parts[2].parse().ok()?;
                let h: f64 = parts[3].parse().ok()?;
                if w > 0.0 && h > 0.0 {
                    return Some((w.ceil() as u32, h.ceil() as u32));
                }
            }
        }
    }
    // Fallback: width="N" height="N" attributes
    let parse_attr = |attr: &str| -> Option<u32> {
        let needle = format!("{attr}=\"");
        let start = s.find(&needle)? + needle.len();
        let rest = &s[start..];
        let end = rest.find('"')?;
        let val: f64 = rest[..end].trim_end_matches("px").parse().ok()?;
        Some(val.ceil() as u32)
    };
    let w = parse_attr("width")?;
    let h = parse_attr("height")?;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// escape_html escapes the handful of characters that matter in HTML text.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// js_string JSON-encodes a value for embedding inside a <script> block,
/// escaping "</" so the sequence can never close the script tag early
/// (mermaid labels may legitimately contain "</script>").
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace("</", "<\\/")
}

/// Embedded at compile time; see `templates/visualize-viewer.{html,js}`.
/// Substitution tokens are `__NAME__` so they don't collide with CSS or
/// JS brace syntax. Keep both files in sync — the HTML expects the JS
/// template to provide `setSvg`, `fit`, `zoomBy`, `reset`, and
/// `downloadSvg`.
const VIEWER_HTML: &str = include_str!("../templates/visualize-viewer.html");
const VIEWER_JS: &str = include_str!("../templates/visualize-viewer.js");

fn viewer_html(title: &str, slug: &str, svg: &str, code: &str, theme: &Theme) -> String {
    // JS first, then the HTML wraps it.
    let viewer_js = VIEWER_JS
        .replace("__SVG_JS__", &js_string(svg))
        .replace("__SLUG__", slug);

    VIEWER_HTML
        .replace("__TITLE__", &escape_html(title))
        .replace("__CSS_VARS__", &style::css_vars(theme))
        .replace("__CODE_HTML__", &escape_html(code))
        .replace("__VIEWER_JS__", &viewer_js)
}

/// execute_visualize renders a Mermaid diagram. On success the result
/// embeds the machine-readable `[atom-diagram]` marker line the TUI
/// parses; it is stripped from the transcript rendering.
pub async fn execute_visualize(args_json: &str, _ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        code: String,
        #[serde(default)]
        title: String,
        /// Edge-label color tokens: label text -> blue|green|orange|pink.
        #[serde(default)]
        label_colors: Vec<(String, String)>,
    }
    if args_json.trim().is_empty() {
        return ToolOutcome::from_text(crate::exec::empty_arguments_msg("visualize"));
    }
    let args: Args = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::from_text(format!("error parsing arguments: {e}")),
    };
    let code = args.code.trim().to_string();
    if code.is_empty() {
        return ToolOutcome::from_text("error: code (mermaid source) is required".into());
    }
    let mut edge_colors = Vec::with_capacity(args.label_colors.len());
    let theme = style::theme();
    for (label, token) in &args.label_colors {
        match style::label_token_color(&theme, token.trim().to_lowercase().as_str()) {
            Some(color) => edge_colors.push((label.trim().to_string(), color)),
            None => {
                return ToolOutcome::from_text(format!(
                    "error: unknown label color token {token:?} (use blue, green, orange, pink)"
                ));
            }
        }
    }
    let Some(bin) = renderer() else {
        return ToolOutcome::from_text(
            "error: no mermaid renderer found (merman-cli, mmdc). Restart atom to install it \
             via the startup dependency check, or run: `brew install merman-cli` \
             (native, no Node; or: cargo install merman-cli)."
                .into(),
        );
    };

    let title = {
        let t = args.title.trim();
        if t.is_empty() {
            "Diagram"
        } else {
            t
        }
    };

    let svg = match render_mermaid(&code, &bin).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::from_text(format!("error: {e}")),
    };

    // Normalize edge-label geometry, then apply the atom diagram theme
    // (both passes idempotent; see atom_core::render::mermaid). The raw
    // SVG is also written: the TUI rasterizer re-applies the theme from
    // it at paint time, so the preview follows theme changes live.
    let raw = atom_core::render::mermaid::inject_edge_colors(
        &String::from_utf8_lossy(&svg),
        &edge_colors,
    );
    let svg = {
        let text = String::from_utf8_lossy(&svg).into_owned();
        let normalized = atom_core::render::mermaid::normalize_edge_labels(&text);
        let theme = atom_core::render::mermaid::diagram_theme_from(&style::theme());
        atom_core::render::mermaid::apply_diagram_theme(&normalized, &theme, &edge_colors)
            .into_bytes()
    };

    let Some((w, h)) = svg_size(&svg) else {
        return ToolOutcome::from_text("error: rendered SVG has no dimensions".into());
    };

    // Content-addressed artifacts: identical sources reuse the same
    // files. The theme fingerprint keys the artifacts so a theme switch
    // renders fresh ones.
    let active = style::theme();
    let hash = atom_core::util::sha256_hash(
        format!(
            "{}{}{}{:?}",
            style::theme_fingerprint(&active),
            style::mermaid_theme_variables(&active),
            code,
            edge_colors
        )
        .as_bytes(),
    );
    let hash8: String = hash.chars().take(8).collect();
    let slug = slugify(title);
    let stem = format!("{slug}-{hash8}");
    let dir = diagram_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ToolOutcome::from_text(format!("error: create {}: {e}", dir.display()));
    }
    let raw_path = dir.join(format!("{stem}-raw.svg"));
    let svg_path = dir.join(format!("{stem}.svg"));
    let html_path = dir.join(format!("{stem}.html"));
    if let Err(e) = std::fs::write(&raw_path, &raw) {
        return ToolOutcome::from_text(format!("error: write raw svg: {e}"));
    }
    if let Err(e) = std::fs::write(&svg_path, &svg) {
        return ToolOutcome::from_text(format!("error: write svg: {e}"));
    }
    let svg_str = match std::str::from_utf8(&svg) {
        Ok(s) => s,
        Err(_) => return ToolOutcome::from_text("error: svg is not valid utf-8".into()),
    };
    if let Err(e) = std::fs::write(
        &html_path,
        viewer_html(title, &slug, svg_str, &code, &active),
    ) {
        return ToolOutcome::from_text(format!("error: write viewer: {e}"));
    }

    // The result is the bare machine-readable marker line: the block
    // header already shows the title and the diagram itself is rendered
    // inline, so no human-readable prose is added around it. The svg
    // path is the RAW artifact: the TUI re-themes it against the live
    // palette at raster time.
    ToolOutcome::from_text(diagram_marker(
        &raw_path.display().to_string(),
        &html_path.display().to_string(),
        w,
        h,
    ))
}

// ---------------------------------------------------------------------------
// Marker helpers (shared contract with the TUI's block parser).
// ---------------------------------------------------------------------------

/// The exact machine-readable marker line embedded in visualize results.
/// Paths are double-quoted: the artifacts dir contains a space on macOS
/// (`~/Library/Application Support/atom/diagrams/`), and unquoted paths
/// would break the TUI's marker parser as well as terminal link
/// detection.
pub fn diagram_marker(svg: &str, html: &str, w: u32, h: u32) -> String {
    format!("[atom-diagram] svg=\"{svg}\" html=\"{html}\" width={w} height={h}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_sanitizes_and_truncates() {
        assert_eq!(slugify("System Architecture"), "system-architecture");
        assert_eq!(slugify("  A/B<C>  "), "a-b-c");
        assert_eq!(slugify(""), "diagram");
        assert_eq!(slugify("---"), "diagram");
        let long = slugify(&"x".repeat(100));
        assert_eq!(long.len(), 40);
    }

    #[test]
    fn diagram_marker_quotes_paths() {
        let marker = diagram_marker(
            "/Users/a/Library/Application Support/atom/diagrams/arch-1a2b3c4d.svg",
            "/Users/a/Library/Application Support/atom/diagrams/arch-1a2b3c4d.html",
            400,
            200,
        );
        assert_eq!(
            marker,
            "[atom-diagram] svg=\"/Users/a/Library/Application Support/atom/diagrams/arch-1a2b3c4d.svg\" \
             html=\"/Users/a/Library/Application Support/atom/diagrams/arch-1a2b3c4d.html\" \
             width=400 height=200"
        );
    }

    #[test]
    fn svg_size_from_viewbox() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800.5 400.2"></svg>"#;
        assert_eq!(svg_size(svg), Some((801, 401)));
    }

    #[test]
    fn svg_size_from_attributes() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="300"></svg>"#;
        assert_eq!(svg_size(svg), Some((600, 300)));
    }

    #[test]
    fn svg_size_from_px_attributes() {
        let svg = br#"<svg width="600px" height="300px"></svg>"#;
        assert_eq!(svg_size(svg), Some((600, 300)));
    }

    #[test]
    fn escape_html_escapes_text_nodes() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn js_string_escapes_script_closers() {
        assert_eq!(js_string("a</script>b"), "\"a<\\/script>b\"");
        assert_eq!(
            js_string("flowchart TD\nA --> B"),
            "\"flowchart TD\\nA --> B\""
        );
    }

    #[test]
    fn viewer_html_embeds_source_and_fallback() {
        let theme = Theme::default();
        let html = viewer_html(
            "Arch",
            "arch",
            "<svg id=\"x\"></svg>",
            "flowchart TD\nA --> B",
            &theme,
        );
        assert!(html.contains("<title>Arch</title>"));
        assert!(html.contains(">Arch</div>"));
        assert!(html.contains("flowchart TD\nA --&gt; B"));
        assert!(html.contains("svg id=\\\"x\\\""));
        assert!(html.contains("<\\/svg>"));
        assert!(html.contains("id=\"viewport\""));
        assert!(!html.contains("stage-container"));
        assert!(html.contains("a.download = \"arch.svg\""));
        assert!(html.contains("ResizeObserver"));
        assert!(!html.contains("scheduleFit"));
        // The viewer shows the offline SVG directly; no second renderer.
        assert!(!html.contains("mermaid@11"));
    }

    #[test]
    fn mermaid_config_uses_shared_theme_variables() {
        let theme = Theme::default();
        let cfg = mermaid_config(&theme);
        // Keep the native-SVG-text forcing resvg relies on.
        assert!(cfg.contains("\"htmlLabels\":false"), "cfg: {cfg}");
        assert!(cfg.contains("\"flowchart\""), "cfg: {cfg}");
        assert!(cfg.contains("\"sequence\""), "cfg: {cfg}");
        let tv = style::mermaid_theme_variables(&theme);
        assert!(cfg.contains(&tv), "cfg: {cfg}\ntv: {tv}");
    }

    #[test]
    fn theme_variables_derive_from_the_palette() {
        let theme = Theme::default();
        let tv = style::mermaid_theme_variables(&theme);
        // Defaults: muted card fill, theme foreground text.
        assert!(tv.contains(r##""mainBkg":"#3d3d3d""##), "tv: {tv}");
        assert!(tv.contains(r##""nodeTextColor":"#ced5d9""##), "tv: {tv}");
        assert!(tv.contains(r#""clusterBkg":"transparent""#), "tv: {tv}");
        assert!(tv.contains(r##""clusterBorder":"#272b33""##), "tv: {tv}");
    }

    #[test]
    fn theme_variables_follow_a_changed_palette() {
        let mut theme = Theme::default();
        theme.muted_extra = "#123456".into();
        theme.foreground = "#fedcba".into();
        let tv = style::mermaid_theme_variables(&theme);
        assert!(tv.contains(r##""mainBkg":"#123456""##), "tv: {tv}");
        assert!(tv.contains(r##""nodeTextColor":"#fedcba""##), "tv: {tv}");
    }

    #[test]
    fn label_tokens_cover_the_palette() {
        let theme = Theme::default();
        for token in ["blue", "green", "orange", "pink"] {
            assert!(style::label_token_color(&theme, token).is_some());
        }
        assert!(style::label_token_color(&theme, "purple").is_none());
        assert_eq!(
            style::label_token_color(&theme, "blue"),
            Some(theme.primary.clone())
        );
    }

    #[test]
    fn dot_is_half_opacity() {
        assert_eq!(style::dot(&Theme::default()), "rgba(61,61,61,0.5)");
    }

    #[test]
    fn css_vars_cover_all_template_colors() {
        let vars = style::css_vars(&Theme::default());
        for name in ["--bg", "--card", "--border", "--fg", "--muted", "--dot"] {
            assert!(vars.contains(name), "vars: {vars}");
        }
        assert!(vars.contains("--dot: rgba("), "vars: {vars}");
    }
}
