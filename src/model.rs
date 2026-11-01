//! The roster's in-memory model: every block in the session with its status
//! records, of which the rows are the blocks whose `meta` record names our lead.
//!
//! Status changes arrive complete in the event stream, so they need no calls.
//! Only structural events (a block created, the view changed) cost one
//! `session.list_blocks`, plus one `program_status` per block not seen before.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde_json::Value;

use crate::events::Event;
use crate::rex::{self, Rex};

/// How long a roster that has never had a worker waits before giving up.
/// `open` runs right after the first `spawn`, before its meta record lands.
pub const STARTUP_GRACE_SECS: u64 = 30;

/// One program-status record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub app: String,
    pub state: String,
    pub msg: Option<String>,
    /// Unix seconds, from the record's `updated_at`.
    pub updated_at: Option<u64>,
}

impl Record {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            app: v["app"].as_str().unwrap_or_default().to_owned(),
            state: v["state"].as_str()?.to_owned(),
            msg: v["msg"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            updated_at: v["updated_at"].as_str().and_then(parse_utc),
        })
    }
}

/// What a `meta` record carries, from `rexroster tag`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Meta {
    pub lead: String,
    pub task: Option<String>,
    /// The worker's working directory, when its record carries one.
    pub cwd: Option<String>,
}

impl Meta {
    fn from_record(r: &Record) -> Option<Self> {
        if r.app != "rexlead" {
            return None;
        }
        let v: Value = serde_json::from_str(r.msg.as_deref()?).ok()?;
        Some(Self {
            lead: v["lead"].as_str()?.to_owned(),
            task: v["task"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            cwd: v["cwd"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
    }
}

/// `msg` with the worker's `cwd` shortened: a leading `cd <cwd> && ` or
/// `cd <cwd>; ` (after an optional `Running `) is dropped, and the path
/// elsewhere becomes `.`. Only whole path segments match, so `/a/b` leaves
/// `/a/bc` alone.
pub fn strip_cwd(msg: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return msg.to_owned();
    }
    let (lead, rest) = match msg.strip_prefix("Running ") {
        Some(r) => ("Running ", r),
        None => ("", msg),
    };
    let rest = rest
        .strip_prefix("cd ")
        .and_then(|r| r.strip_prefix(cwd))
        .and_then(|r| r.strip_prefix(" && ").or_else(|| r.strip_prefix("; ")))
        .map_or(rest, str::trim_start);
    let path_char = |c: char| c.is_alphanumeric() || "_-.~".contains(c);
    let mut out = String::with_capacity(msg.len());
    out.push_str(lead);
    let mut last = 0;
    for (i, _) in rest.match_indices(cwd) {
        let end = i + cwd.len();
        let before = rest[..i].chars().next_back();
        let after = rest[end..].chars().next();
        if i < last || before.is_some_and(|c| path_char(c) || c == '/') {
            continue;
        }
        if after.is_some_and(path_char) {
            continue;
        }
        out.push_str(&rest[last..i]);
        out.push('.');
        last = end;
    }
    out.push_str(&rest[last..]);
    out
}

#[derive(Debug, Clone, Default)]
struct Block {
    label: String,
    placed: bool,
    records: HashMap<String, Record>,
    /// When the root record last changed state, in Unix seconds.
    since: Option<u64>,
    /// When a `block_created` event announced it, in Unix seconds.
    created: Option<u64>,
    /// Whether its records have been fetched (or it was seen before the roster started).
    fetched: bool,
}

impl Block {
    fn meta(&self) -> Option<Meta> {
        Meta::from_record(self.records.get("meta")?)
    }

    fn remove_record(&mut self, id: &str) {
        self.records.remove(id);
        if id.is_empty() {
            self.since = None;
        }
    }

    fn set_record(&mut self, id: String, record: Record, now: u64) {
        if id.is_empty() {
            let changed = self
                .records
                .get("")
                .is_none_or(|old| old.state != record.state);
            if changed {
                self.since = Some(record.updated_at.unwrap_or(now));
            }
        }
        self.records.insert(id, record);
    }
}

/// One line of the roster.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub block: String,
    pub label: String,
    pub shown: bool,
    /// The root record's state, or `starting` before it has one.
    pub state: String,
    /// Seconds in the current state, when known.
    pub age: Option<u64>,
    /// The root record's message, falling back to the task.
    pub msg: String,
    /// The task from the meta record.
    pub task: Option<String>,
}

/// What the model needs fetched after an event.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Needs {
    /// Labels and placement, from `session.list_blocks`.
    pub blocks: bool,
    /// The layout, from `session.view`, to find minimized workers.
    pub view: bool,
}

impl Needs {
    pub fn or(self, other: Needs) -> Needs {
        Needs {
            blocks: self.blocks || other.blocks,
            view: self.view || other.view,
        }
    }
}

pub struct Model {
    pub lead: String,
    /// The roster's own block, whose events are ignored.
    pub me: Option<String>,
    blocks: BTreeMap<String, Block>,
    started: u64,
    had_workers: bool,
}

impl Model {
    pub fn new(lead: String, me: Option<String>, now: u64) -> Self {
        Self {
            lead,
            me,
            blocks: BTreeMap::new(),
            started: now,
            had_workers: false,
        }
    }

