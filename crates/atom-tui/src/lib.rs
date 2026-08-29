//! atom-tui: the ratatui frontend for atom, ported from tui.go /
//! preview.go / outputtest.go / hot.go (Bubble Tea → tokio + ratatui).
//!
//! Entrypoints: [`run`] starts the interactive client against the atom
//! server; [`run_output_test`] replays the canned transcript without a
//! server. Handlers live on `App` and return `Effect`s so the UI logic
//! is unit-testable without a terminal; this module owns the real event
//! loop (tokio select! over crossterm input, per-session NDJSON streams,
//! and tickers) plus terminal setup/restore.

pub mod ansi;
pub mod api;
pub mod app;
pub mod blocks;
pub mod events;
pub mod fullscreen_view;
pub mod hot;
pub mod math;
pub mod outputtest;
pub mod overlays;
pub mod preview;
pub mod prompt;
pub mod settings;
pub mod spinner;
pub mod statusbar;
pub mod view;

use anyhow::{Context as _, Result};
use base64::Engine;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::Value;
use tokio::sync::mpsc::{self, Receiver, UnboundedSender};

use app::{App, RunOptions};
use events::{AppMsg, Effect};

/// Runs the interactive TUI against the atom server.
pub async fn run(opts: RunOptions, hot: bool) -> Result<()> {
    let mut app = App::new(opts);
    let (tx, rx) = mpsc::unbounded_channel::<AppMsg>();
    let hot_path = if hot {
        Some(
            app.hot_state_path
                .clone()
                .unwrap_or_else(hot::default_hot_state_path),
        )
    } else {
        None
    };
    if let Some(path) = &hot_path {
        apply_hot_state(&mut app, path);
        if let Err(error) = hot::load_hot_theme() {
            app.err_msg = error.to_string();
        }
        tokio::spawn(hot::watch_sources(tx.clone()));
    }

    let mut terminal = setup_terminal().context("terminal setup")?;
    let result = event_loop(&mut app, &mut terminal, tx, rx, false, hot).await;
    restore_terminal(&mut terminal);
    result
}

/// --output-test: replay the canned transcript through the real
/// handlers without a server, model, or API key.
pub async fn run_output_test(
    hot_enabled: bool,
    hot_state_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let mut app = outputtest::output_test_app(hot_state_path);
    let (tx, rx) = mpsc::unbounded_channel::<AppMsg>();
    if hot_enabled {
        let path = app
            .hot_state_path
            .clone()
            .unwrap_or_else(hot::default_hot_state_path);
        apply_hot_state(&mut app, &path);
        if let Err(error) = hot::load_hot_theme() {
            app.err_msg = error.to_string();
        }
        tokio::spawn(hot::watch_sources(tx.clone()));
    }
    let mut terminal = setup_terminal().context("terminal setup")?;
    let result = event_loop(&mut app, &mut terminal, tx, rx, true, hot_enabled).await;
    restore_terminal(&mut terminal);
    result
}

// ---------------------------------------------------------------------------
// Terminal lifecycle.
// ---------------------------------------------------------------------------

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Heartbeat interval: bounds the damage of any lost wakeup to one hiccup
/// instead of a permanent freeze. See event_loop.
const HEARTBEAT_TICK_MS: u64 = 250;

