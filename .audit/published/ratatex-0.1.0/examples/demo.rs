use std::{
    io::{self, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatex::{Formula, FormulaState, FormulaWidget, Ratatex, TerminalProfile, compact_latex};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};

const NAVIER_STOKES: &str = r"\rho\left(
    \frac{\partial \mathbf{u}}{\partial t}
    +(\mathbf{u}\cdot\nabla)\mathbf{u}
    \right)
    =-\nabla p+\mu\nabla^2\mathbf{u}+\rho\mathbf{f}";
const INCOMPRESSIBILITY: &str = r"\nabla\cdot\mathbf{u}=0";

#[derive(Clone, Copy, Debug, Default)]
struct DemoLayout {
    navier_stokes: Rect,
    incompressibility: Rect,
}

impl DemoLayout {
    fn contains_formula(self, position: Position) -> bool {
        self.navier_stokes.contains(position) || self.incompressibility.contains(position)
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut output = io::stdout();
    enable_raw_mode().wrap_err("failed to enable terminal raw mode")?;
    execute!(output, EnterAlternateScreen, EnableMouseCapture)
        .wrap_err("failed to enter alternate screen")?;
    let mut guard = TerminalGuard { active: true };

    let profile = TerminalProfile::query(Duration::from_millis(750));
    let (wake_tx, wake_rx) = mpsc::sync_channel(8);
    let renderer = Ratatex::builder(profile)
        .on_update(move || {
            let _ = wake_tx.try_send(());
        })
        .build()
        .wrap_err("failed to start Ratatex workers")?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(output)).wrap_err("failed to create terminal")?;
    terminal.clear()?;
    let formula_width = terminal.size()?.width.saturating_sub(8).max(1);
    prepare_first_frame(&renderer, &wake_rx, formula_width)?;

    let result = run(&mut terminal, &renderer, &wake_rx);
    renderer.shutdown();
    terminal.show_cursor()?;
    guard.restore()?;
    result
}

fn prepare_first_frame(
    renderer: &Ratatex,
    wake_rx: &mpsc::Receiver<()>,
    formula_width: u16,
) -> Result<()> {
    let formulas = [NAVIER_STOKES, INCOMPRESSIBILITY];
    for source in formulas {
        let _ = renderer.request(source, formula_width);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending = formulas.iter().any(|source| {
            matches!(
                renderer.request(source, formula_width),
                FormulaState::Pending
            )
        });
        if !pending {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            color_eyre::eyre::bail!("timed out preparing the first formula frame");
        }
        wake_rx
            .recv_timeout(remaining)
            .wrap_err("formula workers stopped before the first frame was ready")?;
    }
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    renderer: &Ratatex,
    wake_rx: &mpsc::Receiver<()>,
) -> Result<()> {
    let mut source_mode = false;
    let mut layout = DemoLayout::default();
    let mut redraw = true;
    loop {
        if redraw {
            flush_commands(terminal, renderer)?;
            terminal.draw(|frame| {
                layout = draw(frame, renderer, source_mode);
            })?;
            redraw = false;
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && (matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                            || key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)) =>
                {
                    return Ok(());
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('r')
                        && source_mode =>
                {
                    source_mode = false;
                    set_mouse_capture(terminal, true)?;
                    redraw = true;
                }
                Event::Mouse(mouse)
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && !source_mode
                        && layout.contains_formula(Position::new(mouse.column, mouse.row)) =>
                {
                    source_mode = true;
                    set_mouse_capture(terminal, false)?;
                    redraw = true;
                }
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }
        while wake_rx.try_recv().is_ok() {
            redraw = true;
        }
    }
}

fn flush_commands(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    renderer: &Ratatex,
) -> Result<()> {
    let commands = renderer.drain_terminal_commands();
    if commands.is_empty() {
        return Ok(());
    }
    let backend = terminal.backend_mut();
    for command in commands {
        backend.write_all(command.as_bytes())?;
    }
    backend.flush()?;
    Ok(())
}

fn set_mouse_capture(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    enabled: bool,
) -> Result<()> {
    let backend = terminal.backend_mut();
    if enabled {
        execute!(backend, EnableMouseCapture)?;
    } else {
        execute!(backend, DisableMouseCapture)?;
    }
    backend.flush()?;
    Ok(())
}