    fn ignored(&self, block: &str) -> bool {
        block == self.lead || Some(block) == self.me.as_deref()
    }

    /// Apply one event. Returns what has to be fetched to finish the update.
    pub fn apply(&mut self, event: Event, now: u64) -> Needs {
        if event.block().is_some_and(|b| self.ignored(b)) {
            return Needs::default();
        }
        let needs = match event {
            Event::BlockCreated(block) => {
                self.blocks.entry(block).or_default().created = Some(now);
                Needs {
                    blocks: true,
                    ..Needs::default()
                }
            }
            Event::BlockChanged(_) => Needs {
                blocks: true,
                ..Needs::default()
            },
            Event::ViewChanged => Needs {
                blocks: true,
                view: true,
            },
            Event::BlockClosed(block) => {
                self.blocks.remove(&block);
                Needs::default()
            }
            Event::Status { block, id, record } => {
                self.blocks
                    .entry(block)
                    .or_default()
                    .set_record(id, record, now);
                Needs::default()
            }
            Event::StatusRemoved { block, id } => {
                if let Some(b) = self.blocks.get_mut(&block) {
                    b.remove_record(&id);
                }
                Needs::default()
            }
        };
        self.note_workers();
        needs
    }

    /// Bring labels and placement up to date from `session.list_blocks`, and
    /// fetch the status records of blocks not seen before.
    pub fn refresh(&mut self, rex: &dyn Rex, session: &str, now: u64) -> Result<()> {
        let list = rex::list_blocks(rex, session)?;
        let mut fresh = Vec::new();
        self.blocks.retain(|id, _| list.iter().any(|b| &b.id == id));
        for info in list {
            if self.ignored(&info.id) {
                continue;
            }
            let block = self.blocks.entry(info.id.clone()).or_default();
            if !block.fetched {
                block.fetched = true;
                fresh.push(info.id.clone());
            }
            block.label = info.label;
            block.placed = info.placed;
        }
        for id in fresh {
            // A block that closes between the two calls just has no records.
            let Ok(status) = rex.block_call(session, &id, "program_status") else {
                continue;
            };
            let block = self.blocks.get_mut(&id).expect("inserted above");
            for r in status["records"].as_array().into_iter().flatten() {
                let rid = r["id"].as_str().unwrap_or_default().to_owned();
                // An event may already have brought a newer record.
                if let Some(record) = Record::from_json(r)
                    && !block.records.contains_key(&rid)
                {
                    block.set_record(rid, record, now);
                }
            }
        }
        self.note_workers();
        Ok(())
    }

    fn note_workers(&mut self) {
        if !self.had_workers && self.workers().next().is_some() {
            self.had_workers = true;
        }
    }

    /// Whether a block's meta record names our lead. The lead and the
    /// roster are never in `blocks`, so this is all a worker needs.
    fn is_ours(&self, b: &Block) -> bool {
        b.meta().is_some_and(|m| m.lead == self.lead)
    }

    fn workers(&self) -> impl Iterator<Item = (&String, &Block)> {
        self.blocks.iter().filter(|(_, b)| self.is_ours(b))
    }

