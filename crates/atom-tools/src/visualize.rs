//! visualize tool: renders Mermaid diagrams to a high-density PNG for
//! inline kitty-graphics display in the TUI plus a self-contained,
//! pan/zoom HTML viewer for the browser.
//!
//! Rendering shells out to a browserless Mermaid CLI — `merman-cli`
//! (native Rust, preferred) or the official `mmdc` (Node) — using the
//! shared `mmdc` dialect both accept: `-i <in> -o <out> -s <scale>
//! -b <bg>`. Both write Mermaid-parity SVG, which is what the HTML
//! viewer embeds, so browser zoom stays vector-crisp.
//!
//! Artifacts are content-addressed under the app data dir
//! (`<data>/atom/diagrams/<slug>-<hash8>.{png,html}`): identical
//! Mermaid sources reuse the same files across calls and sessions.
//!
//! The tool result embeds a single machine-readable marker line
//! (`[atom-diagram] png=… html=… width=… height=…`) that the TUI parses
//! to paint the inline image; it is stripped from the rendered summary.

use crate::{ToolCtx, ToolOutcome};
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

/// Renders Mermaid `code` to (svg_bytes, png_bytes) via the external CLI.
/// Uses a tempdir workspace because both CLIs want file inputs/outputs.
async fn render_mermaid(code: &str, bin: &std::path::Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let src = dir.path().join("diagram.mmd");
    std::fs::write(&src, code).map_err(|e| format!("write source: {e}"))?;
    let svg_path = dir.path().join("diagram.svg");
    let png_path = dir.path().join("diagram.png");

    // SVG (mermaid-parity, for the browser viewer) then PNG (2x density,
    // transparent background so kitty composites onto the terminal bg).
    let run = |out: &std::path::Path, extra: &[String]| -> Vec<String> {
        let mut args = vec![
            "-i".to_string(),
            src.display().to_string(),
            "-o".to_string(),
            out.display().to_string(),
        ];
        args.extend(extra.iter().cloned());
        args
    };
    renderer_run(bin, &run(&svg_path, &[])).await?;
    renderer_run(
        bin,
        &run(
            &png_path,
            &[
                "-s".to_string(),
                "2".to_string(),
                "-b".into(),
                "transparent".into(),
            ],
        ),
    )
    .await?;

    let svg = std::fs::read(&svg_path).map_err(|e| format!("read svg: {e}"))?;
    let png = std::fs::read(&png_path).map_err(|e| format!("read png: {e}"))?;
    if svg.is_empty() || png.is_empty() {
        return Err("renderer produced empty output".to_string());
    }
    Ok((svg, png))
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

/// PNG pixel dimensions without a full decode.
fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// escape_html escapes the handful of characters that matter in HTML text.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// viewer_html builds the self-contained browser viewer: inlined SVG,
/// drag-to-pan, wheel/pinch zoom, keyboard navigation, fit/reset, and
/// the mermaid source in a collapsible section. No external assets —
/// works offline from file://.
fn viewer_html(title: &str, svg: &str, code: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{ color-scheme: light dark; }}
  html, body {{ margin: 0; height: 100%; overflow: hidden;
    background: light-dark(#fafafa, #14161a); }}
  header {{ display: flex; align-items: center; gap: 12px; padding: 8px 14px;
    font: 13px -apple-system, "Segoe UI", sans-serif;
    color: light-dark(#333, #ccc); border-bottom: 1px solid light-dark(#e2e2e2, #2a2a2a);
    user-select: none; }}
  header .title {{ font-weight: 600; flex: 1; overflow: hidden;
    text-overflow: ellipsis; white-space: nowrap; }}
  header button {{ font: inherit; border: 1px solid light-dark(#ccc, #3a3a3a);
    background: transparent; color: inherit; border-radius: 6px;
    padding: 2px 10px; cursor: pointer; }}
  header button:hover {{ background: light-dark(#ececec, #24262b); }}
  #stage {{ width: 100%; height: calc(100% - 37px); cursor: grab; touch-action: none; }}
  #stage {{ transform-origin: 0 0; display: inline-block; }}
  #stage svg {{ display: block; max-width: none; }}
  details {{ position: fixed; right: 12px; bottom: 12px; max-width: min(640px, 80vw);
    background: light-dark(#ffffff, #1b1d22); border: 1px solid light-dark(#ddd, #2c2e33);
    border-radius: 8px; padding: 8px 12px; font: 12px ui-monospace, monospace;
    box-shadow: 0 4px 16px rgba(0,0,0,.15); }}
  details pre {{ max-height: 40vh; overflow: auto; white-space: pre-wrap; }}
  summary {{ cursor: pointer; font: 13px -apple-system, "Segoe UI", sans-serif; }}
</style>
</head>
<body>
<header>
  <div class="title">{title_html}</div>
  <button onclick="fit()">Fit</button>
  <button onclick="zoomBy(1.25)">+</button>
  <button onclick="zoomBy(0.8)">&minus;</button>
  <button onclick="reset()">100%</button>
  <span id="pct"></span>
</header>
<div id="stage-container" style="display:none"></div>
<div id="stage"></div>
<details><summary>Mermaid source</summary><pre>{code_html}</pre></details>
<script>
"use strict";
const stage = document.getElementById("stage");
const stageContainer = document.getElementById("stage-container");
const FIT_PAD = 32;
let scale = 1, tx = 0, ty = 0;

stage.innerHTML = {svg_js};
// Mermaid SVGs carry fixed width/height; swap to a viewBox so CSS
// scaling stays crisp, then hide the raw svg and drive the transform
// from the container.
const svgEl = stage.querySelector("svg");
let natW = 800, natH = 600;
if (svgEl) {{
  const vb = svgEl.viewBox.baseVal;
  if (vb && vb.width && vb.height) {{ natW = vb.width; natH = vb.height; }}
  else if (svgEl.width.baseVal.value && svgEl.height.baseVal.value) {{
    natW = svgEl.width.baseVal.value; natH = svgEl.height.baseVal.value;
  }}
  svgEl.removeAttribute("width"); svgEl.removeAttribute("height");
  svgEl.style.width = natW + "px"; svgEl.style.height = natH + "px";
}}
stage.style.width = natW + "px"; stage.style.height = natH + "px";

function apply() {{
  stage.style.transform = `translate(${{tx}}px, ${{ty}}px) scale(${{scale}})`;
  document.getElementById("pct").textContent = Math.round(scale * 100) + "%";
}}
function zoomBy(f, cx, cy) {{
  const r = stageContainer.getBoundingClientRect();
  cx = cx === undefined ? r.width / 2 : cx - r.left;
  cy = cy === undefined ? r.height / 2 : cy - r.top;
  const ns = Math.min(20, Math.max(0.02, scale * f));
  const k = ns / scale;
  tx = cx - (cx - tx) * k; ty = cy - (cy - ty) * k; scale = ns; apply();
}}
function fit() {{
  const r = stageContainer.getBoundingClientRect();
  scale = Math.min((r.width - FIT_PAD) / natW, (r.height - FIT_PAD) / natH);
  scale = Math.min(4, Math.max(0.02, scale));
  tx = (r.width - natW * scale) / 2; ty = (r.height - natH * scale) / 2; apply();
}}
function reset() {{ scale = 1; tx = 0; ty = 0; apply(); }}
let px = 0, py = 0, down = false;
stageContainer.addEventListener("pointerdown", e => {{
  down = true; px = e.clientX; py = e.clientY;
  stageContainer.style.cursor = "grabbing"; stageContainer.setPointerCapture(e.pointerId);
}});
stageContainer.addEventListener("pointermove", e => {{
  if (!down) return; tx += e.clientX - px; ty += e.clientY - py;
  px = e.clientX; py = e.clientY; apply();
}});
window.addEventListener("pointerup", () => {{
  down = false; stageContainer.style.cursor = "grab";
}});
stageContainer.addEventListener("wheel", e => {{
  e.preventDefault();
  zoomBy(e.deltaY < 0 ? 1.1 : 0.9, e.clientX, e.clientY);
}}, {{ passive: false }});
stageContainer.addEventListener("dblclick", fit);
window.addEventListener("keydown", e => {{
  if (e.key === "+" || e.key === "=") zoomBy(1.25);
  else if (e.key === "-") zoomBy(0.8);
  else if (e.key === "0") reset();
  else if (e.key === "f") fit();
  else if (e.key.startsWith("Arrow")) {{
    const d = 40 * (e.shiftKey ? 3 : 1);
    if (e.key === "ArrowLeft") tx += d;
    if (e.key === "ArrowRight") tx -= d;
    if (e.key === "ArrowUp") ty += d;
    if (e.key === "ArrowDown") ty -= d;
    apply(); e.preventDefault();
  }}
}});
window.addEventListener("resize", fit);
fit();
</script>
</body>
</html>
"#,
        title = escape_html(title),
        title_html = escape_html(title),
        code_html = escape_html(code),
        svg_js = serde_json::to_string(svg).unwrap_or_else(|_| "\"\"".into()),
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

    let (svg, png) = match render_mermaid(&code, &bin).await {
        Ok(v) => v,
        Err(e) => return ToolOutcome::from_text(format!("error: {e}")),
    };
    let Some((w, h)) = png_size(&png) else {
        return ToolOutcome::from_text("error: rendered PNG has no dimensions".into());
    };

    // Content-addressed artifacts: identical sources reuse the same files.
    let hash = atom_core::util::sha256_hash(code.as_bytes());
    let hash8: String = hash.chars().take(8).collect();
    let stem = format!("{}-{}", slugify(title), hash8);
    let dir = diagram_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ToolOutcome::from_text(format!("error: create {}: {e}", dir.display()));
    }
    let png_path = dir.join(format!("{stem}.png"));
    let html_path = dir.join(format!("{stem}.html"));
    if let Err(e) = std::fs::write(&png_path, &png) {
        return ToolOutcome::from_text(format!("error: write png: {e}"));
    }
    let svg_str = match std::str::from_utf8(&svg) {
        Ok(s) => s,
        Err(_) => return ToolOutcome::from_text("error: svg is not valid utf-8".into()),
    };
    if let Err(e) = std::fs::write(&html_path, viewer_html(title, svg_str, &code)) {
        return ToolOutcome::from_text(format!("error: write viewer: {e}"));
    }

    ToolOutcome::from_text(format!(
        "rendered diagram \"{title}\" ({w}x{h} px, saved at 2x density)\n\
         inline preview is shown in the atom TUI; expand it for a pan/zoom view in the browser\n\
         {}",
        diagram_marker(
            &png_path.display().to_string(),
            &html_path.display().to_string(),
            w,
            h
        ),
    ))
}

// ---------------------------------------------------------------------------
// Marker helpers (shared contract with the TUI's block parser).
// ---------------------------------------------------------------------------

/// The exact machine-readable marker line embedded in visualize results.
pub fn diagram_marker(png: &str, html: &str, w: u32, h: u32) -> String {
    format!("[atom-diagram] png={png} html={html} width={w} height={h}")
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
    fn escape_html_escapes_text_nodes() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
