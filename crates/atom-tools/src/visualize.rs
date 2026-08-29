//! visualize tool: renders Mermaid diagrams to SVG for inline display
//! in the TUI (rasterized on-demand via resvg at the exact terminal
//! dimensions) plus a self-contained, pan/zoom HTML viewer for the
//! browser.
//!
//! Rendering shells out to a browserless Mermaid CLI — `merman-cli`
//! (native Rust, preferred) or the official `mmdc` (Node) — producing
//! a single SVG. The TUI rasterizes this SVG at paint time (in the
//! kitty graphics paint pass) using resvg with a proper card background
//! matched to the terminal theme. The browser viewer re-renders with the
//! real Mermaid.js from a CDN (theme-aware) and falls back to the SVG
//! when offline.
//!
//! Artifacts are content-addressed under the app data dir
//! (`<data>/atom/diagrams/<slug>-<hash8>.{svg,html}`): identical
//! Mermaid sources reuse the same files across calls and sessions.
//!
//! The tool result embeds a single machine-readable marker line
//! (`[atom-diagram] svg="…" html="…" width=… height=…`)
//! that the TUI parses to paint the inline image; it is stripped from
//! the rendered summary. Paths are quoted because the artifacts dir
//! contains a space on macOS.

use crate::{ToolCtx, ToolOutcome};
use atom_core::render::colors::COLOR_BORDER;
use std::path::PathBuf;

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

/// Builds the mermaid CLI `config.json`. Forces native SVG `<text>`
/// labels (htmlLabels:false — resvg can't render `<foreignObject>`) and
/// overrides the dark theme's default muted subgraph fill so clusters
/// render as transparent boxes with a border. `themeVariables` only
/// change the default cluster fill; explicit user-supplied
/// `style ... fill:` directives are left untouched.
fn mermaid_config() -> String {
    format!(
        r#"{{"flowchart":{{"htmlLabels":false}},"sequence":{{"htmlLabels":false}},"htmlLabels":false,"themeVariables":{{"clusterBkg":"transparent","clusterBorder":"{COLOR_BORDER}"}}}}"#
    )
}

/// Renders Mermaid `code` to SVG via the external CLI.
/// Uses the dark theme so diagrams look native in atom's dark TUI.
/// Forces native SVG text (htmlLabels:false) so resvg can render labels.
async fn render_mermaid(code: &str, bin: &std::path::Path) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let src = dir.path().join("diagram.mmd");
    std::fs::write(&src, code).map_err(|e| format!("write source: {e}"))?;
    let svg_path = dir.path().join("diagram.svg");

    // Config that forces SVG <text> instead of <foreignObject> (which
    // resvg can't render). htmlLabels:false is the key setting.
    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, mermaid_config()).map_err(|e| format!("write config: {e}"))?;

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

