//! vert: a terminal UI to grok what your agents are building.
//!
//! This binary is thin glue: terminal setup, the input thread, the event
//! loop and the external editor. State lives in [`vert::app`] and I/O in
//! [`vert::jobs`].

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::DefaultTerminal;
use vert::app::{App, Effect, Msg, WatchStatus};
use vert::config::Config;
use vert::{age, editor, git, jobs, ui, watch};

/// Input poll interval; also drives spinners and relative times.
const TICK: Duration = Duration::from_millis(120);
/// How long to wait for the input thread to park before launching an editor.
const PARK_TIMEOUT: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vert: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Pauses the input thread so a child process can own the terminal.
#[derive(Default)]
struct InputGate {
    paused: AtomicBool,
    parked: AtomicBool,
}

struct Runtime {
    jobs: jobs::Context,
    tx: Sender<Msg>,
    gate: Arc<InputGate>,
}

impl Runtime {
    /// Runs a background effect on a worker thread, sending its message back.
    fn spawn(&self, effect: Effect) {
        jobs::begin(&effect, &self.jobs);
        let (ctx, tx) = (self.jobs.clone(), self.tx.clone());
        thread::spawn(move || {
            if let Some(msg) = jobs::run(effect, &ctx) {
                // The receiver only disappears when vert is exiting.
                let _ = tx.send(msg);
            }
        });
    }
}

fn run() -> Result<()> {
    let config = Config::parse();
    let labeler = config.labeler().context("invalid label thresholds")?;
    let repo = git::open(&config.path)?;
    let workdir = git::workdir(&repo)?;
    let info = git::info(&repo);
    let layout = watch::Layout::of(&repo)?;
    drop(repo);

    let (tx, rx) = mpsc::channel();
    let (mut app, effects) = App::new(
        info,
        usize::from(config.module_depth),
        config.max_highlight_lines,
        age::now(),
    );
    app.set_refresh(config.refresh());
    let _watch = if config.no_watch {
        app.watch = WatchStatus::Disabled;
        None
    } else {
        let tx = tx.clone();
        match watch::start(&layout, config.debounce(), move |changes| {
            let _ = tx.send(Msg::Changed(changes));
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                app.watch = WatchStatus::Failed(e.to_string());
                None
            }
        }
    };

    let rt = Runtime {
        jobs: jobs::Context {
            workdir,
            labeler,
            commit_limit: config.commit_limit(),
            search_limit: config.search_limit(),
            max_file_bytes: config.max_file_bytes(),
            search_generation: Arc::new(AtomicU64::new(0)),
            commit_cache: Arc::default(),
            base: config.base.clone(),
            branch_limit: config.branch_limit(),
            branch_cache: Arc::default(),
            github: config.github(),
            tokens: jobs::TokenSource::default(),
            token: Arc::default(),
        },
        tx: tx.clone(),
        gate: Arc::new(InputGate::default()),
    };
    spawn_input(tx, Arc::clone(&rt.gate));

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &rt, &rx, effects);
    ratatui::restore();
    result
}

fn spawn_input(tx: Sender<Msg>, gate: Arc<InputGate>) {
    thread::spawn(move || {
        loop {
            if gate.paused.load(Ordering::Acquire) {
                gate.parked.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            gate.parked.store(false, Ordering::Release);
            let msg = match event::poll(TICK) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => Msg::Key(key),
                    Ok(_) => Msg::Tick(age::now()),
                    Err(_) => break,
                },
                Ok(false) => Msg::Tick(age::now()),
                Err(_) => break,
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rt: &Runtime,
    rx: &Receiver<Msg>,
    initial: Vec<Effect>,
) -> Result<()> {
    let mut pending = initial;
    loop {
        for effect in pending.drain(..) {
            match effect {
                Effect::Quit => return Ok(()),
                Effect::Edit { path, line, root } => {
                    let dir = root.unwrap_or_else(|| rt.jobs.workdir.clone());
                    if let Err(e) = edit(terminal, rt, &dir, &path, line) {
                        app.update(Msg::Notice(format!("{e:#}")));
                    }
                }
                background => rt.spawn(background),
            }
        }
        terminal
            .draw(|frame| ui::draw(frame, app))
            .context("failed to draw")?;
        let msg = rx.recv().context("event channel closed")?;
        pending.extend(app.update(msg));
        while let Ok(msg) = rx.try_recv() {
            pending.extend(app.update(msg));
        }
    }
}

/// Suspends the UI, runs the editor, and restores the UI.
fn edit(
    terminal: &mut DefaultTerminal,
    rt: &Runtime,
    dir: &Path,
    path: &str,
    line: u32,
) -> Result<()> {
    rt.gate.paused.store(true, Ordering::Release);
    let start = Instant::now();
    while !rt.gate.parked.load(Ordering::Acquire) && start.elapsed() < PARK_TIMEOUT {
        thread::sleep(Duration::from_millis(5));
    }
    ratatui::restore();
    let editor = editor::editor_from_env();
    let status = editor::command(&editor, &dir.join(path), line)
        .current_dir(dir)
        .status();
    let restored = (|| -> Result<()> {
        enable_raw_mode().context("failed to re-enter raw mode")?;
        execute!(std::io::stdout(), EnterAlternateScreen)
            .context("failed to restore the screen")?;
        terminal.clear().context("failed to clear the screen")?;
        Ok(())
    })();
    // Resume input even if restoring failed, so vert stays responsive.
    rt.gate.paused.store(false, Ordering::Release);
    restored?;
    let status = status.with_context(|| format!("couldn't launch editor `{editor}`"))?;
    anyhow::ensure!(status.success(), "editor `{editor}` exited with {status}");
    Ok(())
}