fn draw(frame: &mut ratatui::Frame<'_>, renderer: &Ratatex, source_mode: bool) -> DemoLayout {
    let area = frame.area();
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" ratatex · TeX display math in Ratatui "),
        area,
    );
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 3,
        vertical: 2,
    });
    if inner.is_empty() {
        return DemoLayout::default();
    }

    let formula_width = inner.width.saturating_sub(2).max(1);
    let navier = renderer.request(NAVIER_STOKES, formula_width);
    let incompressibility = renderer.request(INCOMPRESSIBILITY, formula_width);
    let navier_rows = ready_rows(&navier).unwrap_or(3);
    let incompressibility_rows = ready_rows(&incompressibility).unwrap_or(2);
    let constraints = [
        Constraint::Length(4),
        Constraint::Length(navier_rows),
        Constraint::Length(2),
        Constraint::Length(incompressibility_rows),
        Constraint::Min(3),
        Constraint::Length(1),
    ];
    let [
        intro,
        navier_area,
        condition,
        incompressibility_area,
        details,
        footer,
    ] = Layout::vertical(constraints).areas(inner);

    let availability = renderer.availability();
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from("For an incompressible Newtonian fluid:"),
            Line::default(),
            Line::styled(
                if availability.fully_available() {
                    "in-process TeX layout + Kitty placeholders ready"
                } else {
                    "using text fallback; see README requirements"
                },
                Style::default().fg(if availability.fully_available() {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
        ])),
        intro,
    );
    let navier_source = compact_latex(&format!("$$\n{NAVIER_STOKES}\n$$"));
    draw_state(
        frame,
        navier_area,
        &navier,
        "$$ … Navier–Stokes … $$",
        &navier_source,
        source_mode,
    );
    frame.render_widget(
        Paragraph::new("with the incompressibility condition"),
        condition,
    );
    let incompressibility_source = compact_latex(&format!("$$\n{INCOMPRESSIBILITY}\n$$"));
    draw_state(
        frame,
        incompressibility_area,
        &incompressibility,
        r"$$ \nabla\cdot\mathbf{u}=0 $$",
        &incompressibility_source,
        source_mode,
    );
    draw_copy_help(frame, details, footer, source_mode);

    DemoLayout {
        navier_stokes: formula_hit_area(navier_area, &navier),
        incompressibility: formula_hit_area(incompressibility_area, &incompressibility),
    }
}

fn draw_copy_help(frame: &mut ratatui::Frame<'_>, details: Rect, footer: Rect, source_mode: bool) {
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::default(),
            Line::from(if source_mode {
                "LaTeX copy mode: drag-select equations together with surrounding text."
            } else {
                "Click any equation to make every equation selectable as LaTeX."
            }),
            Line::from("The replacement keeps the same rows, so the transcript does not jump."),
        ])),
        details,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            if source_mode {
                "r restores graphics · q / Esc / Ctrl-C quits"
            } else {
                "q / Esc / Ctrl-C to quit"
            },
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
        footer,
    );
}

fn ready_rows(state: &FormulaState) -> Option<u16> {
    match state {
        FormulaState::Ready(formula) => Some(formula.rows()),
        FormulaState::Pending | FormulaState::Failed(_) | FormulaState::Unsupported => None,
    }
}

fn formula_hit_area(area: Rect, state: &FormulaState) -> Rect {
    match state {
        FormulaState::Ready(formula) => Rect::new(
            area.x,
            area.y,
            formula.columns().min(area.width),
            formula.rows().min(area.height),
        ),
        FormulaState::Pending | FormulaState::Failed(_) | FormulaState::Unsupported => area,
    }
}

fn draw_state(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &FormulaState,
    fallback: &str,
    source: &str,
    show_source: bool,
) {
    match state {
        FormulaState::Ready(formula) => draw_ready(frame, area, formula, source, show_source),
        FormulaState::Pending | FormulaState::Unsupported => {
            frame.render_widget(
                Paragraph::new(if show_source { source } else { fallback })
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        FormulaState::Failed(error) => frame.render_widget(
            Paragraph::new(format!(
                "{}\nRatatex: {error}",
                if show_source { source } else { fallback }
            ))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
            area,
        ),
    }
}

fn draw_ready(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    formula: &Formula,
    source: &str,
    show_source: bool,
) {
    let widget = FormulaWidget::new(formula);
    if show_source {
        widget
            .compact_source_fallback(source)
            .source_fallback_style(Style::default().fg(Color::White))
            .render(area, frame.buffer_mut());
    } else {
        widget.render(area, frame.buffer_mut());
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn restore(&mut self) -> Result<()> {
        disable_raw_mode().wrap_err("failed to disable terminal raw mode")?;
        execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)
            .wrap_err("failed to leave alternate screen")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        }
    }
}
