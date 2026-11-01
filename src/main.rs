mod cli;
mod events;
mod launch;
mod model;
mod rex;
mod size;
mod tag;
mod tui;

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
    let session = std::env::var("REX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .context("REX_SESSION is not set; run rexroster inside a Rex terminal")?;
    match cli.command {
        None => {
            let lead = cli
                .lead
                .context("no lead: pass --lead <block-id> or set REXROSTER_LEAD")?;
            tui::run(lead, session)
        }
        Some(Command::Open { lead }) => {
            let exe = std::env::current_exe().context("finding rexroster's own path")?;
            if let Some(layer) = launch::open(&rex::Cli::new(), &session, &lead, &exe)? {
                println!("{layer}");
            }
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
            let out = launch::spawn(&rex::Cli::new(), &session, &spec, &exe, path.as_ref())?;
            println!("{out}");
            Ok(())
        }
        Some(Command::Tag { lead, task, cmd }) => tag::run(&lead, task.as_deref(), &cmd),
    }
}
