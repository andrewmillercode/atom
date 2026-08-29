"use strict";
const viewport = document.getElementById("viewport");
const stage = document.getElementById("stage");
let scale = 1, tx = 0, ty = 0, natW = 800, natH = 600;

function setSvg(svg) {
  stage.innerHTML = svg;
  const el = stage.querySelector("svg");
  if (!el) return;
  const vb = el.viewBox && el.viewBox.baseVal;
  if (vb && vb.width > 0 && vb.height > 0) { natW = vb.width; natH = vb.height; }
  else {
    const w = el.width && el.width.baseVal.value;
    const h = el.height && el.height.baseVal.value;
    if (w > 0 && h > 0) { natW = w; natH = h; }
  }
  el.removeAttribute("width"); el.removeAttribute("height");
  el.style.width = natW + "px"; el.style.height = natH + "px";
  stage.style.width = natW + "px"; stage.style.height = natH + "px";
  fit();
}

setSvg(__SVG_JS__);

const MERMAID_CDN = "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js";
function renderWithMermaid() {
  if (typeof mermaid === "undefined") return;
  mermaid.initialize({
    startOnLoad: false, theme: "dark", securityLevel: "loose",
    themeVariables: { clusterBkg: "transparent", clusterBorder: "__BORDER__" }
  });
  mermaid.render("mmd-" + Date.now(), __CODE_JS__)
    .then(({ svg }) => setSvg(svg))
    .catch(() => {});
}
const s = document.createElement("script");
s.src = MERMAID_CDN; s.onload = renderWithMermaid;
document.head.appendChild(s);

new ResizeObserver(() => fit()).observe(viewport);

function apply() {
  stage.style.transform = `translate(${tx}px,${ty}px) scale(${scale})`;
  document.getElementById("pct").textContent = Math.round(scale * 100) + "%";
}
function zoomBy(f, cx, cy) {
  const r = viewport.getBoundingClientRect();
  cx = cx === undefined ? r.width / 2 : cx - r.left;
  cy = cy === undefined ? r.height / 2 : cy - r.top;
  const ns = Math.min(20, Math.max(0.02, scale * f));
  const k = ns / scale;
  tx = cx - (cx - tx) * k; ty = cy - (cy - ty) * k; scale = ns;
  apply();
}
function fit() {
  const r = viewport.getBoundingClientRect();
  if (r.width < 1 || r.height < 1 || natW < 1 || natH < 1) return;
  scale = Math.min((r.width - 64) / natW, (r.height - 64) / natH, 4);
  scale = Math.max(0.02, scale);
  tx = (r.width - natW * scale) / 2;
  ty = (r.height - natH * scale) / 2;
  apply();
}
function reset() {
  const r = viewport.getBoundingClientRect();
  scale = 1;
  tx = (r.width - natW) / 2; ty = (r.height - natH) / 2;
  apply();
}
function downloadSvg() {
  const el = stage.querySelector("svg");
  if (!el) return;
  const blob = new Blob([new XMLSerializer().serializeToString(el)], {type: "image/svg+xml"});
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob); a.download = "__SLUG__.svg"; a.click();
}
let px = 0, py = 0, down = false;
viewport.addEventListener("pointerdown", e => {
  if (e.button !== 0) return;
  down = true; px = e.clientX; py = e.clientY;
  viewport.style.cursor = "grabbing"; viewport.setPointerCapture(e.pointerId);
});
viewport.addEventListener("pointermove", e => {
  if (!down) return;
  tx += e.clientX - px; ty += e.clientY - py;
  px = e.clientX; py = e.clientY; apply();
});
window.addEventListener("pointerup", () => { down = false; viewport.style.cursor = "grab"; });
viewport.addEventListener("wheel", e => {
  e.preventDefault();
  zoomBy(e.deltaY < 0 ? 1.05 : 0.95, e.clientX, e.clientY);
}, {passive: false});
viewport.addEventListener("dblclick", fit);
window.addEventListener("keydown", e => {
  if (e.key === "+" || e.key === "=") zoomBy(1.25);
  else if (e.key === "-") zoomBy(0.8);
  else if (e.key === "0") reset();
  else if (e.key === "f") fit();
});
