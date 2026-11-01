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
    /// These flags, falling back to `other` for any not given.
    pub fn or(self, other: Display) -> Display {
        Display {
            border: self.border.or(other.border),
            view: self.view.or(other.view),
        }
    }

    fn is_empty(self) -> bool {
        self.border.is_none() && self.view.is_none()
    }

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

impl Cli {
    /// The display flags for a subcommand. Flags given before `open` apply
    /// to it (its own win); before `spawn` or `tag` they have no meaning, so
    /// they are an error rather than silently ignored.
    pub fn subcommand_display(&self) -> anyhow::Result<Display> {
        match &self.command {
            Some(Command::Open { display, .. }) => Ok(display.or(self.display)),
            Some(_) if !self.display.is_empty() => {
                anyhow::bail!("--border and --view only apply to the roster and to open")
            }
            _ => Ok(self.display),
        }
    }
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
    /// Float detached workers that are blocked on a prompt into view, and
    /// sink them when they move on. One runs per user per Rex server; open
    /// and spawn start it.
    Watch,
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
    fn watch_takes_no_arguments() {
        let cli = Cli::try_parse_from(["rexroster", "watch"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Watch)));
        assert!(Cli::try_parse_from(["rexroster", "watch", "x"]).is_err());
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

    #[test]
    fn top_level_display_flags_reach_open_or_are_refused() {
        // Review finding: `rexroster --view expanded open L` ignored the flag.
        let cli = Cli::try_parse_from(["rexroster", "--view", "expanded", "open", "L"]).unwrap();
        let flags = cli.subcommand_display().unwrap();
        assert_eq!(flags.to_args(), ["--view", "expanded"]);
        let cli = Cli::try_parse_from([
            "rexroster",
            "--border",
            "none",
            "open",
            "L",
            "--border",
            "rounded",
        ])
        .unwrap();
        assert_eq!(
            cli.subcommand_display().unwrap().to_args(),
            ["--border", "rounded"]
        );
        let cli = Cli::try_parse_from(["rexroster", "--border", "rounded", "tag", "L", "--", "x"])
            .unwrap();
        assert!(cli.subcommand_display().is_err());
    }
}
