//! Command-line interface. Arguments follow the contract in the README.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "Lists a Rex lead's detached worker sessions")]
pub struct Cli {
    /// The lead's block id. Defaults to $REXROSTER_LEAD.
    #[arg(long, env = "REXROSTER_LEAD")]
    pub lead: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the roster in a floating layer in the lead's tab, unless one is already open.
    Open {
        /// The lead's block id.
        lead: String,
    },
    /// Start a detached worker block for a lead, then open its roster.
    Spawn {
        /// The lead's block id.
        lead: String,
        /// The worker's block label.
        name: String,
        /// What the worker is doing, shown in the roster.
        #[arg(long)]
        task: Option<String>,
        /// Working directory for the worker.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// The command to run, after `--`.
        #[arg(last = true, required = true)]
        cmd: Vec<OsString>,
    },
    /// Tag this terminal as a lead's worker, then exec the command.
    Tag {
        /// The lead's block id.
        lead: String,
        /// What the worker is doing, shown in the roster.
        #[arg(long)]
        task: Option<String>,
        /// The command to run, after `--`.
        #[arg(last = true, required = true)]
        cmd: Vec<OsString>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn tag_takes_the_command_after_double_dash() {
        let cli = Cli::try_parse_from([
            "rexroster",
            "tag",
            "L",
            "--task",
            "t",
            "--",
            "printf",
            "--x",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Tag { lead, task, cmd }) => {
                assert_eq!(lead, "L");
                assert_eq!(task.as_deref(), Some("t"));
                assert_eq!(cmd, ["printf", "--x"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tag_needs_a_command() {
        assert!(Cli::try_parse_from(["rexroster", "tag", "L"]).is_err());
    }
}
