//! `rexroster events`: one line per change in a lead's workers that the lead
//! has to act on, for an agent's background monitor to wake it with.
//!
//! The roster's model does the work: it finds the lead's workers from their
//! meta records and gives each an effective state, in which an idle root with
//! a working subagent still counts as working. This module only diffs those
//! states between events and words the changes that matter: a worker stopped
//! (with its report's first line, when one was written this turn), a worker
//! blocked or in error, a new worker ready for its brief, a worker's program
//! exited, a worker's block closed.
//! Starting work is silent.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::events::{self, Event};
use crate::model::{Model, Row};
use crate::rex;

/// What the lead last knew about a worker.
#[derive(Debug, Clone)]
struct Seen {
    label: String,
    state: String,
    /// When its current turn started, in Unix seconds: reports older than
    /// this belong to an earlier turn.
    turn: u64,
}

/// Diffs the model's rows into lines.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
    primed: bool,
}

fn stopped(state: &str) -> bool {
    matches!(state, "idle" | "done")
}

impl Tracker {
    /// The lines for the change from the last call to `rows`. The first call
    /// only records where every worker stands, except that a worker already
    /// blocked or in error is reported: a lead that restarts its monitor
    /// still hears about it.
    pub fn diff(&mut self, rows: &[Row], now: u64, reports: Option<&Path>) -> Vec<String> {
        let mut out = Vec::new();
        let first = !self.primed;
        self.primed = true;
        for row in rows {
            let prev = self.seen.get(&row.block).cloned();
            let mut turn = prev.as_ref().map_or(now, |p| p.turn);
            let changed = prev.as_ref().is_none_or(|p| p.state != row.state);
            if changed {
                let was = prev.as_ref().map(|p| p.state.as_str());
                if row.state == "working" && !was.is_some_and(|w| w == "blocked") {
                    turn = now;
                }
                if let Some(line) = word(row, was, first, turn, reports) {
                    out.push(line);
                }
            }
            self.seen.insert(
                row.block.clone(),
                Seen {
                    label: row.label.clone(),
                    state: row.state.clone(),
                    turn,
                },
            );
        }
        let live: Vec<&str> = rows.iter().map(|r| r.block.as_str()).collect();
        let mut gone: Vec<String> = self
            .seen
            .keys()
            .filter(|b| !live.contains(&b.as_str()))
            .cloned()
            .collect();
        gone.sort();
        for block in gone {
            let seen = self.seen.remove(&block).expect("listed above");
            out.push(format!("{} closed", seen.label));
        }
        out
    }
}

/// The line for one worker's change of state, if the lead needs one.
fn word(
    row: &Row,
    was: Option<&str>,
    first: bool,
    turn: u64,
    reports: Option<&Path>,
) -> Option<String> {
    let name = &row.label;
    match row.state.as_str() {
        "blocked" | "error" => Some(format!("{name} {}: {}", row.state, row.msg)),
        _ if first => None,
        // A new worker reaching its prompt is ready for a brief.
        s if stopped(s) && was.is_none_or(|w| w == "starting") => Some(format!("{name} ready")),
        // A switch between idle and done is not a new stop.
        s if stopped(s) => {
            if was.is_some_and(stopped) {
                return None;
            }
            Some(match reports {
                Some(dir) => format!("{name} {s}: {}", report_line(dir, name, turn)),
                None => format!("{name} {s}"),
            })
        }
        // The root record went away: the worker's program exited, its block
        // stayed open.
        "starting" if was.is_some_and(|w| w != "starting") => Some(format!("{name} exited")),
        _ => None,
    }
}

/// The first line and path of the newest `<dir>/<name>#*.md` written at or
/// after `turn`, or `no report`.
fn report_line(dir: &Path, name: &str, turn: u64) -> String {
    let prefix = format!("{name}#");
    let newest = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            if !file.starts_with(&prefix) || !file.ends_with(".md") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            let secs = mtime.duration_since(UNIX_EPOCH).ok()?.as_secs();
            (secs >= turn).then_some((secs, e.path()))
        })
        .max();
    let Some((_, path)) = newest else {
        return "no report".to_owned();
    };
    let first = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.lines().next().map(str::to_owned))
        .unwrap_or_default();
    format!("{first} ({})", path.display())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A mistyped `--reports` would make every stop read `no report`, so it has
/// to name a directory from the start.
fn check_reports(dir: &Path) -> Result<()> {
    let meta = std::fs::metadata(dir).with_context(|| format!("--reports {}", dir.display()))?;
    anyhow::ensure!(
        meta.is_dir(),
        "--reports {}: not a directory",
        dir.display()
    );
    Ok(())
}