fn setup_terminal() -> Result<Term> {
    use crossterm::event::{
        EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    use crossterm::execute;
    use crossterm::terminal::*;

    let mut stdout = std::io::stdout();
    if !hot::inherited_terminal() {
        enable_raw_mode()?;
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange
        )?;
        execute!(stdout, EnableBracketedPaste)?;
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
        }
    }
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Term) {
    use crossterm::event::{DisableBracketedPaste, DisableFocusChange, DisableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::*;
    // A blocking paint task may still be mid-payload when we tear down;
    // serialize with it so the exit sequence cannot interleave.
    let _tty = preview::lock_tty();
    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = terminal.show_cursor();
}

// ---------------------------------------------------------------------------
// The event loop.
// ---------------------------------------------------------------------------

struct LoopState {
    send_rx: Option<Receiver<Value>>,
    sub_rx: Option<Receiver<Value>>,
    sub_sid: String,
    last_title: String,
}

async fn event_loop(
    app: &mut App,
    terminal: &mut Term,
    tx: UnboundedSender<AppMsg>,
    mut rx: mpsc::UnboundedReceiver<AppMsg>,
    test_mode: bool,
    hot_enabled: bool,
) -> Result<()> {
    let mut st = LoopState {
        send_rx: None,
        sub_rx: None,
        sub_sid: String::new(),
        last_title: String::new(),
    };

    // Display math (`$$…$$` in assistant messages): one engine per
    // process, only when the terminal speaks the Kitty graphics
    // protocol. Completion callbacks arrive here as AppMsg::MathWake.
    math::init(tx.clone());

    // Terminal input lives in its own task so the EventStream is only ever
    // polled by one persistent future with a real waker.  Polling it via
    // now_or_never() (no-op waker) or dropping it mid-poll from select!
    // can permanently wedge crossterm's single-slot task protocol.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<crossterm::event::Event>();
    tokio::spawn(async move {
        let mut stream = crossterm::event::EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            if input_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut effects = initial_effects(app, test_mode);

    let mut spinner_tick =
        tokio::time::interval(tokio::time::Duration::from_millis(app::SPINNER_TICK_MS));
    let mut splash_tick =
        tokio::time::interval(tokio::time::Duration::from_millis(app::SPLASH_TICK_MS));
    let mut scene_tick =
        tokio::time::interval(tokio::time::Duration::from_secs(app::TEST_SCENE_TICK_SECS));
    // Safety net against lost wakeups: any future that permanently fails to
    // fire would otherwise freeze the TUI with no way to detect or recover.
    // A 250ms heartbeat bounds the worst case to a brief input hiccup while
    // costing ~4 idle wakes/sec (ratatui's diff emits no bytes for a static
    // frame). The select! re-arms every wakeup source on each heartbeat.
    let mut heartbeat_tick =
        tokio::time::interval(tokio::time::Duration::from_millis(HEARTBEAT_TICK_MS));
    for tick in [
        &mut spinner_tick,
        &mut splash_tick,
        &mut scene_tick,
        &mut heartbeat_tick,
    ] {
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.reset();
    }
    let mut spinner_was_active = false;
    let mut splash_was_active = false;
    let mut scene_was_active = false;
    let splash_start = tokio::time::Instant::now();

    loop {
        run_effects(app, &tx, &mut st, &mut effects).await;
        drain_ready_streams(app, &mut st, &mut effects);
        if !effects.is_empty() {
            run_effects(app, &tx, &mut st, &mut effects).await;
        }

        // Window title follows the session title.
        let title = window_title(app);
        if title != st.last_title {
            st.last_title = title.clone();
            // Locked: SetTitle bytes must not interleave with a concurrent
            // kitty paint payload (same tty, different writers).
            let _guard = preview::lock_tty();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(title));
        }

        let mut cursor_pos: Option<(u16, u16)> = None;
        // Hold the tty write lock across the whole frame — draw, flush and
        // cursor update are one escape sequence burst; a concurrent kitty
        // paint interleaving here is what garbled the TUI until resize.
        let tty_guard = preview::lock_tty();
        // Flush newly rendered formula uploads/placements before the frame
        // that displays their placeholder cells (same guard: these bytes
        // must not interleave with the frame).
        math::flush_terminal_commands();
        terminal.draw(|f| {
            cursor_pos = view::draw(app, f.area(), f.buffer_mut());
        })?;
        match cursor_pos {
            Some((x, y)) => {
                let _ = terminal.show_cursor();
                let _ = terminal.set_cursor_position(ratatui::layout::Position::new(x, y));
            }
            None => {
                let _ = terminal.hide_cursor();
            }
        }
        drop(tty_guard);
        // A first frame can discover the real terminal width before a
        // crossterm Resize event arrives. If that changes diagram geometry,
        // paint the matching kitty placement after the placeholder grid is
        // on screen instead of leaving a stale/blank reserved area.
        if app.preview_dirty && preview::kitty_terminal() {
            effects.push(Effect::PaintPreviews);
            run_effects(app, &tx, &mut st, &mut effects).await;
        }
        if app.quitting {
            return Ok(());
        }

        // Gate animation ticks: only fire when their animation is actually
        // visible. Idle sessions have no live spinner or splash, so the
        // loop blocks on input/events instead of waking ~30x/sec to redraw
        // (Bubble Tea renders on demand; this restores that behavior and
        // drops idle CPU to ~0).
        let spinner_active = app.streaming
            || app.remote_working
            || !app.working_msg.is_empty()
            || app.test_mode
            || (app.manage_visible
                && app.manage_agents.iter().any(|agent| {
                    matches!(
                        agent.status,
                        atom_core::session::store::DelegateStatus::Queued
                            | atom_core::session::store::DelegateStatus::Working
                    )
                }));
        let splash_active = app.splash_visible();
        let scene_active = app.test_mode;
        if spinner_active && !spinner_was_active {
            spinner_tick.reset();
        }
        if splash_active && !splash_was_active {
            splash_tick.reset();
        }
        if scene_active && !scene_was_active {
            scene_tick.reset();
        }
        spinner_was_active = spinner_active;
        splash_was_active = splash_active;
        scene_was_active = scene_active;

        let msg: Option<AppMsg> = tokio::select! {
            m = rx.recv() => m,
            ev = input_rx.recv() => match ev {
                Some(crossterm::event::Event::Key(k)) => Some(AppMsg::Key(k)),
                Some(crossterm::event::Event::Mouse(m)) => Some(AppMsg::Mouse(m)),
                Some(crossterm::event::Event::Resize(w, h)) => Some(AppMsg::Resize(w, h)),
                Some(crossterm::event::Event::Paste(s)) => Some(AppMsg::Paste(s)),
                Some(crossterm::event::Event::FocusGained) => Some(AppMsg::Redraw),
                _ => None,
            },
            v = send_next(&mut st.send_rx) => v,
            v = sub_next(&mut st.sub_rx, &mut st.sub_sid) => v,
            _ = async {
                if spinner_active {
                    spinner_tick.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => Some(AppMsg::TickSpinner),
            _ = async {
                if splash_active {
                    splash_tick.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                Some(AppMsg::TickSplash(splash_start.elapsed().as_secs_f64()))
            }
            _ = async {
                if scene_active {
                    scene_tick.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => Some(AppMsg::TestSceneTick),
            _ = heartbeat_tick.tick() => Some(AppMsg::Heartbeat),
        };

        let Some(msg) = msg else { continue };

        // Process this first message, then drain any other immediately
        // available events before rendering.  This coalesces bursts of
        // scroll / key events into a single frame, making input feel
        // instant even under heavy streaming load.
        if dispatch_msg(app, &mut st, &mut effects, msg, terminal, hot_enabled)? {
            continue;
        }
        if !effects.is_empty() {
            run_effects(app, &tx, &mut st, &mut effects).await;
        }

        // Drain further pending messages (up to a budget per frame).
        //
        // Both the app-internal channel AND the input channel are drained
        // here so input (Esc, Enter) is picked up instantly even during
        // heavy streaming bursts. The input channel is a regular tokio mpsc
        // fed by a dedicated task — try_recv is always safe (no waker
        // involvement, unlike the raw EventStream).
        const MAX_DRAIN: usize = 128;
        for _ in 0..MAX_DRAIN {
            let next = rx.try_recv().ok().or_else(|| {
                input_rx.try_recv().ok().and_then(|ev| match ev {
                    crossterm::event::Event::Key(k) => Some(AppMsg::Key(k)),
                    crossterm::event::Event::Mouse(m) => Some(AppMsg::Mouse(m)),
                    crossterm::event::Event::Resize(w, h) => Some(AppMsg::Resize(w, h)),
                    crossterm::event::Event::Paste(s) => Some(AppMsg::Paste(s)),
                    crossterm::event::Event::FocusGained => Some(AppMsg::Redraw),
                    _ => None,
                })
            });
            let Some(extra) = next else { break };
            dispatch_msg(app, &mut st, &mut effects, extra, terminal, hot_enabled)?;
        }
    }
}

/// Dispatch a single AppMsg.  Returns Ok(true) when the caller should
/// `continue` (skip rendering this iteration - e.g. Redraw cleared the
/// terminal and the next loop pass will repaint).
fn dispatch_msg(
    app: &mut App,
    st: &mut LoopState,
    effects: &mut Vec<Effect>,
    msg: AppMsg,
    terminal: &mut Term,
    hot_enabled: bool,
) -> Result<bool> {
    // Heartbeat exists purely to re-arm the select! wakeup sources; no
    // state change, no redraw request.
    if matches!(msg, AppMsg::Heartbeat) {
        return Ok(false);
    }
    if matches!(msg, AppMsg::Redraw) {
        terminal.clear()?;
        return Ok(true);
    }
    if matches!(msg, AppMsg::MathWake) {
        // A display formula finished rendering in the background. Mark the
        // viewport dirty so stale LaTeX fallbacks re-render into placeholder
        // rows on the next frame; no full-screen clear needed (ratatui diffs
        // the changed rows).
        app.viewport_dirty = true;
        return Ok(false);
    }

    // --hot rebuild finished: save state + exec the new binary.
    if let AppMsg::HotRebuilt(result) = msg {
        match result {
            Ok(build) => {
                if hot_enabled {
                    let path = app
                        .hot_state_path
                        .clone()
                        .unwrap_or_else(hot::default_hot_state_path);
                    if let Err(e) = hot_handoff(app, &build, &path) {
                        app.err_msg = e.to_string();
                        app.refresh_viewport();
                    } else {
                        return Ok(true); // unreachable: exec replaced us
                    }
                }
            }
            Err(build_err) => {
                app.err_msg = build_err;
                app.refresh_viewport();
            }
        }
        return Ok(false);
    }
    if let AppMsg::ThemeReloaded(result) = msg {
        match result {
            Ok(elapsed) => {
                if app.err_msg.starts_with("theme:") {
                    app.err_msg.clear();
                }
                app.invalidate_all_blocks();
                app.preview_dirty = true;
                app.refresh_viewport();
                app.copied_msg = format!("theme reloaded in {} ms", elapsed.as_millis());
                app.copied_at = Some(std::time::Instant::now());
                effects.push(Effect::PaintPreviews);
                let _guard = preview::lock_tty();
                terminal.clear()?;
            }
            Err(error) => {
                app.err_msg = error;
                app.refresh_viewport();
            }
        }
        return Ok(false);
    }
    if let AppMsg::SubscribeNow(id) = msg {
        effects.push(Effect::Subscribe { id });
        return Ok(false);
    }
    if let AppMsg::SendReady { sid, rx } = msg {
        if sid != app.session.id {
            return Ok(false);
        }
        st.send_rx = Some(rx);
        effects.extend(app.handle_msg(AppMsg::SendStarted { sid }));
        return Ok(false);
    }
    if let AppMsg::SubReady { sid, rx } = msg {
        if sid != app.session.id {
            return Ok(false);
        }
        st.sub_rx = Some(rx);
        st.sub_sid = sid.clone();
        effects.extend(app.handle_msg(AppMsg::SubStarted { sid }));
        return Ok(false);
    }

    effects.extend(app.handle_msg(msg));
    Ok(false)
}

fn initial_effects(app: &App, test_mode: bool) -> Vec<Effect> {
    if test_mode {
        return Vec::new();
    }
    let mut effects = Vec::new();
    if matches!(app.overlay, Some(overlays::OverlayKind::Providers)) {
        effects.push(Effect::EnsureCatalog);
    }
    match app.overlay {
        Some(overlays::OverlayKind::Model) => effects.push(Effect::FetchModels),
        Some(overlays::OverlayKind::Session) => effects.push(Effect::FetchSessions),
        Some(overlays::OverlayKind::Stats) => effects.push(Effect::FetchStats {
            days: app.stats_days,
        }),
        _ => {}
    }
    if app.manage_visible && !app.session.id.is_empty() {
        effects.push(Effect::ListChildren {
            id: app.session.id.clone(),
        });
    }
    if app.context_visible && !app.session.id.is_empty() {
        effects.push(Effect::FetchContext {
            id: app.session.id.clone(),
        });
    }
    if !app.session.id.is_empty() {
        effects.push(Effect::LoadSession {
            id: app.session.id.clone(),
        });
    }
    effects
}

/// Applies already-buffered stream events before drawing. This bounds a
/// burst of provider deltas to one markdown/layout pass and one frame while
/// leaving terminal input in the main select loop responsive.
fn drain_ready_streams(app: &mut App, st: &mut LoopState, effects: &mut Vec<Effect>) {
    const MAX_DRAIN_PER_STREAM: usize = 64;

    for _ in 0..MAX_DRAIN_PER_STREAM {
        let next = match st.send_rx.as_mut().map(Receiver::try_recv) {
            Some(Ok(value)) => Some(AppMsg::SendEvent(value)),
            Some(Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)) => {
                st.send_rx = None;
                Some(AppMsg::SendClosed)
            }
            _ => None,
        };
        let Some(msg) = next else { break };
        effects.extend(app.handle_msg(msg));
    }

    for _ in 0..MAX_DRAIN_PER_STREAM {
        let next = match st.sub_rx.as_mut().map(Receiver::try_recv) {
            Some(Ok(value)) => Some(AppMsg::SubEvent(value)),
            Some(Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)) => {
                st.sub_rx = None;
                Some(AppMsg::SubEnded {
                    sid: std::mem::take(&mut st.sub_sid),
                })
            }
            _ => None,
        };
        let Some(msg) = next else { break };
        effects.extend(app.handle_msg(msg));
    }
}

fn window_title(app: &App) -> String {
    let t = app.session.title.trim();
    if t.is_empty() {
        "atom".to_string()
    } else {
        t.to_string()
    }
}

/// Drains one NDJSON value from the active /send stream; emits SendClosed
/// and clears the slot when the channel closes.
async fn send_next(rx: &mut Option<Receiver<Value>>) -> Option<AppMsg> {
    match rx.as_mut() {
        None => std::future::pending().await,
        Some(r) => match r.recv().await {
            Some(v) => Some(AppMsg::SendEvent(v)),
            None => {
                *rx = None;
                Some(AppMsg::SendClosed)
            }
        },
    }
}

/// Same for the /events subscription channel.
async fn sub_next(rx: &mut Option<Receiver<Value>>, sid: &mut String) -> Option<AppMsg> {
    match rx.as_mut() {
        None => std::future::pending().await,
        Some(r) => match r.recv().await {
            Some(v) => Some(AppMsg::SubEvent(v)),
            None => {
                let ended = AppMsg::SubEnded {
                    sid: std::mem::take(sid),
                };
                *rx = None;
                Some(ended)
            }
        },
    }
}

/// Executes queued effects. Network effects spawn tasks that feed
/// results back through `tx`; stream-opening effects run inline so the
/// loop can own their receivers.
async fn run_effects(
    app: &mut App,
    tx: &UnboundedSender<AppMsg>,
    st: &mut LoopState,
    pending: &mut Vec<Effect>,
) {
    for effect in std::mem::take(pending) {
        match effect {
            Effect::Quit => app.quitting = true,

            Effect::Subscribe { id } => {
                if st.sub_sid == id && st.sub_rx.is_some() {
                    continue; // already subscribed to this session
                }
                // Drop any prior subscription before dialing a new one.
                st.sub_rx = None;
                st.sub_sid.clear();
                let tx = tx.clone();
                tokio::spawn(async move {
                    match api::stream_events(&id).await {
                        Ok(rx) => {
                            let _ = tx.send(AppMsg::SubReady { sid: id, rx });
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                            let _ = tx.send(AppMsg::SubscribeNow(id));
                        }
                    }
                });
            }

            Effect::SubscribeAfter { id, delay_ms } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    let _ = tx.send(AppMsg::SubscribeNow(id));
                });
            }

            Effect::SendTurn(req) => {
                let body = req.to_body();
                let session_id = req.session_id.clone();
                let tx = tx.clone();
                // Dial off the event loop so input/render/Esc stay live while
                // the server warms up the stream (provider TTFB can be seconds).
                tokio::spawn(async move {
                    let mut resp = api::stream_send_healed(&req).await;
                    // The server may have shut down; restart and retry once.
                    if resp.is_err()
                        && !api::is_running().await
                        && api::ensure_server().await.is_ok()
                    {
                        resp = atom_server::client::stream_send(&session_id, &body).await;
                    }
                    match resp {
                        Ok(rx) => {
                            let _ = tx.send(AppMsg::SendReady {
                                sid: session_id,
                                rx,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::SendEvent(
                                serde_json::json!({"type":"error","message": e.to_string()}),
                            ));
                            let _ = tx.send(AppMsg::SendClosed);
                        }
                    }
                });
            }

            Effect::EnsureCatalog => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
                    let _ = tx.send(AppMsg::ModelsDevReady);
                });
            }
            Effect::FetchModels => {
                let providers = app.providers.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let pairs = tokio::time::timeout(
                        tokio::time::Duration::from_secs(7),
                        api::fetch_all_models(&providers),
                    )
                    .await
                    .unwrap_or_default();
                    let entries: Vec<atom_core::providers::providers::ModelEntry> = pairs
                        .into_iter()
                        .map(
                            |(provider, model)| atom_core::providers::providers::ModelEntry {
                                provider,
                                model,
                            },
                        )
                        .collect();
                    let _ = tx.send(AppMsg::ModelsLoaded(entries));
                });
            }
            Effect::FetchSessions => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    match api::list_sessions().await {
                        Ok(mut sessions) => {
                            sessions.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
                            let _ = tx.send(AppMsg::SessionsLoaded(sessions));
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                        }
                    }
                });
            }
            Effect::FetchStats { days } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(AppMsg::StatsLoaded(
                        api::fetch_stats_report(days)
                            .await
                            .map(Box::new)
                            .map_err(|e| e.to_string()),
                    ));
                });
            }
            Effect::LoadSession { id } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    match api::get_session(&id).await {
                        Ok(sess) => {
                            let _ = tx.send(AppMsg::SessionLoaded(Box::new(sess)));
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                        }
                    }
                });
            }
            Effect::ListChildren { id } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let agents = api::list_children(&id).await.unwrap_or_default();
                    let _ = tx.send(AppMsg::ChildrenLoaded { id, agents });
                });
            }
            Effect::FetchContext { id } => {
                let cwd = app.cwd.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let tools = atom_tools::tool_definitions();
                    let rows = match api::get_session(&id).await {
                        Ok(sess) => {
                            atom_core::session::context_breakdown::context_breakdown(&sess, &tools)
                        }
                        Err(_) => {
                            let sess = atom_core::session::store::Session {
                                cwd,
                                ..Default::default()
                            };
                            atom_core::session::context_breakdown::context_breakdown(&sess, &tools)
                        }
                    };
                    let _ = tx.send(AppMsg::ContextLoaded(rows));
                });
            }
            Effect::LoadForkSource { id } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    match api::get_session(&id).await {
                        Ok(sess) => {
                            let _ = tx.send(AppMsg::ForkSourceLoaded {
                                id,
                                sess: Box::new(sess),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                        }
                    }
                });
            }
            Effect::ForkSession {
                source_id,
                position,
            } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    match api::fork_session(&source_id, position).await {
                        Ok(forked) => {
                            let _ = tx.send(AppMsg::ForkedSession {
                                info: Box::new(forked.info),
                                draft: forked.draft,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                        }
                    }
                });
            }
            Effect::CreateSession {
                provider,
                model,
                cwd,
                thinking,
            } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = api::ensure_server().await;
                    match api::create_session(&provider, &model, &cwd, &thinking).await {
                        Ok(info) => {
                            let _ = tx.send(AppMsg::CreatedSession(Box::new(info)));
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::Errored(e.to_string()));
                        }
                    }
                });
            }
            Effect::PatchSessionModel {
                provider,
                model,
                thinking,
            } => {
                let id = app.session.id.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        api::patch_session_model(&id, &provider, &model, &thinking).await
                    {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::PatchSessionThinking => {
                let id = app.session.id.clone();
                let thinking = app.session.thinking.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::patch_session_thinking(&id, &thinking).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::DeleteSession { id } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::delete_session(&id).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::PauseTurn => {
                let id = app.session.id.clone();
                let turn_id = app.turn_id.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::pause_turn(&id, &turn_id).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::InterruptTurn { pause_turn_id, req } => {
                // Mid-stream submit: pause the running turn via the
                // server first, then dial the interruption stream. The
                // message rides in the request; nothing is stored.
                let id = req.session_id.clone();
                let body = req.to_body();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::pause_turn(&id, &pause_turn_id).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                    // The pause targeted the turn that was streaming, but the
                    // server may have another registration behind it (raced
                    // pause, stale entry): heal the same way a plain send
                    // would instead of surfacing a 409.
                    let mut resp = api::stream_send_healed(&req).await;
                    // The server may have shut down; restart and retry once.
                    if resp.is_err()
                        && !api::is_running().await
                        && api::ensure_server().await.is_ok()
                    {
                        resp = atom_server::client::stream_send(&id, &body).await;
                    }
                    match resp {
                        Ok(rx) => {
                            let _ = tx.send(AppMsg::SendReady { sid: id, rx });
                        }
                        Err(e) => {
                            let _ = tx.send(AppMsg::SendEvent(
                                serde_json::json!({"type":"error","message": e.to_string()}),
                            ));
                            let _ = tx.send(AppMsg::SendClosed);
                        }
                    }
                });
            }
            Effect::Compact { instructions } => {
                let id = app.session.id.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(AppMsg::CompactDone(
                        api::compact(&id, &instructions)
                            .await
                            .map_err(|e| e.to_string()),
                    ));
                });
            }
            Effect::RespondApproval { sid, id, decision } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::respond_approval(&sid, &id, &decision).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::StartOpenAIOAuth => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5 * 60),
                        atom_core::providers::oauth::run_openai_oauth(),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("OAuth sign-in timed out")))
                    .map_err(|e| e.to_string());
                    let _ = tx.send(AppMsg::OAuthDone(result));
                });
            }
            Effect::StartMcpOAuth {
                server,
                url,
                client_id,
                client_secret,
                token_endpoint_auth_method,
            } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    // bearer_token with interactive=true runs the full
                    // PKCE flow when no usable cached token is present,
                    // persists the resulting tokens, and returns them.
                    // On success the auth store is already updated —
                    // the App only needs to refresh the picker / catalog.
                    let result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(3 * 60),
                        atom_tools::mcp_oauth::bearer_token(
                            &server,
                            &url,
                            &client_id,
                            &client_secret,
                            token_endpoint_auth_method.as_deref(),
                            true,
                        ),
                    )
                    .await
                    .unwrap_or_else(|_| Err("MCP OAuth sign-in timed out".into()))
                    .and_then(|tok| {
                        if tok.is_none() {
                            Err("OAuth flow returned no token".into())
                        } else {
                            Ok(())
                        }
                    });
                    let _ = tx.send(AppMsg::McpOAuthDone { server, result });
                });
            }
            Effect::ReloadProviders => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
                    let providers = atom_core::providers::providers::build_providers().await;
                    let _ = tx.send(AppMsg::ProvidersRebuilt(providers));
                });
            }
            Effect::ReadClipboard => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    // Match Go's readClipboard (clipboard.go): image is
                    // preferred over text. The platform-specific image
                    // probe lives in atom_tools::clipboard (pngpaste /
                    // osascript on macOS, wl-paste on Linux, etc.); the
                    // text-only fallback below is only used when no
                    // image is present.
                    let c = atom_tools::clipboard::read_clipboard().await;
                    if let Some(data) = c.data {
                        let _ = tx.send(AppMsg::ClipboardImage { name: c.name, data });
                    } else if !c.text.is_empty() {
                        let _ = tx.send(AppMsg::ClipboardText(c.text));
                    }
                });
            }
            Effect::CopyToClipboard { text } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = write_clipboard_text(&text).await {
                        let _ = tx.send(AppMsg::Errored(format!("copy failed: {e}")));
                    }
                });
            }
            Effect::OpenLink { uri } => {
                atom_core::util::open_url(&uri);
            }
            Effect::RunShell { cmd, cwd } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (output, code, new_cwd) = run_user_shell(&cmd, &cwd, &tx).await;
                    let _ = tx.send(AppMsg::ShellDone {
                        cmd,
                        cwd,
                        output,
                        code,
                        new_cwd,
                    });
                });
            }
            Effect::PatchSessionCwd { id, cwd } => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::patch_session_cwd(&id, &cwd).await {
                        let _ = tx.send(AppMsg::Errored(e.to_string()));
                    }
                });
            }
            Effect::PaintPreviews => {
                if !preview::kitty_terminal() {
                    continue;
                }
                // Diagram ids and geometry are (re)derived here so freshly
                // attached diagrams still paint even if a render pass has
                // not filled them in yet. When either changes, the
                // placeholder grid on screen (cached block lines) is stale:
                // invalidate it and defer the kitty paint to the post-draw
                // check, which fires after the frame shows the new grid.
                // Painting immediately would transmit placements whose
                // c/r dims match nothing on screen — tiles then stretch
                // over unrelated content until a resize.
                let inner = app.inner_width().saturating_sub(2).max(1);
                let mut changed = crate::blocks::assign_block_diagram_ids(&mut app.blocks);
                for block in app.blocks.iter_mut() {
                    if let Some(d) = block.diagram.as_mut() {
                        if crate::blocks::diagram_geometry(d, inner) {
                            changed = true;
                        }
                    }
                }
                if changed {
                    for block in app.blocks.iter_mut() {
                        if block.diagram.is_some() {
                            block.lines = None;
                        }
                    }
                    app.viewport_dirty = true;
                    // The post-draw check only fires when this flag is set;
                    // make the deferred repaint unconditional.
                    app.preview_dirty = true;
                    continue;
                }
                if !app.preview_dirty {
                    continue;
                }
                app.preview_dirty = false;
                let mut entries: Vec<(usize, Vec<u8>)> = Vec::new();
                for p in app.pending.iter().filter(|p| p.cols > 0) {
                    if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&p.img.data)
                    {
                        entries.push((p.num, data));
                    }
                }
                for block in app.blocks.iter() {
                    if block.kind != crate::blocks::BlockKind::User {
                        continue;
                    }
                    for p in block.images.iter().filter(|p| p.cols > 0 && p.num > 0) {
                        if entries.iter().any(|(n, _)| *n == p.num) {
                            continue;
                        }
                        if let Ok(data) =
                            base64::engine::general_purpose::STANDARD.decode(&p.img.data)
                        {
                            entries.push((p.num, data));
                        }
                    }
                }
                // Diagrams: current placements plus every diagram id no
                // longer referenced, which gets deleted on the tty.
                let mut diagram_specs: Vec<preview::DiagramSpec> = Vec::new();
                let mut diagram_ids: Vec<usize> = Vec::new();
                for block in app.blocks.iter() {
                    if let Some(d) = &block.diagram {
                        if d.id > 0 && d.cols > 0 && d.rows > 0 {
                            diagram_ids.push(d.id);
                            diagram_specs.push(preview::DiagramSpec {
                                id: d.id,
                                svg: d.svg.clone(),
                                png: d.png.clone(),
                                cols: d.cols,
                                rows: d.rows,
                            });
                        }
                    }
                }
                let stale: Vec<usize> = (crate::blocks::MIN_KITTY_DIAGRAM_ID
                    ..=crate::blocks::MAX_KITTY_DIAGRAM_ID)
                    .filter(|id| !diagram_ids.contains(id))
                    .collect();
                tokio::task::spawn_blocking(move || {
                    preview::paint_kitty_diagrams(&diagram_specs, &stale);
                    preview::paint_kitty_previews(&entries);
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hot-reload handoff.
// ---------------------------------------------------------------------------

fn apply_hot_state(app: &mut App, path: &std::path::Path) {
    let Some(state) = hot::load_state(path) else {
        return;
    };
    if !state.session_id.is_empty() && state.session_id != app.session.id {
        return; // stale state from another session
    }
    app.input.set_value(&state.input);
    if let Some(cursor) = state.input_cursor {
        let mut cursor = cursor.min(app.input.value.len());
        while cursor > 0 && !app.input.value.is_char_boundary(cursor) {
            cursor -= 1;
        }
        app.input.cursor = cursor;
        app.input.sel = state.input_selection;
        app.input.scroll_y = state.input_scroll_y;
    } else {
        // Compatibility with state written by the original hot loop.
        for _ in 0..state.cursor_line {
            app.input.down();
        }
        for _ in 0..state.cursor_col {
            app.input.right();
        }
    }
    app.show_reasoning = state.show_reasoning;
    app.following = state.following;
    app.scroll_y = state.scroll_y;
    if !state.thinking_pref.is_empty() {
        app.apply_thinking(&state.thinking_pref);
    }
    app.overlay = state.overlay;
    app.overlay_q = state.overlay_q;
    app.overlay_q_sel = state.overlay_q_sel;
    app.overlay_sel = state.overlay_sel;
    app.overlay_scroll = state.overlay_scroll;
    app.menu_visible = state.menu_visible;
    app.menu_sel = state.menu_sel;
    app.menu_virtual = state.menu_virtual;
    app.manage_visible = state.manage_visible;
    app.manage_sel = state.manage_sel;
    if state.picker_kind != overlays::PickerKind::None {
        app.open_picker(state.picker_kind);
        app.picker_sel = state
            .picker_sel
            .min(app.picker_items.len().saturating_sub(1));
    }
    app.context_visible = state.context_visible;
    app.context_sel = state.context_sel;
    app.reasoning_visible = state.reasoning_visible;
    app.reasoning_sel = state.reasoning_sel;
    if let Some(ms) = state.reload_ms {
        app.copied_msg = format!("reloaded in {ms} ms");
        app.copied_at = Some(std::time::Instant::now());
    }
}

fn hot_handoff(app: &mut App, build: &hot::HotBuild, path: &std::path::Path) -> Result<()> {
    let (line, col) = app.input.line_col_cells();
    let state = hot::HotState {
        session_id: app.session.id.clone(),
        scroll_y: app.scroll_y,
        following: app.following,
        input: app.input.value.clone(),
        cursor_line: line,
        cursor_col: col,
        input_cursor: Some(app.input.cursor),
        input_selection: app.input.sel,
        input_scroll_y: app.input.scroll_y,
        show_reasoning: app.show_reasoning,
        thinking_pref: app.thinking_pref.clone(),
        overlay: app.overlay,
        overlay_q: app.overlay_q.clone(),
        overlay_q_sel: app.overlay_q_sel,
        overlay_sel: app.overlay_sel,
        overlay_scroll: app.overlay_scroll,
        menu_visible: app.menu_visible,
        menu_sel: app.menu_sel,
        menu_virtual: app.menu_virtual,
        manage_visible: app.manage_visible,
        manage_sel: app.manage_sel,
        picker_kind: app.picker_kind,
        picker_sel: app.picker_sel,
        context_visible: app.context_visible,
        context_sel: app.context_sel,
        reasoning_visible: app.reasoning_visible,
        reasoning_sel: app.reasoning_sel,
        reload_ms: Some(build.elapsed.as_millis().min(u64::MAX as u128) as u64),
    };
    hot::write_state(path, &state)?;
    hot::restart_self(&build.executable, &app.session.id, path)
}

// ---------------------------------------------------------------------------
// Shell mode: run a user-typed command, reporting $PWD afterwards so a
// `cd` moves the app (and the session) with the shell.
// ---------------------------------------------------------------------------

/// The user's login shell when it can host the POSIX wrapper (zsh, bash,
/// sh…); fish gets /bin/sh because the marker script isn't fish syntax.
fn user_shell() -> String {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let base = shell.rsplit('/').next().unwrap_or_default();
    if shell.is_empty() || base == "fish" {
        "/bin/sh".to_string()
    } else {
        shell
    }
}

/// Marker the wrapper prints to stderr after the command runs: the shell's
/// final $PWD, which shell mode uses to follow `cd`.
const SHELL_PWD_MARKER: &str = "__ATOM_PWD__";

/// Runs `cmd` from `cwd` in the user's shell. Returns (output, exit code,
/// new $PWD). The code is None when the command was killed via the armed
/// kill switch (Ctrl+C in shell mode).
async fn run_user_shell(
    cmd: &str,
    cwd: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<AppMsg>,
) -> (String, Option<i32>, String) {
    let mut command = tokio::process::Command::new(user_shell());
    // eval keeps the user's quoting intact while letting the wrapper print
    // its marker afterwards, even when the command `cd`s mid-way.
    command
        .arg("-c")
        .arg(format!(
            "eval \"$ATOM_SHELL_CMD\"; __atom_ec=$?; printf '\\n{SHELL_PWD_MARKER}%s' \"$PWD\" >&2; exit $__atom_ec"
        ))
        .env("ATOM_SHELL_CMD", cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return (format!("error: {e}"), Some(127), String::new()),
    };
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    // The App stores kill_tx so Ctrl+C can abort the command while it runs.
    let _ = tx.send(AppMsg::ShellKillArmed(kill_tx));

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let (mut out, mut err) = (String::new(), String::new());
    let status = tokio::select! {
        _ = async {
            if let Some(s) = stdout.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_string(s, &mut out).await;
            }
            if let Some(s) = stderr.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_string(s, &mut err).await;
            }
        } => {
            // Both pipes closed; reap the exit status.
            child.wait().await.ok().and_then(|st| st.code())
        }
        _ = &mut kill_rx => {
            let _ = child.start_kill();
            // Reap so the child doesn't linger as a zombie.
            let _ = child.wait().await;
            None
        }
    };

    // Strip the trailing $PWD marker from the displayed stderr.
    let new_cwd = match err.rfind(SHELL_PWD_MARKER) {
        Some(i) => {
            let pwd = err[i + SHELL_PWD_MARKER.len()..].trim().to_string();
            err.truncate(i);
            pwd
        }
        None => String::new(),
    };

    let mut output = out;
    if !err.trim().is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&err);
    }
    (output, status, new_cwd)
}

// ---------------------------------------------------------------------------
// OS clipboard writes (text). Reads go through atom_tools::clipboard,
// which prefers an image over text — matching Go's clipboard.go.
// ---------------------------------------------------------------------------

async fn write_clipboard_text(text: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    #[cfg(target_os = "macos")]
    let (prog, args): (&str, &[&str]) = ("pbcopy", &[]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (prog, args): (&str, &[&str]) = ("wl-copy", &[]);
    #[cfg(not(unix))]
    let (prog, args): (&str, &[&str]) = ("cat", &[]);
    let mut child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await.ok();
        stdin.flush().await.ok();
    }
    drop(child.stdin.take());
    child.wait().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_loads_catalog_for_provider_overlay() {
        let mut app = App::new_test(80, 24);
        app.overlay = Some(overlays::OverlayKind::Providers);

        let effects = initial_effects(&app, false);

        assert!(matches!(effects.as_slice(), [Effect::EnsureCatalog]));
    }

    #[test]
    fn startup_skips_catalog_without_provider_overlay() {
        let app = App::new_test(80, 24);

        assert!(initial_effects(&app, false).is_empty());
    }
}
