//! gitmap — a WinDirStat-style TUI git interface.

use gitmap::{app, git, layout, worker};

use anyhow::Result;
use app::{App, Split};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use git::PorcelainBackend;
use layout::tree::Scale;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::mpsc;
use worker::Request;

struct Opts {
    scale: Scale,
    split: Split,
    path: std::path::PathBuf,
}

fn parse_args() -> Result<Option<Opts>> {
    let mut o = Opts {
        scale: Scale::Sqrt,
        split: Split::Auto,
        path: std::env::current_dir()?,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("gitmap {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--scale" => {
                o.scale = match args.next().as_deref() {
                    Some("linear") => Scale::Linear,
                    Some("sqrt") => Scale::Sqrt,
                    Some("log") => Scale::Log,
                    other => anyhow::bail!("--scale wants linear|sqrt|log, got {other:?}"),
                }
            }
            "--split" => {
                o.split = match args.next().as_deref() {
                    Some("auto") => Split::Auto,
                    Some("vertical") => Split::Vertical,
                    Some("horizontal") => Split::Horizontal,
                    other => {
                        anyhow::bail!("--split wants auto|vertical|horizontal, got {other:?}")
                    }
                }
            }
            p if !p.starts_with('-') => o.path = std::path::PathBuf::from(p),
            other => anyhow::bail!("unknown option {other}; try --help"),
        }
    }
    Ok(Some(o))
}

const USAGE: &str = "\
gitmap — a spatial map of a git repository

USAGE:
    gitmap [OPTIONS] [PATH]

OPTIONS:
    --scale <linear|sqrt|log>            area transform (default: sqrt)
    --split <auto|vertical|horizontal>   pane split (default: auto)
    -h, --help                           show this help
    -V, --version                        show the version

Press ? inside the program for keybindings.";

fn main() -> std::process::ExitCode {
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The terminal is already restored by this point, so a plain
            // stderr write is safe and lands where a shell expects it.
            eprintln!("gitmap: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    let Some(opts) = parse_args()? else {
        return Ok(());
    };

    let backend = Arc::new(PorcelainBackend::discover(&opts.path)?);
    // Git paths are relative to the repo root, so run there.
    std::env::set_current_dir(backend.root())?;

    let mut term = setup()?;
    let result = run(&mut term, opts, backend);
    restore()?;
    result
}

/// Enter the alternate screen and install the panic hook.
///
/// A TUI that panics without cleanup leaves the user with a wrecked shell, so
/// the hook goes in before anything can fail.
fn setup() -> Result<ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut out = io::stdout();
    // 1003 (any-event motion) plus 1006 (SGR coordinates, required above 223
    // columns) is what makes hover work; crossterm's EnableMouseCapture sets both.
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(out);
    Ok(ratatui::Terminal::new(backend)?)
}

fn restore() -> Result<()> {
    let mut out = io::stdout();
    execute!(out, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn run(
    term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    opts: Opts,
    backend: Arc<PorcelainBackend>,
) -> Result<()> {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (msg_tx, msg_rx) = mpsc::channel::<worker::Message>();
    let worker = worker::spawn(backend, req_rx, msg_tx);

    // The input thread does nothing but block on read() and forward.
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(e) = crossterm::event::read() {
            if ev_tx.send(e).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(req_tx.clone());
    app.scale = opts.scale;
    app.split = opts.split;
    let _ = req_tx.send(Request::Tree);
    let _ = req_tx.send(Request::Status);

    while !app.should_quit {
        if app.dirty {
            term.draw(|f| app.draw(f))?;
            app.dirty = false;
        }
        app.pump(&ev_rx, &msg_rx)?;
    }

    let _ = req_tx.send(Request::Quit);
    drop(req_tx);
    let _ = worker.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_mentions_every_documented_flag() {
        for flag in ["--scale", "--split", "--help", "--version"] {
            assert!(USAGE.contains(flag), "{flag} missing from usage");
        }
    }
}