/// Follow the lead's workers until none is left, printing a line per change.
/// A closed event stream prints a line too, then fails, so a lost Rex server
/// is not mistaken for quiet workers.
pub fn run(session: &str, lead: String, reports: Option<PathBuf>) -> Result<()> {
    if let Some(dir) = &reports {
        check_reports(dir)?;
    }
    let cli = rex::Cli::new();
    let (tx, rx) = mpsc::channel::<Option<Event>>();
    // Subscribe before the first refresh so nothing falls between them.
    let mut child = events::spawn(&cli, session, tx)?;
    let result = follow(&cli, session, lead, reports.as_deref(), &rx);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn follow(
    cli: &rex::Cli,
    session: &str,
    lead: String,
    reports: Option<&Path>,
    rx: &mpsc::Receiver<Option<Event>>,
) -> Result<()> {
    let me = std::env::var("REX_BLOCK").ok().filter(|s| !s.is_empty());
    let mut model = Model::new(lead, me, now());
    let mut tracker = Tracker::default();
    model.refresh(cli, session, now())?;
    let mut out = std::io::stdout().lock();
    let mut emit = |lines: Vec<String>| -> Result<()> {
        for line in lines {
            writeln!(out, "{line}")?;
        }
        out.flush()?;
        Ok(())
    };
    emit(tracker.diff(&model.rows(now()), now(), reports))?;
    loop {
        if model.should_exit(now()) {
            return Ok(());
        }
        let event = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(event)) => event,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                emit(vec![
                    "rexroster events: the rex event stream ended".to_owned(),
                ])?;
                anyhow::bail!("the rex event stream ended");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let needs = model.apply(event, now());
        if needs.blocks {
            // A failed listing leaves the rows as they were; the next
            // structural event tries again.
            let _ = model.refresh(cli, session, now());
        }
        emit(tracker.diff(&model.rows(now()), now(), reports))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(block: &str, state: &str, msg: &str) -> Row {
        Row {
            block: block.to_owned(),
            label: format!("w-{block}"),
            shown: false,
            state: state.to_owned(),
            age: None,
            msg: msg.to_owned(),
            task: None,
        }
    }

    #[test]
    fn the_first_call_reports_only_blocked_workers() {
        let mut t = Tracker::default();
        let lines = t.diff(
            &[row("a", "done", ""), row("b", "blocked", "Allow touch?")],
            10,
            None,
        );
        assert_eq!(lines, ["w-b blocked: Allow touch?"]);
    }

    #[test]
    fn a_turn_gives_one_line_when_it_stops() {
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 10, None);
        assert!(t.diff(&[row("a", "working", "ls")], 11, None).is_empty());
        assert!(t.diff(&[row("a", "working", "date")], 12, None).is_empty());
        assert_eq!(t.diff(&[row("a", "done", "")], 13, None), ["w-a done"]);
        // done then idle is the same stop
        assert!(t.diff(&[row("a", "idle", "")], 14, None).is_empty());
    }

    #[test]
    fn blocked_and_closed_are_reported() {
        let mut t = Tracker::default();
        t.diff(&[row("a", "working", "")], 10, None);
        assert_eq!(
            t.diff(&[row("a", "blocked", "Bash: touch x")], 11, None),
            ["w-a blocked: Bash: touch x"]
        );
        assert!(t.diff(&[row("a", "working", "")], 12, None).is_empty());
        assert_eq!(t.diff(&[], 13, None), ["w-a closed"]);
    }

    #[test]
    fn a_lost_root_record_is_an_exit() {
        let mut t = Tracker::default();
        t.diff(&[row("a", "working", "")], 10, None);
        assert_eq!(
            t.diff(&[row("a", "starting", "")], 11, None),
            ["w-a exited"]
        );
    }

    #[test]
    fn a_new_worker_is_silent_until_ready() {
        let mut t = Tracker::default();
        t.diff(&[], 10, None);
        assert!(t.diff(&[row("a", "starting", "")], 11, None).is_empty());
        assert_eq!(t.diff(&[row("a", "idle", "")], 12, None), ["w-a ready"]);
    }

    fn reports_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rexroster-follow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reports_must_be_an_existing_directory() {
        let dir = reports_dir("check");
        assert!(check_reports(&dir).is_ok());
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        for bad in [dir.join("missing"), file] {
            let err = format!("{:#}", check_reports(&bad).unwrap_err());
            assert!(err.contains(&bad.display().to_string()), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stop_carries_this_turns_report() {
        let dir = reports_dir("this-turn");
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 0, Some(&dir));
        let turn = now();
        t.diff(&[row("a", "working", "")], turn, Some(&dir));
        let path = dir.join("w-a#1.md");
        std::fs::write(&path, "w-a#1 succeeded\nremaining: nothing\n").unwrap();
        assert_eq!(
            t.diff(&[row("a", "done", "")], turn, Some(&dir)),
            [format!("w-a done: w-a#1 succeeded ({})", path.display())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_report_is_not_this_turns() {
        let dir = reports_dir("older");
        std::fs::write(dir.join("w-a#1.md"), "w-a#1 succeeded\n").unwrap();
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 0, Some(&dir));
        let later = now() + 60;
        t.diff(&[row("a", "working", "")], later, Some(&dir));
        assert_eq!(
            t.diff(&[row("a", "idle", "")], later, Some(&dir)),
            ["w-a idle: no report"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_prompt_does_not_start_a_new_turn() {
        let dir = reports_dir("blocked");
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 0, Some(&dir));
        let turn = now();
        t.diff(&[row("a", "working", "")], turn, Some(&dir));
        std::fs::write(dir.join("w-a#1.md"), "w-a#1 failed\n").unwrap();
        t.diff(&[row("a", "blocked", "")], turn + 100, Some(&dir));
        t.diff(&[row("a", "working", "")], turn + 200, Some(&dir));
        let lines = t.diff(&[row("a", "done", "")], turn + 300, Some(&dir));
        assert!(
            lines[0].starts_with("w-a done: w-a#1 failed ("),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
