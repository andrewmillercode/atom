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
        .join("atom")
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

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  html, body {{ height: 100%; overflow: hidden; background: {bg}; color: {fg}; }}
  body {{ display: flex; flex-direction: column; font: 13px/1.4 -apple-system, "Segoe UI", sans-serif; }}
  header {{ display: flex; align-items: center; gap: 12px; padding: 8px 14px;
    border-bottom: 1px solid {border}; user-select: none; z-index: 10; }}
  header .title {{ font-weight: 600; flex: 1; overflow: hidden;
    text-overflow: ellipsis; white-space: nowrap; }}
  header button {{ font: inherit; border: 1px solid {border};
    background: {card}; color: {fg}; border-radius: 6px;
    padding: 3px 10px; cursor: pointer; }}
  header button:hover {{ background: {border}; }}
  #viewport {{ flex: 1; position: relative; overflow: hidden; cursor: grab;
    touch-action: none; user-select: none;
    background-color: {bg};
    background-image: radial-gradient(circle, {muted_extra} 1px, transparent 1px);
    background-size: 24px 24px; }}
  #stage {{ position: absolute; left: 0; top: 0; transform-origin: 0 0; }}
  #stage svg {{ display: block; max-width: none; }}
  details {{ position: fixed; right: 12px; bottom: 12px; max-width: min(640px, 80vw);
    background: {card}; border: 1px solid {border};
    border-radius: 8px; padding: 8px 12px; font: 12px ui-monospace, monospace;
    box-shadow: 0 4px 16px rgba(0,0,0,.4); z-index: 10; }}
  details pre {{ max-height: 40vh; overflow: auto; white-space: pre-wrap; color: {muted}; }}
  summary {{ cursor: pointer; color: {muted}; }}
</style>
</head>
<body>
<header>
  <div class="title">{title_html}</div>
  <button onclick="fit()">Fit</button>
  <button onclick="zoomBy(1.25)">+</button>
  <button onclick="zoomBy(0.8)">&minus;</button>
  <button onclick="reset()">100%</button>
  <button onclick="downloadSvg()">SVG</button>
  <span id="pct" style="min-width:3em;text-align:right;color:{muted}"></span>
</header>
<div id="viewport"><div id="stage"></div></div>
<details><summary>&#9654; Mermaid source</summary><pre>{code_html}</pre></details>
<script>
"use strict";
const viewport = document.getElementById("viewport");
const stage = document.getElementById("stage");
let scale = 1, tx = 0, ty = 0, natW = 800, natH = 600;

function setSvg(svg) {{
  stage.innerHTML = svg;
  const el = stage.querySelector("svg");
  if (!el) return;
  const vb = el.viewBox && el.viewBox.baseVal;
  if (vb && vb.width > 0 && vb.height > 0) {{ natW = vb.width; natH = vb.height; }}
  else {{
    const w = el.width && el.width.baseVal.value;
    const h = el.height && el.height.baseVal.value;
    if (w > 0 && h > 0) {{ natW = w; natH = h; }}
  }}
  el.removeAttribute("width"); el.removeAttribute("height");
  el.style.width = natW + "px"; el.style.height = natH + "px";
  stage.style.width = natW + "px"; stage.style.height = natH + "px";
  fit();
}}

setSvg({svg_js});

const MERMAID_CDN = "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js";
function renderWithMermaid() {{
  if (typeof mermaid === "undefined") return;
  mermaid.initialize({{ startOnLoad: false, theme: "dark", securityLevel: "loose", themeVariables: {{ clusterBkg: "transparent", clusterBorder: "{border}" }} }});
  mermaid.render("mmd-" + Date.now(), {code_js})
    .then(({{ svg }}) => setSvg(svg))
    .catch(() => {{}});
}}
const s = document.createElement("script");
s.src = MERMAID_CDN; s.onload = renderWithMermaid;
document.head.appendChild(s);

new ResizeObserver(() => fit()).observe(viewport);

function apply() {{
  stage.style.transform = `translate(${{tx}}px,${{ty}}px) scale(${{scale}})`;
  document.getElementById("pct").textContent = Math.round(scale * 100) + "%";
}}
function zoomBy(f, cx, cy) {{
  const r = viewport.getBoundingClientRect();
  cx = cx === undefined ? r.width / 2 : cx - r.left;
  cy = cy === undefined ? r.height / 2 : cy - r.top;
  const ns = Math.min(20, Math.max(0.02, scale * f));
  const k = ns / scale;
  tx = cx - (cx - tx) * k; ty = cy - (cy - ty) * k; scale = ns;
  apply();
}}
function fit() {{
  const r = viewport.getBoundingClientRect();
  if (r.width < 1 || r.height < 1 || natW < 1 || natH < 1) return;
  scale = Math.min((r.width - 64) / natW, (r.height - 64) / natH, 4);
  scale = Math.max(0.02, scale);
  tx = (r.width - natW * scale) / 2;
  ty = (r.height - natH * scale) / 2;
  apply();
}}
function reset() {{
  const r = viewport.getBoundingClientRect();
  scale = 1;
  tx = (r.width - natW) / 2; ty = (r.height - natH) / 2;
  apply();
}}
function downloadSvg() {{
  const el = stage.querySelector("svg");
  if (!el) return;
  const blob = new Blob([new XMLSerializer().serializeToString(el)], {{type:"image/svg+xml"}});
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob); a.download = "{slug}.svg"; a.click();
}}
let px=0, py=0, down=false;
viewport.addEventListener("pointerdown", e => {{
  if (e.button !== 0) return;
  down = true; px = e.clientX; py = e.clientY;
  viewport.style.cursor = "grabbing"; viewport.setPointerCapture(e.pointerId);
}});
viewport.addEventListener("pointermove", e => {{
  if (!down) return;
  tx += e.clientX - px; ty += e.clientY - py;
  px = e.clientX; py = e.clientY; apply();
}});
window.addEventListener("pointerup", () => {{ down = false; viewport.style.cursor = "grab"; }});
viewport.addEventListener("wheel", e => {{
  e.preventDefault();
  zoomBy(e.deltaY < 0 ? 1.15 : 0.87, e.clientX, e.clientY);
}}, {{passive:false}});
viewport.addEventListener("dblclick", fit);
window.addEventListener("keydown", e => {{
  if (e.key === "+" || e.key === "=") zoomBy(1.25);
  else if (e.key === "-") zoomBy(0.8);
  else if (e.key === "0") reset();
  else if (e.key === "f") fit();
}});
</script>
</body>
</html>
"#,
        title = escape_html(title),
        title_html = escape_html(title),
        code_html = escape_html(code),
        code_js = js_string(code),
        svg_js = js_string(svg),
        slug = slug,
    )
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