    pub fn rows(&self, now: u64) -> Vec<Row> {
        self.workers()
            .map(|(id, b)| {
                let root = b.records.get("");
                let meta = b.meta();
                let cwd = meta.as_ref().and_then(|m| m.cwd.clone());
                // The task gets the same rewrite as the message, so the
                // expanded view still sees them as equal and shows one.
                let task = meta.and_then(|m| m.task).map(|t| match &cwd {
                    Some(cwd) => strip_cwd(&t, cwd),
                    None => t,
                });
                Row {
                    block: id.clone(),
                    label: b.label.clone(),
                    shown: b.placed,
                    state: root.map_or_else(|| "starting".to_owned(), |r| r.state.clone()),
                    age: b.since.map(|s| now.saturating_sub(s)),
                    msg: {
                        let msg = root
                            .and_then(|r| r.msg.clone())
                            .or_else(|| task.clone())
                            .unwrap_or_default();
                        match &cwd {
                            Some(cwd) => strip_cwd(&msg, cwd),
                            None => msg,
                        }
                    },
                    task,
                }
            })
            .collect()
    }

    /// Our workers that sit in a minimized floating layer of `view` (a
    /// `session.view` snapshot). Minimizing a shown worker hides it, so these
    /// get detached.
    pub fn to_detach(&self, view: &Value) -> Vec<String> {
        rex::minimized_blocks(view)
            .into_iter()
            .filter(|id| self.blocks.get(id).is_some_and(|b| self.is_ours(b)))
            .collect()
    }

    pub fn is_shown(&self, block: &str) -> bool {
        self.blocks.get(block).is_some_and(|b| b.placed)
    }

