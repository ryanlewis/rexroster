mod cli;
mod tag;

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
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
    let _session = std::env::var("REX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .context("REX_SESSION is not set; run rexroster inside a Rex terminal")?;
    match cli.command {
        None => {
            let _lead = cli
                .lead
                .context("no lead: pass --lead <block-id> or set REXROSTER_LEAD")?;
            bail!("the roster TUI is not implemented yet")
        }
        Some(Command::Open { .. }) => bail!("open is not implemented yet"),
        Some(Command::Spawn { .. }) => bail!("spawn is not implemented yet"),
        Some(Command::Tag { lead, task, cmd }) => tag::run(&lead, task.as_deref(), &cmd),
    }
}
