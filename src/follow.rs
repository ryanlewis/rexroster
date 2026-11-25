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
//! Starting work is silent, and a stop waits until it has lasted `SETTLE`
//! seconds. With a reports dir, the tracker's state is kept there so a
//! restart picks up where the last run stopped.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::events::{self, Event};
use crate::model::{Model, Row};
use crate::rex;

/// How long a worker has to stay stopped before its stop is printed: a
/// worker's root record goes idle for a moment while its subagents start and
/// finish, and each of those would otherwise wake the lead.
const SETTLE: u64 = 10;

/// The file in the reports dir that keeps the tracker's state across
/// restarts of `rexroster events`.
const STATE_FILE: &str = ".rexroster-events.json";

/// What the lead last knew about a worker.
#[derive(Debug, Clone)]
struct Seen {
    label: String,
    state: String,
    /// When its current turn started, in Unix seconds: reports older than
    /// this belong to an earlier turn.
    turn: u64,
    /// When its current stop started.
    since: u64,
    /// Whether its current stop was printed. A stop not yet printed is held.
    printed: bool,
    /// Whether its current stop is a new worker's first: worded `ready`.
    ready: bool,
}

impl Seen {
    fn held(&self) -> bool {
        stopped(&self.state) && !self.printed
    }
}

/// Diffs the model's rows into lines.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
    primed: bool,
    /// Loaded from a state file and not yet diffed.
    restored: bool,
}

fn stopped(state: &str) -> bool {
    matches!(state, "idle" | "done")
}

impl Tracker {
    /// The tracker the last run left in `path`, so a restart diffs against
    /// what it knew. A missing or unreadable file is a fresh start.
    pub fn load(path: &Path) -> Tracker {
        let seen = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| {
                v.as_object()?
                    .iter()
                    .map(|(block, w)| {
                        let s = |k: &str| w.get(k)?.as_str().map(str::to_owned);
                        let n = |k: &str| w.get(k)?.as_u64();
                        let b = |k: &str| w.get(k)?.as_bool();
                        let seen = Seen {
                            label: s("label")?,
                            state: s("state")?,
                            turn: n("turn")?,
                            since: n("since")?,
                            printed: b("printed")?,
                            ready: b("ready")?,
                        };
                        Some((block.clone(), seen))
                    })
                    .collect::<Option<HashMap<_, _>>>()
            });
        match seen {
            Some(seen) => Tracker {
                seen,
                primed: true,
                restored: true,
            },
            None => Tracker::default(),
        }
    }

    fn to_json(&self) -> String {
        let map: serde_json::Map<String, Value> = self
            .seen
            .iter()
            .map(|(block, w)| {
                let v = json!({
                    "label": w.label,
                    "state": w.state,
                    "turn": w.turn,
                    "since": w.since,
                    "printed": w.printed,
                    "ready": w.ready,
                });
                (block.clone(), v)
            })
            .collect();
        Value::Object(map).to_string()
    }

    /// Write the tracker to `path`, replacing it whole.
    fn save(path: &Path, json: &str) -> Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    /// The lines for the change from the last call to `rows`. A fresh
    /// tracker's first call only records where every worker stands, except
    /// that a worker already blocked or in error is reported. A stop is held
    /// until the worker has stayed stopped for `SETTLE` seconds; working
    /// again within that drops it.
    pub fn diff(&mut self, rows: &[Row], now: u64, reports: Option<&Path>) -> Vec<String> {
        let mut out = Vec::new();
        let fresh = !self.primed;
        let restored = std::mem::take(&mut self.restored);
        self.primed = true;
        for row in rows {
            let prev = self.seen.get(&row.block).cloned();
            let was = prev.as_ref().map(|p| p.state.clone());
            let mut w = prev.unwrap_or_else(|| Seen {
                label: row.label.clone(),
                state: String::new(),
                turn: now,
                since: now,
                printed: true,
                ready: false,
            });
            w.label.clone_from(&row.label);
            if was.as_deref() != Some(row.state.as_str()) {
                if stopped(&row.state) {
                    if !was.as_deref().is_some_and(stopped) {
                        // A fresh start already knows nothing to report.
                        w.since = now;
                        w.printed = fresh;
                        w.ready = was.as_deref().is_none_or(|s| s == "starting");
                    }
                } else {
                    let held = was.is_some() && w.held();
                    match row.state.as_str() {
                        // Back to work within the settle: the same turn.
                        "working" | "blocked" | "error" if held => {}
                        "working" if was.as_deref() != Some("blocked") => w.turn = now,
                        // The program ended: its stop stands.
                        _ if held => out.push(stop_line(&w, reports)),
                        _ => {}
                    }
                    w.printed = true;
                    if let Some(line) = word(row, was.as_deref(), fresh) {
                        out.push(line);
                    }
                }
                w.state.clone_from(&row.state);
            }
            if w.held() && now.saturating_sub(w.since) >= SETTLE {
                out.push(stop_line(&w, reports));
                w.printed = true;
            }
            self.seen.insert(row.block.clone(), w);
        }
        let mut gone: Vec<String> = self
            .seen
            .keys()
            .filter(|b| !rows.iter().any(|r| &r.block == *b))
            .cloned()
            .collect();
        gone.sort();
        for block in gone {
            let w = self.seen.remove(&block).expect("listed above");
            if w.held() {
                out.push(stop_line(&w, reports));
            }
            // A worker that closed while no watcher ran may have reported.
            let report = (restored && w.state == "working")
                .then(|| reports.and_then(|dir| report_line(dir, &w.label, w.turn)))
                .flatten();
            out.push(match report {
                Some(report) => format!("{} closed: {report}", w.label),
                None => format!("{} closed", w.label),
            });
        }
        out
    }
}

