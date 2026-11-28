//! terminal setup, panic hook, event loop, cli dispatch. the panic hook is
//! not decoration: without it a panic leaves the terminal in raw mode and you
//! get to enjoy a shell where every newline is a carriage return.

mod anim;
mod app;
mod cli;
mod config;
mod doctor;
mod export;
mod fetch;
mod model;
mod theme;
mod ui;
mod util;
mod worker;

use app::{App, Focus, Overlay, Status, ToastKind};
use cli::Args;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use worker::{Event as WorkerEvent, EventKind as WorkerEventKind};

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        // if any step here fails, the ones before it still need undoing. raw
        // mode left behind on a failed start is a nasty thing to debug from a
        // shell where every newline forgets to return the cursor.
        if let Err(e) = execute!(
            out,
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        ) {
            let _ = execute!(out, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self { active: true })
    }

    fn restore(&mut self) {
        if self.active {
            let mut steps = terminal_cleanup();
            let _ = restore_terminal_steps(&mut steps);
            self.active = false;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

type CleanupStep = (&'static str, Box<dyn FnMut() -> io::Result<()>>);

/// run each terminal cleanup independently. a failure in one must never skip
/// the rest, and the caller gets to see which ones failed.
fn restore_terminal_steps(steps: &mut [CleanupStep]) -> Vec<String> {
    let mut failed = Vec::new();
    for (name, step) in steps.iter_mut() {
        if step().is_err() {
            failed.push((*name).to_string());
        }
    }
    failed
}

fn terminal_cleanup() -> Vec<CleanupStep> {
    vec![
        (
            "bracketed paste",
            Box::new(|| {
                let mut out = io::stdout();
                execute!(out, crossterm::event::DisableBracketedPaste)
            }),
        ),
        (
            "alternate screen",
            Box::new(|| {
                let mut out = io::stdout();
                execute!(out, LeaveAlternateScreen)
            }),
        ),
        ("raw mode", Box::new(terminal::disable_raw_mode)),
    ]
}

fn cli_palette(args: &cli::Args) -> theme::Palette {
    let kind = args
        .theme
        .as_deref()
        .map(theme::ThemeKind::parse)
        .unwrap_or(theme::ThemeKind::CatppuccinMocha);
    theme::palette(kind, theme::detect_depth())
}

fn cli_color() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && theme::detect_depth() != theme::ColorDepth::Monochrome
}

fn cli_error(label: &str, message: &str, color: bool) {
    use std::io::Write;
    let p = cli_palette(&cli::Args::default());
    let mut err = std::io::stderr();
    if color {
        let c = theme::to_crossterm(p.error);
        let _ = crossterm::execute!(
            err,
            crossterm::style::SetForegroundColor(c),
            crossterm::style::SetAttribute(crossterm::style::Attribute::Bold)
        );
    }
    let _ = write!(err, "{label}");
    if color {
        let _ = crossterm::execute!(
            err,
            crossterm::style::ResetColor,
            crossterm::style::SetAttribute(crossterm::style::Attribute::Reset)
        );
    }
    let _ = writeln!(err, " {message}");
    let _ = err.flush();
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            let color = cli_color();
            cli_error("error:", &e, color);
            cli_error("hint:", cli::hint(), color);
            std::process::exit(2);
        }
    };

    // read saved preferences before help so --help can wear the configured theme.
    config::apply(&mut args);
    let color = cli_color();

    if args.help {
        let p = cli_palette(&args);
        let mut out = std::io::stdout();
        cli::render_help(&mut out, &p, color);
        return;
    }
    if args.version {
        println!("scribe {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.doctor {
        let code = doctor::run(args.model.as_ref().and_then(|p| p.to_str()));
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }

    if args.no_tui {
        match run_plain(&args) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                cli_error("error:", &e, color);
                std::process::exit(1);
            }
        }
        return;
    }

    install_panic_hook();
    if let Err(e) = run_tui(&args) {
        cli_error("error:", &e, color);
        std::process::exit(1);
    }
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = execute!(out, crossterm::event::DisableBracketedPaste);
        let _ = execute!(out, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        original(info);
    }));
}

fn collect_plain_segments(
    events: impl IntoIterator<Item = WorkerEventKind>,
) -> Result<Vec<export::Segment>, String> {
    let mut segments = Vec::new();
    for event in events {
        match event {
            WorkerEventKind::Segment(segment) => segments.push(segment),
            WorkerEventKind::Done => return Ok(segments),
            WorkerEventKind::Cancelled => return Err("transcription cancelled".into()),
            WorkerEventKind::CancelledWithWarning(message) => {
                return Err(format!("transcription cancelled; {message}"));
            }
            WorkerEventKind::Error(error) => return Err(error),
            _ => {}
        }
    }
    Err("transcription ended before the worker reported completion".into())
}