    /// The roster is done once it has had workers and has none left, or when
    /// it never got one within the startup grace. A block created within the
    /// grace that hasn't sent a meta record yet may be a worker still starting,
    /// so it keeps the roster open.
    pub fn should_exit(&self, now: u64) -> bool {
        if self.had_workers {
            let starting = self.blocks.values().any(|b| {
                b.meta().is_none()
                    && b.created
                        .is_some_and(|t| now.saturating_sub(t) < STARTUP_GRACE_SECS)
            });
            self.workers().next().is_none() && !starting
        } else {
            now.saturating_sub(self.started) >= STARTUP_GRACE_SECS
        }
    }
}

/// Parse an RFC 3339 UTC time such as `2026-11-01T11:05:52.28996Z` into Unix seconds.
pub fn parse_utc(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(str::parse::<i64>);
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// A short duration: `45s`, `12m`, `3h`, `2d`.
pub fn short_age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events;
    use crate::rex::fake::Fake;
    use serde_json::json;

    const LEAD: &str = "block:lead";
    /// When the worker's first record landed in the fixture.
    const T0: u64 = 1_793_531_152;

    fn lifecycle() -> Vec<Event> {
        include_str!("../testdata/lifecycle.jsonl")
            .lines()
            .filter_map(events::parse)
            .collect()
    }

    fn status(block: &str, id: &str, app: &str, state: &str, msg: Option<&str>) -> Event {
        Event::Status {
            block: block.into(),
            id: id.into(),
            record: Record {
                app: app.into(),
                state: state.into(),
                msg: msg.map(Into::into),
                updated_at: None,
            },
        }
    }

    fn meta(block: &str, lead: &str, task: Option<&str>) -> Event {
        let msg = json!({ "lead": lead, "task": task }).to_string();
        status(block, "meta", "rexlead", "idle", Some(&msg))
    }

    const CWD: &str = "/home/u/proj/.worktrees/feat-x";

    #[test]
    fn strip_cwd_drops_a_leading_cd_and_shortens_paths() {
        // Prefix dropped, with `&&` or `;`, with and without `Running `.
        assert_eq!(
            strip_cwd(&format!("Running cd {CWD}; sed -n 1p a.rs"), CWD),
            "Running sed -n 1p a.rs"
        );
        assert_eq!(
            strip_cwd(&format!("cd {CWD} && cargo test"), CWD),
            "cargo test"
        );
        // A path inside the message becomes `.`.
        assert_eq!(
            strip_cwd(&format!("Reading {CWD}/src/main.rs"), CWD),
            "Reading ./src/main.rs"
        );
        assert_eq!(strip_cwd(&format!("ls {CWD} {CWD}/a"), CWD), "ls . ./a");
        assert_eq!(strip_cwd(&format!("cd {CWD}"), CWD), "cd .");
        // A trailing slash on the cwd makes no difference.
        assert_eq!(
            strip_cwd(&format!("ls {CWD}/a"), &format!("{CWD}/")),
            "ls ./a"
        );
    }

    #[test]
    fn strip_cwd_matches_whole_segments_only() {
        assert_eq!(
            strip_cwd("ls /a/bc /x/a/b /a/b-c /a/b", "/a/b"),
            "ls /a/bc /x/a/b /a/b-c ."
        );
        assert_eq!(
            strip_cwd(&format!("cd {CWD}x && ls"), CWD),
            format!("cd {CWD}x && ls")
        );
        assert_eq!(strip_cwd("ls /a", "/"), "ls /a");
        assert_eq!(strip_cwd("ls /a", ""), "ls /a");
    }

    #[test]
    fn rows_rewrite_the_cwd_only_when_the_meta_record_has_one() {
        let text = format!("Running cd {CWD}; ls");
        let working = |b: &str| status(b, "", "claude-code", "working", Some(&text));
        let mut m = Model::new(LEAD.into(), None, 0);
        let with = json!({ "lead": LEAD, "task": null, "cwd": CWD }).to_string();
        m.apply(status("block:a", "meta", "rexlead", "idle", Some(&with)), 0);
        m.apply(working("block:a"), 0);
        // An old record has no `cwd`: the message is left alone.
        m.apply(meta("block:b", LEAD, None), 0);
        m.apply(working("block:b"), 0);
        let rows = m.rows(0);
        let msg = |b: &str| rows.iter().find(|r| r.block == b).unwrap().msg.clone();
        assert_eq!(msg("block:a"), "Running ls");
        assert_eq!(msg("block:b"), text);
        // A task that is the fallback message stays equal to it.
        let t = format!("cd {CWD} && cargo test");
        let with = json!({ "lead": LEAD, "task": t, "cwd": CWD }).to_string();
        m.apply(status("block:c", "meta", "rexlead", "idle", Some(&with)), 0);
        let row = m
            .rows(0)
            .into_iter()
            .find(|r| r.block == "block:c")
            .unwrap();
        assert_eq!(row.msg, "cargo test");
        assert_eq!(row.task.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parses_utc_times() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-01T11:05:52.28996Z"), Some(T0));
        assert_eq!(parse_utc("2024-02-29T00:00:00+00:00"), Some(1_709_164_800));
        assert_eq!(parse_utc("2026-11-01T11:05:52+01:00"), None);
        assert_eq!(parse_utc("garbage"), None);
    }

    #[test]
    fn short_ages() {
        assert_eq!(short_age(5), "5s");
        assert_eq!(short_age(125), "2m");
        assert_eq!(short_age(7300), "2h");
        assert_eq!(short_age(200_000), "2d");
    }

    #[test]
    fn replays_the_recorded_lifecycle() {
        let mut m = Model::new(LEAD.into(), None, T0);
        let mut saw = Vec::new();
        for e in lifecycle() {
            m.apply(e, T0);
            if let Some(row) = m.rows(T0 + 100).first() {
                saw.push(row.state.clone());
            }
        }
        // Listed from the meta record on, through every state.
        for state in ["working", "blocked", "done", "idle"] {
            assert!(saw.iter().any(|s| s == state), "{state} in {saw:?}");
        }
        // The worker closed at the end, so the roster is done.
        assert!(m.rows(T0).is_empty());
        assert!(m.should_exit(T0));
    }

    #[test]
    fn lists_only_its_own_leads_workers() {
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, Some("mine")), 0);
        m.apply(meta("block:b", "block:other-lead", Some("theirs")), 0);
        m.apply(status("block:c", "", "claude-code", "working", None), 0);
        m.apply(
            status(
                "block:d",
                "meta",
                "something-else",
                "idle",
                Some("{\"lead\":\"x\"}"),
            ),
            0,
        );
        let rows = m.rows(0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].block, "block:a");
    }

    #[test]
    fn row_shows_state_age_and_msg_falling_back_to_task() {
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, Some("fix login")), 0);
        let row = &m.rows(0)[0];
        assert_eq!(row.state, "starting");
        assert_eq!(row.msg, "fix login");
        assert_eq!(row.age, None);

        m.apply(
            status(
                "block:a",
                "",
                "claude-code",
                "working",
                Some("Running tests"),
            ),
            100,
        );
        let row = &m.rows(130)[0];
        assert_eq!(row.state, "working");
        assert_eq!(row.msg, "Running tests");
        assert_eq!(row.age, Some(30));

        // A new message in the same state keeps the clock running.
        m.apply(
            status("block:a", "", "claude-code", "working", Some("Linting")),
            120,
        );
        assert_eq!(m.rows(130)[0].age, Some(30));
        // A new state restarts it.
        m.apply(status("block:a", "", "claude-code", "blocked", None), 125);
        let row = &m.rows(130)[0];
        assert_eq!(row.age, Some(5));
        assert_eq!(row.msg, "fix login");
    }

    #[test]
    fn ignores_its_own_and_the_leads_events() {
        let me = "block:roster";
        let mut m = Model::new(LEAD.into(), Some(me.into()), 0);
        assert_eq!(m.apply(Event::BlockCreated(me.into()), 0), Needs::default());
        assert_eq!(
            m.apply(Event::BlockChanged(LEAD.into()), 0),
            Needs::default()
        );
        m.apply(meta(me, LEAD, None), 0);
        m.apply(meta(LEAD, LEAD, None), 0);
        assert!(m.rows(0).is_empty());
        // Other blocks' structural events still ask for a refresh.
        assert_eq!(
            m.apply(Event::BlockCreated("block:w".into()), 0),
            Needs {
                blocks: true,
                view: false
            }
        );
        assert_eq!(
            m.apply(Event::ViewChanged, 0),
            Needs {
                blocks: true,
                view: true
            }
        );
    }

    #[test]
    fn status_events_need_no_calls() {
        let mut m = Model::new(LEAD.into(), None, 0);
        assert_eq!(
            m.apply(status("block:a", "", "claude-code", "working", None), 0),
            Needs::default()
        );
        assert_eq!(
            m.apply(Event::BlockClosed("block:a".into()), 0),
            Needs::default()
        );
    }

    #[test]
    fn waits_out_the_startup_grace_when_empty_from_the_start() {
        let m = Model::new(LEAD.into(), None, 1000);
        assert!(!m.should_exit(1000));
        assert!(!m.should_exit(1000 + STARTUP_GRACE_SECS - 1));
        assert!(m.should_exit(1000 + STARTUP_GRACE_SECS));
    }

    #[test]
    fn exits_once_the_last_worker_goes() {
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, None), 0);
        m.apply(meta("block:b", LEAD, None), 0);
        m.apply(Event::BlockClosed("block:a".into()), 1);
        assert!(!m.should_exit(1));
        m.apply(Event::BlockClosed("block:b".into()), 2);
        assert!(m.should_exit(2));
    }

    #[test]
    fn refresh_fetches_status_only_for_new_blocks() {
        let rex = Fake::default();
        rex.on(
            "session.list_blocks",
            json!({"blocks": [
                {"block_id": LEAD, "label": "lead", "placement": "placed"},
                {"block_id": "block:a", "label": "alpha", "placement": "detached"},
            ]}),
        );
        rex.on(
            "block:a program_status",
            json!({"records": [
                {"state": "working", "app": "claude-code", "msg": "Reading", "updated_at": "1970-01-01T00:00:10Z"},
                {"id": "meta", "state": "idle", "app": "rexlead", "msg": format!("{{\"lead\":\"{LEAD}\",\"task\":\"t\"}}")},
            ]}),
        );
        let mut m = Model::new(LEAD.into(), None, 0);
        m.refresh(&rex, "s", 20).unwrap();
        assert_eq!(
            m.rows(20),
            vec![Row {
                block: "block:a".into(),
                label: "alpha".into(),
                shown: false,
                state: "working".into(),
                age: Some(10),
                msg: "Reading".into(),
                task: Some("t".into()),
            }]
        );
        // The lead is never asked for its status.
        assert_eq!(
            rex.methods(),
            ["session.list_blocks", "block:a program_status"]
        );

        // A second refresh (say the block was shown) is a single call.
        rex.on(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:a", "label": "alpha", "placement": "placed"}]}),
        );
        m.refresh(&rex, "s", 30).unwrap();
        assert!(m.rows(30)[0].shown);
        assert_eq!(rex.methods().len(), 3);

        // A block missing from the list is gone.
        rex.on("session.list_blocks", json!({"blocks": []}));
        m.refresh(&rex, "s", 40).unwrap();
        assert!(m.rows(40).is_empty());
        assert!(m.should_exit(40));
    }

    #[test]
    fn replays_five_workers_spawned_and_closed() {
        let lead = "block:lead";
        let roster = "block:roster";
        let mut m = Model::new(lead.into(), Some(roster.into()), T0);
        let mut most = 0;
        let mut last_listed = 0;
        let mut first_exit = None;
        let events: Vec<_> = include_str!("../testdata/five-workers.jsonl")
            .lines()
            .filter_map(events::parse)
            .collect();
        for (i, e) in events.into_iter().enumerate() {
            m.apply(e, T0);
            let n = m.rows(T0).len();
            most = most.max(n);
            if n > 0 {
                last_listed = i;
            }
            if m.should_exit(T0) && first_exit.is_none() {
                first_exit = Some(i);
            }
        }
        assert_eq!(most, 5);
        // It doesn't exit while it still has workers, nor before the first arrives.
        assert_eq!(first_exit, Some(last_listed + 1));
        assert!(m.rows(T0).is_empty());
        assert!(m.should_exit(T0));
    }

    #[test]
    fn stays_open_for_a_spawned_worker_that_has_not_tagged_itself_yet() {
        // Review finding: A is listed, B is spawned (open sees the roster and
        // returns null), A closes before B's meta lands.
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, None), 0);
        m.apply(Event::BlockCreated("block:b".into()), 10);
        m.apply(Event::BlockClosed("block:a".into()), 11);
        assert!(!m.should_exit(11));
        m.apply(meta("block:b", LEAD, None), 12);
        assert_eq!(m.rows(12).len(), 1);
    }

    #[test]
    fn an_untagged_new_block_holds_the_roster_only_for_the_grace() {
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, None), 0);
        m.apply(Event::BlockCreated("block:shell".into()), 10);
        m.apply(Event::BlockClosed("block:a".into()), 11);
        assert!(!m.should_exit(10 + STARTUP_GRACE_SECS - 1));
        assert!(m.should_exit(10 + STARTUP_GRACE_SECS));
    }

    #[test]
    fn removing_the_root_record_clears_the_age() {
        let mut m = Model::new(LEAD.into(), None, 0);
        m.apply(meta("block:a", LEAD, None), 0);
        m.apply(status("block:a", "", "claude-code", "working", None), 0);
        m.apply(
            Event::StatusRemoved {
                block: "block:a".into(),
                id: String::new(),
            },
            600,
        );
        let row = &m.rows(600)[0];
        assert_eq!(row.state, "starting");
        assert_eq!(row.age, None);
    }

    #[test]
    fn detaches_only_its_own_minimized_workers() {
        let mut m = Model::new("block:lead".into(), Some("block:roster".into()), 0);
        for b in ["block:mini", "block:shown"] {
            m.apply(meta(b, "block:lead", None), 0);
        }
        m.apply(meta("block:theirs", "block:other-lead", None), 0);
        let view: Value =
            serde_json::from_str(include_str!("../testdata/view-minimized.json")).unwrap();
        assert_eq!(m.to_detach(&view), ["block:mini"]);
        // The minimized roster is never detached, even when its own meta
        // record names the lead: its events never reach the model.
        m.apply(meta("block:roster", "block:lead", None), 0);
        assert_eq!(m.to_detach(&view), ["block:mini"]);

        // After the detach the block is in no layer, so the next pass finds nothing.
        let after: Value = serde_json::from_str(
            &include_str!("../testdata/view-minimized.json")
                .replace("\"block:mini\"", "\"block:gone\""),
        )
        .unwrap();
        assert!(m.to_detach(&after).is_empty());
    }
}
