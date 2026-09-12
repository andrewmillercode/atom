//! Visual probe: renders the common diagram types through the full
//! pipeline (merman -> normalize -> apply_diagram_theme -> resvg) and
//! writes PNGs to the temp dir for eyeballing. Skips if merman-cli
//! is not installed.

use atom_core::render::colors::Theme;
use atom_core::render::mermaid::{apply_diagram_theme, diagram_theme_from, normalize_edge_labels};

fn theme() -> atom_core::render::mermaid::DiagramTheme {
    diagram_theme_from(&Theme::default())
}

fn merman_missing() -> bool {
    std::process::Command::new("merman-cli")
        .arg("--help")
        .output()
        .is_err()
}

fn render(name: &str, code: &str, colors: &[(String, String)]) {
    let dir = std::env::temp_dir().join("atom-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join(format!("{name}.mmd"));
    std::fs::write(&src, code).unwrap();
    let cfg = dir.join("cfg.json");
    std::fs::write(
        &cfg,
        r##"{"flowchart":{"htmlLabels":false},"sequence":{"htmlLabels":false},"htmlLabels":false,"themeVariables":{"clusterBkg":"transparent","clusterBorder":"#272b33","mainBkg":"#3d3d3d","nodeBorder":"#3d3d3d","nodeTextColor":"#ffffff","primaryTextColor":"#ffffff","edgeLabelBackground":"transparent"}}"##,
    )
    .unwrap();
    let out = std::process::Command::new("merman-cli")
        .args([
            "-i",
            src.to_str().unwrap(),
            "-o",
            dir.join(format!("{name}.svg")).to_str().unwrap(),
            "-t",
            "dark",
            "-b",
            "transparent",
            "-c",
            cfg.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let svg = std::fs::read_to_string(dir.join(format!("{name}.svg"))).unwrap();
    let themed = apply_diagram_theme(&normalize_edge_labels(&svg), &theme(), colors);
    std::fs::write(dir.join(format!("{name}-themed.svg")), &themed).unwrap();

    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(themed.as_bytes(), &opt).unwrap();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(1200, 900).unwrap();
    pixmap.fill(resvg::tiny_skia::Color::from_rgba8(0x11, 0x11, 0x12, 255));
    let size = tree.size();
    let scale = (1100.0 / size.width()).min(850.0 / size.height()).min(2.0);
    let tx = (1200.0 - size.width() * scale) / 2.0;
    let ty = (900.0 - size.height() * scale) / 2.0;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_translate(tx, ty).post_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    std::fs::write(
        dir.join(format!("{name}.png")),
        pixmap.encode_png().unwrap(),
    )
    .unwrap();

    let mut buckets = std::collections::BTreeMap::new();
    for px in pixmap.pixels() {
        let key = format!(
            "{:02x}{:02x}{:02x}",
            px.red() & 0xF8,
            px.green() & 0xF8,
            px.blue() & 0xF8
        );
        *buckets.entry(key).or_insert(0usize) += 1;
    }
    let top: Vec<String> = buckets
        .iter()
        .filter(|(k, _)| k.as_str() != "101010" && k.as_str() != "111110")
        .filter(|(_, n)| **n > 300)
        .map(|(k, n)| format!("{k}={n}"))
        .collect();
    println!("{name}: {top:?}");
}

#[test]
fn probe_all_diagram_types() {
    if merman_missing() {
        eprintln!("skipping: merman-cli not installed");
        return;
    }
    let token = |t: &str| match t {
        "green" => "#96d1ae",
        "orange" => "#e8a07a",
        "pink" => "#b491b0",
        _ => "#8cadd1",
    };
    let colors = [("start", "green"), ("pause", "orange"), ("cancel", "pink")]
        .iter()
        .map(|(l, t)| (l.to_string(), token(t).to_string()))
        .collect::<Vec<_>>();
    render(
        "state",
        "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Running: start\n  Running --> Paused: pause\n  Paused --> Running: resume\n  Running --> Done: finish\n  Paused --> Done: cancel",
        &colors,
    );
    render(
        "flowchart",
        "flowchart LR\n  A[Tool call] --> B{\"Layer 1<br/>static rules\"}\n  B -->|allow| C[\"run confined<br/>Seatbelt on\"]\n  subgraph crates\n    C --> D[atom-tui]\n  end",
        &[("allow".to_string(), token("green").to_string())],
    );
    render(
        "er",
        "erDiagram\n  USER ||--o{ POST : writes",
        &[("writes".to_string(), token("pink").to_string())],
    );
    render(
        "seq",
        "sequenceDiagram\n  User->>Server: request\n  Server-->>User: response",
        &[],
    );
    render(
        "class",
        "classDiagram\n  class Animal {\n    +String name\n    +walk()\n  }\n  Animal <|-- Dog",
        &[],
    );
}