fn run_plain(args: &Args) -> Result<String, String> {
    let source = args.input.as_ref().ok_or("give me a file path or a url")?;
    let need = util::is_url(source);
    worker::check_tools(need)?;
    let (_, path) = model::resolve(args.model.as_deref().and_then(|p| p.to_str()))?;
    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(2)
            .clamp(1, 8)
    });
    let job = worker::Job {
        source: source.clone(),
        model: path,
        threads,
        lang: args.lang.clone(),
        translate: args.translate.unwrap_or(false),
    };
    let (tx, rx) = mpsc::channel();
    let handle = worker::spawn(job, tx, worker::next_job_id());
    let result = collect_plain_segments(rx.into_iter().map(|event| event.kind));
    // the worker may still be unwinding after its last event. stop joins it.
    handle.stop();
    let segments = result?;
    let output = export::export(args.format, &segments, Some(source));
    if let Some(dir) = &args.out {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = util::output_path(source, Some(dir), args.format.extension());
        export::write_exclusive(&path, output.as_bytes())?;
        Ok(String::new())
    } else {
        Ok(output)
    }
}

enum DownloadOutcome {
    Ready,
    Paused,
    Failed(String),
}

fn run_tui(args: &Args) -> Result<(), String> {
    let mut guard =
        TerminalGuard::enter().map_err(|e| format!("could not start the terminal ui: {e}"))?;
    let mut stdout = io::stdout();
    let backend = CrosstermBackend::new(&mut stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("could not create the terminal: {e}"))?;
    terminal
        .clear()
        .map_err(|e| format!("could not clear the terminal: {e}"))?;

    let depth = theme::detect_depth();
    let mut app = App::new(args);

    let (label, path) = match model::resolve(args.model.as_deref().and_then(|p| p.to_str())) {
        Ok((l, p)) => (l, p),
        Err(e) => {
            app.set_message(e);
            (String::new(), std::path::PathBuf::new())
        }
    };
    if !label.is_empty() {
        app.set_model(label, path.to_string_lossy().to_string());
    }
    let mut saved_preferences = config::Preferences::from_app(&app);

    let (tx, rx): (mpsc::Sender<WorkerEvent>, mpsc::Receiver<WorkerEvent>) = mpsc::channel();
    let mut handle: Option<worker::Handle> = None;
    // cancelled workers keep running until they notice the flag. park their
    // handles here and reap them in the loop instead of freezing the ui.
    let mut retiring: Vec<worker::Handle> = Vec::new();
    let mut downloader: Option<std::thread::JoinHandle<DownloadOutcome>> = None;
    let mut download_rx: Option<Receiver<model::DownloadProgress>> = None;
    let mut download_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;

    let mut last_frame = Instant::now();
    let mut needs_redraw = true;

    loop {
        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f32();
        app.tick(dt.min(0.1));

        if let Some(drx) = &download_rx {
            while let Ok(p) = drx.try_recv() {
                needs_redraw = true;
                app.download_frac = p.fraction();
                if p.bytes_per_sec > 0.0 {
                    app.download_info = format!(
                        "{}/s eta {}",
                        human_bytes(p.bytes_per_sec as u64),
                        p.eta_s()
                            .map(duration_short)
                            .unwrap_or_else(|| "--:--".into())
                    );
                }
            }
        }
        if downloader.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = downloader.take() {
                let outcome = h.join().unwrap_or_else(|_| {
                    DownloadOutcome::Failed("download worker stopped unexpectedly".into())
                });
                finish_model_download(&mut app, outcome);
            }
            download_rx = None;
            download_cancel = None;
            needs_redraw = true;
        }

        while let Ok(ev) = rx.try_recv() {
            // stragglers from a cancelled or replaced job have no business
            // touching the current transcript. their handle is already joined.
            if ev.job_id != app.job_id {
                continue;
            }
            let terminal = matches!(
                ev.kind,
                WorkerEventKind::Done
                    | WorkerEventKind::Cancelled
                    | WorkerEventKind::CancelledWithWarning(_)
                    | WorkerEventKind::Error(_)
            );
            needs_redraw = true;
            app.accept_event(ev);
            if terminal && let Some(mut h) = handle.take() {
                h.join_finished();
            }
        }
        let mut still_running = Vec::new();
        for mut h in retiring.drain(..) {
            h.join_finished();
            if !h.thread_finished() {
                still_running.push(h);
            }
        }
        retiring = still_running;

        if event::poll(Duration::from_millis(30)).unwrap_or(false)
            && let Ok(ev) = event::read()
        {
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let picker_rows = ui::picker_visible_rows(
                        terminal.size().map(|s| s.height).unwrap_or(24),
                        app.download == app::DownloadState::Downloading,
                    );
                    process_key(
                        key,
                        &mut app,
                        &tx,
                        &mut handle,
                        &mut retiring,
                        &mut downloader,
                        &mut download_rx,
                        &mut download_cancel,
                        picker_rows,
                    );
                    needs_redraw = true;
                }
                Event::Paste(pasted)
                    if app.focus == Focus::Input && app.overlay == Overlay::None =>
                {
                    app.input.push_str(&pasted.replace(['\r', '\n'], ""));
                    app.completion.dismissed = false;
                    app.update_completion();
                    needs_redraw = true;
                }
                _ => {}
            }
        }

        let preferences = config::Preferences::from_app(&app);
        if preferences != saved_preferences {
            if let Err(e) = preferences.save() {
                app.set_message(format!("could not save preferences: {e}"));
            }
            saved_preferences = preferences;
        }

        let meter_visible = terminal
            .size()
            .is_ok_and(|size| size.width >= 70 && size.height >= 14)
            && app.overlay == Overlay::None;
        let cat_visible = app.segments.is_empty() && app.overlay == Overlay::None;
        // a static overlay needs one redraw on open, not thirty per second.
        // key events already set needs_redraw, so it can sit idle afterwards.
        let animating = meter_visible
            || cat_visible
            || app.status.active()
            || app.scroll_home
            || (app.progress - app.progress_smooth as f64).abs() > 0.001
            || (app.scroll as f32 - app.scroll_smooth).abs() > 0.5;
        let elapsed = last_frame.elapsed();
        if needs_redraw || (animating && elapsed >= Duration::from_millis(33)) {
            let palette = theme::palette(app.theme, depth);
            terminal
                .draw(|f| {
                    let area = f.area();
                    let buf = f.buffer_mut();
                    ui::render(buf, area, &mut app, palette);
                })
                .map_err(|e| format!("the draw call failed: {e}"))?;
            needs_redraw = false;
            last_frame = Instant::now();
        }

        if app.status == Status::Idle && !app.message.is_empty() && !animating {
            std::thread::sleep(Duration::from_millis(20));
        }
        if app.quit {
            break;
        }
    }

    if let Some(cancel) = download_cancel.take() {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // restore the terminal before joining a downloader that may be waiting on
    // a network read timeout after cancellation.
    guard.restore();
    if let Some(h) = downloader.take() {
        let _ = h.join();
    }
    if let Some(h) = handle.take() {
        h.stop();
    }
    for h in retiring {
        h.stop();
    }
    Ok(())
}

