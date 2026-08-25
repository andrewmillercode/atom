#[test]
fn reasoning_block_produces_muted_style() {
    let text = "The user just sent test";
    let body = atom_core::render::links::wrap_linked(text, 80, atom_core::render::colors::COLOR_MUTED, "");
    let styled = format!(
        "{}{}\x1b[39m",
        atom_core::render::colors::ansi_fg(atom_core::render::colors::COLOR_MUTED),
        body
    );
    let lines = atom_tui::ansi::ansi_to_lines(&styled);
    println!("lines: {:?}", lines);
    for span in &lines[0].spans {
        println!("span: {:?} style: {:?}", span.content, span.style);
    }
    // The first span should have muted fg.
    assert!(!lines[0].spans.is_empty());
    let fg = lines[0].spans[0].style.fg;
    assert_eq!(fg, Some(ratatui::style::Color::Rgb(107, 110, 119)));
}