/// The line for a held stop.
fn stop_line(w: &Seen, reports: Option<&Path>) -> String {
    let name = &w.label;
    if w.ready {
        return format!("{name} ready");
    }
    let s = &w.state;
    match reports {
        Some(dir) => {
            let report = report_line(dir, name, w.turn);
            format!("{name} {s}: {}", report.as_deref().unwrap_or("no report"))
        }
        None => format!("{name} {s}"),
    }
}

/// The line for a worker leaving a stop or its start, if the lead needs one
/// at once.
fn word(row: &Row, was: Option<&str>, fresh: bool) -> Option<String> {
    let name = &row.label;
    match row.state.as_str() {
        "blocked" | "error" => Some(format!("{name} {}: {}", row.state, row.msg)),
        _ if fresh => None,
        // The root record went away: the worker's program exited, its block
        // stayed open.
        "starting" if was.is_some_and(|w| w != "starting") => Some(format!("{name} exited")),
        _ => None,
    }
}

/// The first line and path of the newest `<dir>/<name>#*.md` written at or
/// after `turn`, if there is one.
fn report_line(dir: &Path, name: &str, turn: u64) -> Option<String> {
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
    let (_, path) = newest?;
    let first = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.lines().next().map(str::to_owned))
        .unwrap_or_default();
    Some(format!("{first} ({})", path.display()))
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
    let state = reports.map(|dir| dir.join(STATE_FILE));
    let mut tracker = state.as_deref().map(Tracker::load).unwrap_or_default();
    let mut saved = String::new();
    model.refresh(cli, session, now())?;
    let mut out = std::io::stdout().lock();
    let mut step = |tracker: &mut Tracker, model: &Model| -> Result<()> {
        let now = now();
        for line in tracker.diff(&model.rows(now), now, reports) {
            writeln!(out, "{line}")?;
        }
        out.flush()?;
        if let Some(path) = &state {
            let json = tracker.to_json();
            // The state is a convenience for the next run: a failed write
            // must not stop this one.
            if json != saved && Tracker::save(path, &json).is_ok() {
                saved = json;
            }
        }
        Ok(())
    };
    step(&mut tracker, &model)?;
    loop {
        if model.should_exit(now()) {
            return Ok(());
        }
        let event = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(event)) => Some(event),
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                writeln!(
                    std::io::stdout(),
                    "rexroster events: the rex event stream ended"
                )?;
                anyhow::bail!("the rex event stream ended");
            }
            // A quiet second still lets a held stop settle.
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        };
        if let Some(event) = event
            && model.apply(event, now()).blocks
        {
            // A failed listing leaves the rows as they were; the next
            // structural event tries again.
            let _ = model.refresh(cli, session, now());
        }
        step(&mut tracker, &model)?;
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
        assert!(t.diff(&[row("a", "done", "")], 13, None).is_empty());
        // done then idle is the same stop
        assert!(t.diff(&[row("a", "idle", "")], 14, None).is_empty());
        assert_eq!(t.diff(&[row("a", "idle", "")], 23, None), ["w-a idle"]);
        assert!(t.diff(&[row("a", "idle", "")], 40, None).is_empty());
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
        assert!(t.diff(&[row("a", "idle", "")], 12, None).is_empty());
        assert_eq!(t.diff(&[row("a", "idle", "")], 22, None), ["w-a ready"]);
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
        t.diff(&[row("a", "done", "")], turn, Some(&dir));
        assert_eq!(
            t.diff(&[row("a", "done", "")], turn + SETTLE, Some(&dir)),
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
        t.diff(&[row("a", "idle", "")], later, Some(&dir));
        assert_eq!(
            t.diff(&[row("a", "idle", "")], later + SETTLE, Some(&dir)),
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
        t.diff(&[row("a", "done", "")], turn + 300, Some(&dir));
        let lines = t.diff(&[row("a", "done", "")], turn + 300 + SETTLE, Some(&dir));
        assert!(
            lines[0].starts_with("w-a done: w-a#1 failed ("),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stop_is_dropped_when_work_resumes_within_the_settle() {
        let mut t = Tracker::default();
        t.diff(&[row("a", "working", "")], 10, None);
        assert!(t.diff(&[row("a", "idle", "")], 11, None).is_empty());
        assert!(t.diff(&[row("a", "working", "")], 20, None).is_empty());
        assert!(t.diff(&[row("a", "working", "")], 40, None).is_empty());
        // a later stop settles from its own start
        assert!(t.diff(&[row("a", "done", "")], 41, None).is_empty());
        assert!(t.diff(&[row("a", "done", "")], 50, None).is_empty());
        assert_eq!(t.diff(&[row("a", "done", "")], 51, None), ["w-a done"]);
    }

    #[test]
    fn a_dropped_stop_keeps_its_turn() {
        let dir = reports_dir("dropped");
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 0, Some(&dir));
        let turn = now();
        t.diff(&[row("a", "working", "")], turn, Some(&dir));
        std::fs::write(dir.join("w-a#1.md"), "w-a#1 succeeded\n").unwrap();
        t.diff(&[row("a", "idle", "")], turn + 100, Some(&dir));
        t.diff(&[row("a", "working", "")], turn + 105, Some(&dir));
        t.diff(&[row("a", "done", "")], turn + 200, Some(&dir));
        let lines = t.diff(&[row("a", "done", "")], turn + 200 + SETTLE, Some(&dir));
        assert!(
            lines[0].starts_with("w-a done: w-a#1 succeeded ("),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_stop_is_printed_before_closed() {
        let mut t = Tracker::default();
        t.diff(&[row("a", "working", "")], 10, None);
        assert!(t.diff(&[row("a", "done", "")], 11, None).is_empty());
        assert_eq!(t.diff(&[], 12, None), ["w-a done", "w-a closed"]);
    }

    fn restart(t: &Tracker, dir: &Path) -> Tracker {
        let path = dir.join(STATE_FILE);
        Tracker::save(&path, &t.to_json()).unwrap();
        Tracker::load(&path)
    }

    #[test]
    fn a_restart_prints_a_gap_stop_and_a_gap_close() {
        let dir = reports_dir("gap");
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", ""), row("b", "idle", "")], 0, Some(&dir));
        let turn = now();
        t.diff(
            &[row("a", "working", ""), row("b", "working", "")],
            turn,
            Some(&dir),
        );
        let path = dir.join("w-b#1.md");
        std::fs::write(&path, "w-b#1 succeeded\n").unwrap();
        // no watcher runs while a stops and b closes
        let mut t = restart(&t, &dir);
        let later = turn + 1800;
        assert_eq!(
            t.diff(&[row("a", "done", "")], later, Some(&dir)),
            [format!("w-b closed: w-b#1 succeeded ({})", path.display())]
        );
        assert_eq!(
            t.diff(&[row("a", "done", "")], later + SETTLE, Some(&dir)),
            ["w-a done: no report"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_does_not_repeat_a_printed_stop() {
        let dir = reports_dir("repeat");
        let mut t = Tracker::default();
        t.diff(&[row("a", "working", "")], 10, Some(&dir));
        t.diff(&[row("a", "done", "")], 11, Some(&dir));
        assert_eq!(t.diff(&[row("a", "done", "")], 21, Some(&dir)).len(), 1);
        let mut t = restart(&t, &dir);
        assert!(t.diff(&[row("a", "done", "")], 30, Some(&dir)).is_empty());
        assert!(t.diff(&[row("a", "done", "")], 60, Some(&dir)).is_empty());
        assert_eq!(t.diff(&[], 61, Some(&dir)), ["w-a closed"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_carries_the_turn_start() {
        let dir = reports_dir("carry");
        let mut t = Tracker::default();
        t.diff(&[row("a", "idle", "")], 0, Some(&dir));
        let turn = now();
        t.diff(&[row("a", "working", "")], turn, Some(&dir));
        let path = dir.join("w-a#1.md");
        std::fs::write(&path, "w-a#1 succeeded\n").unwrap();
        let mut t = restart(&t, &dir);
        let later = turn + 60;
        assert!(
            t.diff(&[row("a", "working", "")], later, Some(&dir))
                .is_empty()
        );
        t.diff(&[row("a", "done", "")], later, Some(&dir));
        assert_eq!(
            t.diff(&[row("a", "done", "")], later + SETTLE, Some(&dir)),
            [format!("w-a done: w-a#1 succeeded ({})", path.display())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_unreadable_state_file_is_a_fresh_start() {
        let dir = reports_dir("fresh");
        let path = dir.join(STATE_FILE);
        let missing = Tracker::load(&path);
        std::fs::write(&path, "not json").unwrap();
        for mut t in [missing, Tracker::load(&path)] {
            assert_eq!(
                t.diff(
                    &[row("a", "done", ""), row("b", "blocked", "Allow?")],
                    10,
                    Some(&dir)
                ),
                ["w-b blocked: Allow?"]
            );
            let rows = [row("a", "done", ""), row("b", "blocked", "Allow?")];
            assert!(t.diff(&rows, 30, Some(&dir)).is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