/// invalidate the current job and park its handle for reaping. the worker's
/// final event will arrive carrying the old id and be dropped by the loop.
fn retire(handle: &mut Option<worker::Handle>, retiring: &mut Vec<worker::Handle>, app: &mut App) {
    if let Some(h) = handle.take() {
        h.cancel();
        // the next id must differ from the one we just retired, even if the
        // counter wrapped around at some point.
        app.job_id = worker::next_job_id().max(h.job_id() + 1);
        retiring.push(h);
    } else {
        app.job_id = worker::next_job_id();
    }
    app.pending_resume = false;
}

#[allow(clippy::too_many_arguments)]
fn process_key(
    key: KeyEvent,
    app: &mut App,
    tx: &mpsc::Sender<WorkerEvent>,
    handle: &mut Option<worker::Handle>,
    retiring: &mut Vec<worker::Handle>,
    downloader: &mut Option<std::thread::JoinHandle<DownloadOutcome>>,
    download_rx: &mut Option<Receiver<model::DownloadProgress>>,
    download_cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    picker_rows: usize,
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('q') => {
                retire(handle, retiring, app);
                app.status = Status::Cancelled;
                app.quit = true;
                return;
            }
            KeyCode::Char(' ') if app.focus == Focus::Input && app.overlay == Overlay::None => {
                app.force_completion();
                return;
            }
            _ => {}
        }
    }

    if app.overlay != Overlay::None {
        match app.overlay {
            Overlay::Help | Overlay::Message => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ')) {
                    app.overlay = Overlay::None;
                    if app.status == Status::Error {
                        app.status = Status::Idle;
                    }
                }
            }
            Overlay::ThemePicker => match key.code {
                KeyCode::Esc => {
                    if let Some(previous) = app.theme_before_preview.take() {
                        app.theme = previous;
                    }
                    app.overlay = Overlay::None;
                }
                KeyCode::Enter => {
                    app.theme_before_preview = None;
                    app.overlay = Overlay::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.theme_index = app.theme_index.saturating_sub(1);
                    app.theme = theme::ThemeKind::ALL
                        .get(app.theme_index)
                        .copied()
                        .unwrap_or(theme::ThemeKind::CatppuccinMocha);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.theme_index = (app.theme_index + 1).min(theme::ThemeKind::ALL.len() - 1);
                    app.theme = theme::ThemeKind::ALL
                        .get(app.theme_index)
                        .copied()
                        .unwrap_or(theme::ThemeKind::CatppuccinMocha);
                }
                _ => {}
            },
            Overlay::Settings => match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Enter if app.settings_index == 4 => {
                    app.theme_before_preview = Some(app.theme);
                    app.theme_index = theme::ThemeKind::ALL
                        .iter()
                        .position(|t| *t == app.theme)
                        .unwrap_or(0);
                    app.overlay = Overlay::ThemePicker;
                }
                KeyCode::Enter if app.settings_index == 3 => {
                    change_setting(app, true);
                }
                KeyCode::Up => app.settings_index = app.settings_index.saturating_sub(1),
                KeyCode::Down => app.settings_index = (app.settings_index + 1).min(5),
                KeyCode::Char('k') => app.settings_index = app.settings_index.saturating_sub(1),
                KeyCode::Char('j') => app.settings_index = (app.settings_index + 1).min(5),
                KeyCode::Left | KeyCode::Right => change_setting(app, key.code == KeyCode::Right),
                _ => {}
            },
            Overlay::ModelPicker => match key.code {
                KeyCode::Esc => {
                    if app.download == app::DownloadState::Downloading {
                        if let Some(c) = download_cancel.take() {
                            c.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        app.download_info = "pausing · saved data will resume on enter".into();
                        app.download_paused = true;
                        app.toast(
                            "download paused · enter resumes from saved data",
                            ToastKind::Info,
                        );
                        // Closing the picker should not require waiting for the
                        // network read to observe cancellation. The job keeps
                        // draining in the background and its partial file is
                        // available when the picker is opened again.
                        app.overlay = Overlay::None;
                    } else {
                        app.overlay = Overlay::None;
                    }
                }
                KeyCode::Char('p') if app.download == app::DownloadState::Downloading => {
                    if let Some(c) = download_cancel.take() {
                        c.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    app.download_info = "pausing · saved data will resume on enter".into();
                    app.download_paused = true;
                }
                KeyCode::Char('c') if app.download == app::DownloadState::Downloading => {
                    app.download_discard = true;
                    app.download_paused = false;
                    if let Some(c) = download_cancel.take() {
                        c.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    app.download_info = "canceling · removing partial download".into();
                }
                KeyCode::Char('x') => {
                    let info = &app.download_models[app.download_index];
                    if app.download == app::DownloadState::Downloading
                        && info.file == app.download_file
                    {
                        app.set_message("cancel the active download before removing its model");
                    } else {
                        match model::remove_model(info) {
                            Ok(()) => {
                                if app.model_name.ends_with(info.file) {
                                    app.model_name.clear();
                                    app.model_label = "-".into();
                                }
                                let message = format!("removed {}", info.label);
                                if app.download != app::DownloadState::Downloading {
                                    app.download = app::DownloadState::Idle;
                                    app.download_info = message.clone();
                                }
                                app.set_message(message);
                            }
                            Err(e) => app.set_message(e),
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => app.move_picker(-1, picker_rows),
                KeyCode::Down | KeyCode::Char('j') => app.move_picker(1, picker_rows),
                KeyCode::PageUp => app.move_picker(-(picker_rows as isize), picker_rows),
                KeyCode::PageDown => app.move_picker(picker_rows as isize, picker_rows),
                KeyCode::Home => {
                    app.download_index = 0;
                    app.picker_top = 0;
                }
                KeyCode::End => {
                    app.download_index = app.download_models.len().saturating_sub(1);
                    app.picker_top = app
                        .download_index
                        .saturating_add(1)
                        .saturating_sub(picker_rows);
                }
                KeyCode::Enter => start_download(app, downloader, download_rx, download_cancel),
                _ => {}
            },
            Overlay::Export => match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Up => app.export_index = app.export_index.saturating_sub(1),
                KeyCode::Char('k') => app.export_index = app.export_index.saturating_sub(1),
                KeyCode::Down => app.export_index = (app.export_index + 1).min(4),
                KeyCode::Char('j') => app.export_index = (app.export_index + 1).min(4),
                KeyCode::Enter => do_export(app),
                _ => {}
            },
            Overlay::Search => match key.code {
                KeyCode::Esc => {
                    app.overlay = Overlay::None;
                    app.query.clear();
                    app.recompute_search();
                }
                KeyCode::Enter => app.overlay = Overlay::None,
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.query.push(c);
                    app.recompute_search();
                }
                KeyCode::Backspace => {
                    app.query.pop();
                    app.recompute_search();
                }
                _ => {}
            },
            Overlay::None => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if app.status.active() {
                // first esc pauses, a second one actually stops the job.
                if let Some(h) = handle.as_ref() {
                    h.pause();
                }
                app.pause();
            } else if app.status == Status::Paused {
                retire(handle, retiring, app);
                app.on_cancelled();
            } else if !app.query.is_empty() {
                app.query.clear();
                app.recompute_search();
            } else if !app.completion.candidates.is_empty() {
                app.dismiss_completion();
            }
        }
        // p and c work in either pane while a job is live; space is reserved
        // for typing in the source field, so it only toggles in the transcript.
        KeyCode::Char('p')
            if app.status.active() || app.pending_resume || app.status == Status::Paused =>
        {
            if app.status.active() {
                if let Some(h) = handle.as_ref() {
                    h.pause();
                }
                app.pause();
            } else {
                if let Some(h) = handle.as_ref() {
                    h.resume();
                }
                app.resume();
            }
        }
        KeyCode::Char(' ') if app.focus == Focus::Transcript => {
            if app.status.active() {
                if let Some(h) = handle.as_ref() {
                    h.pause();
                }
                app.pause();
            } else if app.pending_resume || app.status == Status::Paused {
                if let Some(h) = handle.as_ref() {
                    h.resume();
                }
                app.resume();
            }
        }
        // c stops the job outright, whether it is running or paused.
        KeyCode::Char('c')
            if app.status.active() || app.pending_resume || app.status == Status::Paused =>
        {
            retire(handle, retiring, app);
            app.on_cancelled();
        }
        KeyCode::Up if app.focus == Focus::Input && !app.completion.candidates.is_empty() => {
            app.completion.move_by(-1);
        }
        KeyCode::Down if app.focus == Focus::Input && !app.completion.candidates.is_empty() => {
            app.completion.move_by(1);
        }
        KeyCode::Right if app.focus == Focus::Input && !app.completion.candidates.is_empty() => {
            app.accept_completion();
        }
        KeyCode::Tab if app.focus == Focus::Input && !app.completion.candidates.is_empty() => {
            if app.completion.candidates.len() > 1 && app.accept_common_prefix() {
                // shared prefix filled; keep the popup open for a second tab.
            } else {
                app.accept_completion();
            }
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Input => Focus::Transcript,
                Focus::Transcript => Focus::Input,
            };
            app.completion = crate::app::Completion::default();
            app.update_completion();
        }
        KeyCode::Enter => {
            if app.pending_resume || app.status == Status::Paused {
                // resume in place: the worker is parked at the exact window it
                // was about to decode, so nothing restarts from zero.
                if let Some(h) = handle.as_ref() {
                    h.resume();
                }
                app.resume_from_enter();
                return;
            }
            if app.focus == Focus::Input {
                app.accept_completion();
                if app.can_start() {
                    // if a stale handle survived a missed terminal event, stop
                    // it before handing the channel to a new worker.
                    if let Some(old) = handle.take() {
                        old.stop();
                    }
                    let job_id = worker::next_job_id();
                    app.start(job_id);
                    let job = build_job(app);
                    *handle = Some(worker::spawn(job, tx.clone(), job_id));
                }
            } else if !app.segments.is_empty() {
                app.overlay = Overlay::Export;
            }
        }
        KeyCode::Char('/') if app.focus == Focus::Transcript => {
            app.overlay = Overlay::Search;
        }
        KeyCode::Char('?') if app.focus == Focus::Transcript => {
            app.overlay = Overlay::Help;
        }
        KeyCode::Char('s') if app.focus == Focus::Transcript => app.overlay = Overlay::Settings,
        KeyCode::Char('m') if app.focus == Focus::Transcript => app.overlay = Overlay::ModelPicker,
        KeyCode::Char('t') if app.focus == Focus::Transcript => {
            app.theme_before_preview = Some(app.theme);
            app.theme_index = theme::ThemeKind::ALL
                .iter()
                .position(|t| *t == app.theme)
                .unwrap_or(0);
            app.overlay = Overlay::ThemePicker;
        }
        KeyCode::Char('e') if app.focus == Focus::Transcript => app.overlay = Overlay::Export,
        KeyCode::Char('y') if app.focus == Focus::Transcript => copy_selection(app),
        KeyCode::Char('n') if app.focus == Focus::Transcript => app.jump_match(true),
        KeyCode::Char('N') if app.focus == Focus::Transcript => app.jump_match(false),
        KeyCode::Char('j') | KeyCode::Down if app.focus == Focus::Transcript => {
            app.move_selection(1)
        }
        KeyCode::Char('k') | KeyCode::Up if app.focus == Focus::Transcript => {
            app.move_selection(-1)
        }
        KeyCode::PageDown if app.focus == Focus::Transcript => app.scroll_by(10),
        KeyCode::PageUp if app.focus == Focus::Transcript => app.scroll_by(-10),
        KeyCode::Char(c) if app.focus == Focus::Input => {
            app.input.push(c);
            app.completion.dismissed = false;
            app.update_completion();
        }
        KeyCode::Backspace if app.focus == Focus::Input => {
            app.input.pop();
            app.completion.dismissed = false;
            app.update_completion();
        }
        KeyCode::Delete if app.focus == Focus::Input => {
            app.input.clear();
            app.completion.dismissed = false;
            app.update_completion();
        }
        _ => {}
    }
}

