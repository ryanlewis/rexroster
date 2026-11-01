mod cli;
mod config;
mod events;
mod launch;
mod lock;
mod model;
mod rex;
mod size;
mod tag;
mod tui;
mod watch;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rexroster: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let display = cli.subcommand_display()?;
    if let Some(Command::Watch) = cli.command {
        // The watcher hears every session, so it needs no REX_SESSION.
        let opts = config::load_strict(config::path().as_deref())?;
        return watch::run(rex::Cli::new().command(), opts.notify, &watch::lock_path());
    }
    let session = std::env::var("REX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .context("REX_SESSION is not set; run rexroster inside a Rex terminal")?;
    match cli.command {
        None => {
            let lead = cli
                .lead
                .context("no lead: pass --lead <block-id> or set REXROSTER_LEAD")?;
            let path = config::path();
            let (file, error) = config::load(path.as_deref());
            let opts = file.with_flags(display.border, display.view);
            tui::run(lead, session, tui::Settings { opts, path, error })
        }
        Some(Command::Open { lead, .. }) => {
            let exe = std::env::current_exe().context("finding rexroster's own path")?;
            let flags = display.to_args();
            let layer = launch::open(
                &rex::Cli::new(),
                &session,
                &lead,
                &exe,
                &flags,
                &lock::dir(),
            )?;
            if let Some(layer) = layer {
                println!("{layer}");
            }
            start_watcher(&exe);
            Ok(())
        }
        Some(Command::Spawn {
            lead,
            name,
            task,
            cwd,
            cmd,
        }) => {
            let exe = std::env::current_exe().context("finding rexroster's own path")?;
            let spec = launch::Spawn {
                lead: &lead,
                name: &name,
                task: task.as_deref(),
                cwd: cwd.as_deref(),
                cmd: &cmd,
            };
            let path = std::env::var_os("PATH");
            let out = launch::spawn(
                &rex::Cli::new(),
                &session,
                &spec,
                &exe,
                path.as_ref(),
                &lock::dir(),
            )?;
            println!("{}", out.json);
            if let Some(err) = out.open_error {
                eprintln!("rexroster: the worker started, but the roster didn't open: {err}");
            }
            start_watcher(&exe);
            Ok(())
        }
        Some(Command::Tag { lead, task, cmd }) => tag::run(&lead, task.as_deref(), &cmd),
        Some(Command::Watch) => unreachable!("handled above"),
    }
}

/// Start the watcher in the background when the config wants one. A failure
/// is a warning: the roster and the worker don't depend on it.
fn start_watcher(exe: &std::path::Path) {
    let opts = match config::load_strict(config::path().as_deref()) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("rexroster: not starting the watcher: {err:#}");
            return;
        }
    };
    if !opts.watch {
        return;
    }
    if let Err(err) = watch::start(exe, &watch::lock_path()) {
        eprintln!("rexroster: couldn't start the watcher: {err:#}");
    }
}
