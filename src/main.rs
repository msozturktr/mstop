mod alerts;
mod app;
mod collect;
mod fmt;
mod history;
mod ui;

use anyhow::{Result, bail};
use app::App;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::sync::mpsc;
use std::time::Duration;

enum Msg {
    Snap(Box<collect::Snapshot>),
    Input(Event),
}

struct Args {
    interval_ms: u64,
    dump: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        interval_ms: 1000,
        dump: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => args.dump = true,
            "--interval" | "-i" => {
                let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
                    bail!("--interval needs a number of milliseconds");
                };
                args.interval_ms = v.clamp(100, 60_000);
            }
            "-h" | "--help" => {
                println!(
                    "mstop - friendly terminal system monitor\n\nUsage: mstop [--interval <ms>] [--dump]\n\n  --interval <ms>  refresh interval (default 1000)\n  --dump           print one plain-text snapshot and exit"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(args)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if args.dump {
        let mut c = collect::Collector::new();
        c.sample();
        std::thread::sleep(Duration::from_secs(1));
        // The services worker runs off-thread; give it a moment to report.
        let mut snap = c.sample();
        for _ in 0..40 {
            if !matches!(
                snap.services.systemd,
                collect::services::SystemdState::Pending
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            snap = c.sample();
        }
        print!("{}", snap.dump());
        let mut engine = alerts::Engine::new();
        engine.evaluate(&snap, 0.0, collect::services::unix_now());
        print!("{}", engine.dump());
        return Ok(());
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let result = run(args.interval_ms);
    restore_terminal();
    result
}

fn run(interval_ms: u64) -> Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let (tx, rx) = mpsc::channel::<Msg>();
    let snap_tx = tx.clone();
    collect::spawn(Duration::from_millis(interval_ms), move |s| {
        snap_tx.send(Msg::Snap(Box::new(s))).is_ok()
    });
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(interval_ms);
    let hist_path = history::state_path();
    if let Some(p) = &hist_path {
        app.load_history(p);
    }
    loop {
        term.draw(|f| ui::draw(f, &app))?;
        // Block until something happens, then drain whatever else is queued.
        let mut msg = Some(rx.recv()?);
        while let Some(m) = msg {
            match m {
                Msg::Snap(s) => app.apply_snapshot(*s),
                Msg::Input(Event::Key(k)) => app.handle_key(k),
                Msg::Input(_) => {}
            }
            msg = rx.try_recv().ok();
        }
        if let Some(p) = &hist_path {
            if app.quit {
                app.save_history(p);
            } else {
                app.maybe_save_history(p);
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}