fn build_job(app: &App) -> worker::Job {
    worker::Job {
        source: app.input.clone(),
        model: std::path::PathBuf::from(&app.model_name),
        threads: app.threads,
        lang: if app.lang == "auto" {
            None
        } else {
            Some(app.lang.clone())
        },
        translate: app.translate,
    }
}

fn change_setting(app: &mut App, forward: bool) {
    match app.settings_index {
        0 => app.threads = (app.threads + if forward { 1 } else { -1 }).clamp(1, 32),
        1 => {
            let langs = util::LANGUAGES;
            let pos = langs
                .iter()
                .position(|(code, _)| *code == app.lang)
                .unwrap_or(0);
            let next = (pos as isize + if forward { 1 } else { -1 })
                .rem_euclid(langs.len() as isize) as usize;
            app.lang = langs[next].0.to_string();
            if let Some(name) = util::language_name(&app.lang) {
                app.set_message(format!("language: {name} ({})", app.lang));
            }
        }
        2 => app.translate = !app.translate,
        3 => {
            let current = std::path::PathBuf::from(if app.out_dir.is_empty() {
                "."
            } else {
                &app.out_dir
            });
            let next = if forward {
                std::fs::read_dir(&current)
                    .ok()
                    .and_then(|entries| {
                        entries
                            .flatten()
                            .map(|entry| entry.path())
                            .find(|path| path.is_dir())
                    })
                    .unwrap_or(current)
            } else {
                current
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            };
            app.out_dir = next.to_string_lossy().to_string();
            app.set_message(format!("output directory: {}", app.out_dir));
        }
        4 => {
            app.theme_before_preview.get_or_insert(app.theme);
            let themes = theme::ThemeKind::ALL;
            let pos = themes.iter().position(|t| *t == app.theme).unwrap_or(0);
            let next = (pos as isize + if forward { 1 } else { -1 })
                .rem_euclid(themes.len() as isize) as usize;
            app.theme = themes[next];
            app.theme_index = next;
        }
        5 => {
            let formats = App::export_formats();
            let pos = formats.iter().position(|f| *f == app.format).unwrap_or(0);
            let next = (pos as isize + if forward { 1 } else { -1 })
                .rem_euclid(formats.len() as isize) as usize;
            app.format = formats[next];
        }
        _ => {}
    }
}