/// viewer_html builds the self-contained browser viewer. Dark theme with
/// dot grid background, pan/zoom via pointer drag and wheel.
/// Colors sourced from atom's palette (colors.rs constants).
fn viewer_html(title: &str, slug: &str, svg: &str, code: &str) -> String {
    // Pull colors from the palette at build time.
    use atom_core::render::colors::*;
    let bg = COLOR_BACKGROUND; // #111112
    let card = COLOR_CARD_DARK; // #151516
    let border = COLOR_BORDER; // #272b33
    let muted_extra = COLOR_MUTED_EXTRA; // #3d3d3d
    let muted = COLOR_MUTED; // #666666
    let fg = COLOR_FOREGROUND; // #ced5d9

    // Fill the JS template first so its `__BORDER__` is gone before the
    // HTML pass runs (the HTML also has a `__BORDER__` for the CSS).
    let viewer_js = VIEWER_JS
        .replace("__BORDER__", border)
        .replace("__CODE_JS__", &js_string(code))
        .replace("__SVG_JS__", &js_string(svg))
        .replace("__SLUG__", slug);

    VIEWER_HTML
        .replace("__TITLE__", &escape_html(title))
        .replace("__BG__", bg)
        .replace("__CARD__", card)
        .replace("__BORDER__", border)
        .replace("__MUTED_EXTRA__", muted_extra)
        .replace("__MUTED__", muted)
        .replace("__FG__", fg)
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

    // Normalize edge-label geometry before the SVG goes anywhere: merman
    // anchors edge-label text W/2 left of the edge midpoint (real Mermaid
    // offsets the background rect instead). Without this the label slides
    // under the source node when rasterized, and SVGs downloaded from the
    // browser viewer are broken in other tools too. Idempotent; see
    // atom_core::render::mermaid for the geometry.
    let svg = {
        let text = String::from_utf8_lossy(&svg).into_owned();
        atom_core::render::mermaid::normalize_edge_labels(&text).into_bytes()
    };

    let Some((w, h)) = svg_size(&svg) else {
        return ToolOutcome::from_text("error: rendered SVG has no dimensions".into());
    };

    // Content-addressed artifacts: identical sources reuse the same files.
    let hash = atom_core::util::sha256_hash(code.as_bytes());
    let hash8: String = hash.chars().take(8).collect();
    let slug = slugify(title);
    let stem = format!("{slug}-{hash8}");
    let dir = diagram_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ToolOutcome::from_text(format!("error: create {}: {e}", dir.display()));
    }
    let svg_path = dir.join(format!("{stem}.svg"));
    let html_path = dir.join(format!("{stem}.html"));
    if let Err(e) = std::fs::write(&svg_path, &svg) {
        return ToolOutcome::from_text(format!("error: write svg: {e}"));
    }
    let svg_str = match std::str::from_utf8(&svg) {
        Ok(s) => s,
        Err(_) => return ToolOutcome::from_text("error: svg is not valid utf-8".into()),
    };
    if let Err(e) = std::fs::write(&html_path, viewer_html(title, &slug, svg_str, &code)) {
        return ToolOutcome::from_text(format!("error: write viewer: {e}"));
    }

    // The result is the bare machine-readable marker line: the block
    // header already shows the title and the diagram itself is rendered
    // inline, so no human-readable prose is added around it.
    ToolOutcome::from_text(diagram_marker(
        &svg_path.display().to_string(),
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
        let html = viewer_html(
            "Arch",
            "arch",
            "<svg id=\"x\"></svg>",
            "flowchart TD\nA --> B",
        );
        assert!(html.contains("<title>Arch</title>"));
        assert!(html.contains(">Arch</div>"));
        assert!(html.contains("flowchart TD\\nA --> B"));
        assert!(html.contains("flowchart TD\nA --&gt; B"));
        assert!(html.contains("svg id=\\\"x\\\""));
        assert!(html.contains("<\\/svg>"));
        assert!(html.contains("id=\"viewport\""));
        assert!(!html.contains("stage-container"));
        assert!(html.contains("mermaid@11"));
        assert!(html.contains("a.download = \"arch.svg\""));
        // ResizeObserver replaces old scheduleFit/setTimeout approach
        assert!(html.contains("ResizeObserver"));
        assert!(!html.contains("scheduleFit"));
    }

    #[test]
    fn mermaid_config_renders_clusters_transparent_with_border() {
        // Subgraph/group clusters should render as a bordered, empty
        // (transparent) box rather than the dark theme's muted fill.
        let cfg = mermaid_config();
        assert!(cfg.contains("\"clusterBkg\":\"transparent\""), "cfg: {cfg}");
        assert!(cfg.contains("\"clusterBorder\":\"#272b33\""), "cfg: {cfg}");
        // Keep the native-SVG-text forcing resvg relies on.
        assert!(cfg.contains("\"htmlLabels\":false"), "cfg: {cfg}");
        assert!(cfg.contains("\"flowchart\""), "cfg: {cfg}");
        assert!(cfg.contains("\"sequence\""), "cfg: {cfg}");
    }

    #[test]
    fn viewer_html_sets_cluster_theme_variables_for_mermaid_render() {
        // The onload Mermaid.js re-render in the browser viewer must
        // match the offline SVG: transparent cluster fill + border.
        let html = viewer_html(
            "Arch",
            "arch",
            "<svg id=\"x\"></svg>",
            "flowchart TD\nsubgraph bins\nA\nend",
        );
        assert!(
            html.contains(
                "themeVariables: { clusterBkg: \"transparent\", clusterBorder: \"#272b33\" }"
            ),
            "html: {html}"
        );
    }
}
