//! Command-line interface. Arguments follow the contract in the README.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{Border, View};

/// Display options. They override the config file for this run.
#[derive(Debug, Clone, Copy, Default, Args)]
pub struct Display {
    /// The frame: none or rounded.
    #[arg(long, value_enum)]
    pub border: Option<Border>,
    /// The view to start in: compact or expanded.
    #[arg(long, value_enum)]
    pub view: Option<View>,
}

impl Display {
    /// The flags again, to pass on to another rexroster command.
    pub fn to_args(self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(b) = self.border {
            args.extend(["--border".to_owned(), b.to_string()]);
        }
        if let Some(v) = self.view {
            args.extend(["--view".to_owned(), v.to_string()]);
        }
        args
    }
}

#[derive(Debug, Parser)]
#[command(version, about = "Lists a Rex lead's detached worker sessions")]
pub struct Cli {
    /// The lead's block id. Defaults to $REXROSTER_LEAD.
    #[arg(long, env = "REXROSTER_LEAD")]
    pub lead: Option<String>,

    #[command(flatten)]
    pub display: Display,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the roster in a floating layer in the lead's tab, unless one is already open.
    Open {
        /// The lead's block id.
        lead: String,
        #[command(flatten)]
        display: Display,
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

    #[test]
    fn display_flags_on_the_roster_and_open() {
        let cli = Cli::try_parse_from(["rexroster", "--lead", "L", "--border", "rounded"]).unwrap();
        assert_eq!(cli.display.border, Some(Border::Rounded));
        assert_eq!(cli.display.view, None);
        let cli = Cli::try_parse_from(["rexroster", "open", "L", "--view", "expanded"]).unwrap();
        match cli.command {
            Some(Command::Open { lead, display }) => {
                assert_eq!(lead, "L");
                assert_eq!(display.to_args(), ["--view", "expanded"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["rexroster", "--border", "square"]).is_err());
    }
}