fn retry_cache_removal(mut remove: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..20 {
        match remove() {
            Ok(()) => return Ok(()),
            Err(error)
                if error.contains("another scribe is using the model cache") && attempt < 19 =>
            {
                last_error = error;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

fn remove_discarded_model(info: &model::ModelInfo) -> Result<(), String> {
    retry_cache_removal(|| model::remove_model(info))
}

fn remove_discarded_partial(info: &model::ModelInfo) -> Result<(), String> {
    retry_cache_removal(|| model::remove_partial(info))
}

fn normalized_path(path: &std::path::Path) -> std::path::PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn selected_model_matches_managed(app: &App, file: &str) -> bool {
    let managed = model::cache_dir().join(file);
    let selected = std::path::Path::new(&app.model_name);
    if selected.as_os_str().is_empty() {
        return false;
    }
    let selected = std::fs::canonicalize(selected).unwrap_or_else(|_| normalized_path(selected));
    let managed = std::fs::canonicalize(&managed).unwrap_or_else(|_| normalized_path(&managed));
    paths_equal(&selected, &managed)
}

fn clear_discarded_model_selection(app: &mut App, file: &str, selected_managed: bool) {
    let path = model::cache_dir().join(file);
    if selected_managed && !path.exists() {
        app.model_name.clear();
        app.model_label = "-".into();
    }
}

fn finish_model_download(app: &mut App, outcome: DownloadOutcome) {
    match outcome {
        DownloadOutcome::Ready => {
            if app.download_discard {
                let discarded_file = app.download_file.clone();
                let selected_managed = selected_model_matches_managed(app, &discarded_file);
                let result = app
                    .download_models
                    .iter()
                    .find(|model| model.file == app.download_file)
                    .ok_or_else(|| "downloaded model is no longer in the catalog".to_string())
                    .and_then(remove_discarded_model);
                clear_discarded_model_selection(app, &discarded_file, selected_managed);
                app.download_discard = false;
                app.download_paused = false;
                match result {
                    Ok(()) => {
                        app.download = app::DownloadState::Idle;
                        app.download_info = "download canceled · completed file removed".into();
                        app.toast("download canceled", ToastKind::Warn);
                    }
                    Err(err) => {
                        app.download = app::DownloadState::Failed;
                        app.download_info = err.clone();
                        app.set_message(err);
                    }
                }
                return;
            }
            app.download = app::DownloadState::Done;
            app.download_paused = false;
            app.download_frac = 1.0;
            let label = app
                .download_models
                .iter()
                .find(|model| model.file == app.download_file)
                .map(|model| model.label)
                .unwrap_or("model");
            app.toast(
                format!("{label} downloaded and verified"),
                ToastKind::Success,
            );
            app.set_message(format!("{label} is ready. press enter to select it"));
        }
        DownloadOutcome::Paused => {
            app.download = app::DownloadState::Idle;
            if app.download_discard {
                if let Some(info) = app
                    .download_models
                    .iter()
                    .find(|info| info.file == app.download_file)
                {
                    if let Err(err) = remove_discarded_partial(info) {
                        app.download_info = err.clone();
                        app.set_message(err);
                    } else {
                        app.download_info = "download canceled · partial file removed".into();
                        app.toast("download canceled", ToastKind::Warn);
                    }
                }
                app.download_discard = false;
                app.download_paused = false;
            } else {
                app.download_paused = true;
                app.download_info = "paused · enter resumes from saved data".into();
                app.toast("download paused · enter resumes", ToastKind::Info);
            }
        }
        DownloadOutcome::Failed(err) => {
            app.download = app::DownloadState::Failed;
            app.download_paused = false;
            app.download_info = err.clone();
            app.toast(format!("download failed: {err}"), ToastKind::Error);
            app.set_message(err);
        }
    }
}

fn start_download(
    app: &mut App,
    downloader: &mut Option<std::thread::JoinHandle<DownloadOutcome>>,
    download_rx: &mut Option<Receiver<model::DownloadProgress>>,
    download_cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    let Some(info) = app.download_models.get(app.download_index).cloned() else {
        return;
    };
    if downloader.is_some() && info.file == app.download_file {
        app.set_message("download in progress (press c or esc to pause)");
        return;
    }
    let cached = model::cache_dir().join(info.file);
    if cached.is_file() {
        if let Err(e) = model::validate_cached(&info, &cached) {
            app.set_message(format!(
                "{} is corrupted: {e}. remove and re-download it",
                info.label
            ));
        } else {
            app.set_model(info.label.to_string(), cached.to_string_lossy().to_string());
            app.overlay = Overlay::None;
        }
        return;
    }
    if downloader.is_some() {
        let what = if app.download_paused {
            "previous download is still stopping; press enter again in a moment"
        } else {
            "another download is in progress"
        };
        app.set_message(what);
        return;
    }
    app.download_paused = false;
    let info_label = info.label.to_string();
    app.download_file = info.file.to_string();
    app.download_discard = false;
    app.download = app::DownloadState::Downloading;
    app.download_frac = model::partial_size(&info)
        .map(|bytes| bytes as f64 / (info.size_mib.saturating_mul(1024 * 1024)).max(1) as f64)
        .unwrap_or(0.0)
        .clamp(0.0, 0.99);
    let initial_info = if let Some(part_bytes) = model::partial_size(&info) {
        format!("resuming at {}", human_bytes(part_bytes))
    } else {
        format!("connecting ({})", human_bytes(info.size_mib * 1024 * 1024))
    };
    app.download_info = initial_info;
    let (p_tx, p_rx) = mpsc::channel::<model::DownloadProgress>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_thread = cancel.clone();
    let h = std::thread::spawn(move || {
        let result = model::download(&info, &cancel_thread, |p| {
            let _ = p_tx.send(p);
        });
        match result {
            Ok(_) => DownloadOutcome::Ready,
            Err(_) if cancel_thread.load(std::sync::atomic::Ordering::Relaxed) => {
                DownloadOutcome::Paused
            }
            Err(err) => DownloadOutcome::Failed(err),
        }
    });
    *download_cancel = Some(cancel);
    *download_rx = Some(p_rx);
    *downloader = Some(h);
    app.set_message(format!("fetching {}...", info_label));
}

fn do_export(app: &mut App) {
    if app.segments.is_empty() {
        app.set_message("nothing to export yet. transcribe something first");
        app.overlay = Overlay::Message;
        return;
    }
    let fmt = App::export_formats()[app.export_index];
    let out_dir = if app.out_dir.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&app.out_dir))
    };
    if let Some(dir) = out_dir
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        app.set_message(format!("cannot create {}: {e}", dir.display()));
        app.overlay = Overlay::Message;
        return;
    }
    let path = util::output_path(&app.input, out_dir, fmt.extension());
    let text = export::export(fmt, &app.segments, Some(&app.input));
    match export::write_exclusive(&path, text.as_bytes()) {
        Ok(()) => {
            app.set_message(format!("saved to {}", path.display()));
            app.overlay = Overlay::None;
        }
        Err(e) => {
            app.set_message(e);
            app.overlay = Overlay::Message;
        }
    }
}

fn copy_selection(app: &mut App) {
    if let Some(seg) = app.segments.get(app.selected) {
        let text = format!("{} {}", timestamp_short(seg.start_cs), seg.text);
        let seq = util::osc52(&text);
        print!("{seq}");
        let _ = io::stdout().flush();
        app.set_message("copied. the terminal did the clipboard dance over osc 52");
    }
}

use util::{duration_short, human_bytes, timestamp_short};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_transcription_requires_worker_completion() {
        let segment = || export::Segment::new(0, 100, "hello".into());

        let incomplete = collect_plain_segments([WorkerEventKind::Segment(segment())]);
        assert_eq!(
            incomplete.unwrap_err(),
            "transcription ended before the worker reported completion"
        );

        let complete =
            collect_plain_segments([WorkerEventKind::Segment(segment()), WorkerEventKind::Done])
                .unwrap();
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].text, "hello");
        let empty = collect_plain_segments([WorkerEventKind::Done]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn plain_transcription_reports_worker_failure_and_cancellation() {
        let cancelled = collect_plain_segments([WorkerEventKind::Cancelled]);
        assert_eq!(cancelled.unwrap_err(), "transcription cancelled");

        let failed = collect_plain_segments([WorkerEventKind::Error("decoder failed".into())]);
        assert_eq!(failed.unwrap_err(), "decoder failed");
    }

    #[test]
    fn cache_removal_retries_contention_beyond_the_old_window() {
        let mut attempts = 0;
        let result = retry_cache_removal(|| {
            attempts += 1;
            if attempts < 7 {
                Err("another scribe is using the model cache".into())
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(attempts, 7);
    }

    #[test]
    fn cache_removal_retries_transient_lock_contention() {
        let mut attempts = 0;
        let result = retry_cache_removal(|| {
            attempts += 1;
            if attempts < 3 {
                Err("another scribe is using the model cache".into())
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn cancel_after_download_commit_removes_the_completed_model() {
        let dir = std::env::temp_dir().join(format!("scribe-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::model::set_test_cache_dir(Some(dir.clone()));

        let info = crate::model::find("tiny").unwrap();
        let path = dir.join(info.file);
        std::fs::write(&path, b"completed model").unwrap();

        let mut app = App::new(&crate::cli::Args::default());
        app.download_models = crate::model::CATALOG.to_vec();
        app.download_file = info.file.to_string();
        app.model_name = path.to_string_lossy().to_string();
        app.model_label = info.label.to_string();
        app.download_discard = true;
        app.download_paused = true;

        finish_model_download(&mut app, DownloadOutcome::Ready);

        assert!(!path.exists());
        assert!(app.model_name.is_empty());
        assert_eq!(app.model_label, "-");
        assert!(!app.download_discard);
        assert!(!app.download_paused);
        assert_eq!(app.download, app::DownloadState::Idle);
        crate::model::set_test_cache_dir(None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discard_clears_equivalent_managed_model_selection() {
        let info = crate::model::find("tiny").unwrap();
        let dir =
            std::env::temp_dir().join(format!("scribe-relative-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::model::set_test_cache_dir(Some(dir.clone()));
        let path = dir.join(info.file);
        std::fs::write(&path, b"model").unwrap();
        let alias = dir
            .join("..")
            .join(dir.file_name().unwrap())
            .join(info.file);
        let mut app = App::new(&crate::cli::Args::default());
        app.model_name = alias.to_string_lossy().into_owned();
        app.model_label = info.label.to_string();
        let selected_managed = selected_model_matches_managed(&app, info.file);
        assert!(selected_managed);
        std::fs::remove_file(&path).unwrap();

        clear_discarded_model_selection(&mut app, info.file, selected_managed);

        assert!(app.model_name.is_empty());
        assert_eq!(app.model_label, "-");
        crate::model::set_test_cache_dir(None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn discard_clears_case_insensitive_managed_model_selection() {
        let info = crate::model::find("tiny").unwrap();
        let dir = std::env::temp_dir().join(format!("scribe-case-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::model::set_test_cache_dir(Some(dir.clone()));
        let path = dir.join(info.file);
        std::fs::write(&path, b"model").unwrap();

        let mut app = App::new(&crate::cli::Args::default());
        app.model_name = path.to_string_lossy().to_uppercase();
        app.model_label = info.label.to_string();
        let selected_managed = selected_model_matches_managed(&app, info.file);
        assert!(selected_managed);
        std::fs::remove_file(&path).unwrap();

        clear_discarded_model_selection(&mut app, info.file, selected_managed);

        assert!(app.model_name.is_empty());
        assert_eq!(app.model_label, "-");
        crate::model::set_test_cache_dir(None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn discard_clears_symlinked_managed_model_selection() {
        use std::os::unix::fs::symlink;
        let info = crate::model::find("tiny").unwrap();
        let dir =
            std::env::temp_dir().join(format!("scribe-symlink-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::model::set_test_cache_dir(Some(dir.clone()));
        let path = dir.join(info.file);
        let alias = dir.join("selected-model.bin");
        std::fs::write(&path, b"model").unwrap();
        symlink(&path, &alias).unwrap();

        let mut app = App::new(&crate::cli::Args::default());
        app.model_name = alias.to_string_lossy().into_owned();
        app.model_label = info.label.to_string();
        let selected_managed = selected_model_matches_managed(&app, info.file);
        assert!(selected_managed);
        std::fs::remove_file(&path).unwrap();

        clear_discarded_model_selection(&mut app, info.file, selected_managed);

        assert!(app.model_name.is_empty());
        assert_eq!(app.model_label, "-");
        crate::model::set_test_cache_dir(None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discard_does_not_clear_custom_model_with_matching_filename() {
        let info = crate::model::find("tiny").unwrap();
        let custom = std::env::temp_dir().join("custom").join(info.file);
        let mut app = App::new(&crate::cli::Args::default());
        app.model_name = custom.to_string_lossy().to_string();
        app.model_label = "custom".into();

        let selected_managed = selected_model_matches_managed(&app, info.file);
        clear_discarded_model_selection(&mut app, info.file, selected_managed);

        assert_eq!(app.model_name, custom.to_string_lossy().into_owned());
        assert_eq!(app.model_label, "custom");
    }

    #[test]
    fn terminal_cleanup_continues_after_a_failed_step() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let ran = Rc::new(RefCell::new(Vec::new()));
        let mut steps: Vec<CleanupStep> = Vec::new();
        for (name, fails) in [("first", true), ("second", false), ("third", false)] {
            let ran = ran.clone();
            steps.push((
                name,
                Box::new(move || {
                    ran.borrow_mut().push(name);
                    if fails {
                        Err(io::Error::other("nope"))
                    } else {
                        Ok(())
                    }
                }),
            ));
        }
        let failed = restore_terminal_steps(&mut steps);
        assert_eq!(failed, vec!["first".to_string()]);
        assert_eq!(*ran.borrow(), vec!["first", "second", "third"]);
    }
}
