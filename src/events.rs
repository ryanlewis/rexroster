//! Parsing `rex events -s <session> --json`.
//!
//! Each line is `{"type":"session_event","payload":{"session_id":…,"event":E}}`.
//! `E` is `{"type":…,"payload":…}`. For block events, `E.type` is
//! `block_event` and the block's own event name is in `E.payload.name`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::Record;
use crate::rex;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    BlockCreated(String),
    BlockClosed(String),
    /// A block's label changed, or something else about it that `list_blocks` reports.
    BlockChanged(String),
    /// A program-status record was set. `id` is empty for the root record.
    Status {
        block: String,
        id: String,
        record: Record,
    },
    /// A program-status record was removed.
    StatusRemoved {
        block: String,
        id: String,
    },
    /// Windows, layers or placement changed.
    ViewChanged,
}

impl Event {
    /// The block the event is about, if any.
    pub fn block(&self) -> Option<&str> {
        match self {
            Event::BlockCreated(b)
            | Event::BlockClosed(b)
            | Event::BlockChanged(b)
            | Event::Status { block: b, .. }
            | Event::StatusRemoved { block: b, .. } => Some(b),
            Event::ViewChanged => None,
        }
    }
}

/// Parse one line. Lines that don't matter to the roster give `None`.
pub fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"].as_str()? != "session_event" {
        return None;
    }
    let event = &v["payload"]["event"];
    let payload = &event["payload"];
    let block = || payload["block_id"].as_str().map(str::to_owned);
    match event["type"].as_str()? {
        "block_created" => Some(Event::BlockCreated(block()?)),
        "block_closed" => Some(Event::BlockClosed(block()?)),
        "session_view_changed" => Some(Event::ViewChanged),
        "block_event" => block_event(block()?, payload),
        t if t.starts_with("block_label") => Some(Event::BlockChanged(block()?)),
        _ => None,
    }
}

fn block_event(block: String, payload: &Value) -> Option<Event> {
    let inner = &payload["payload"];
    let id = inner["id"].as_str().unwrap_or_default().to_owned();
    match payload["name"].as_str()? {
        "program_status_changed" => match Record::from_json(&inner["record"]) {
            Some(record) => Some(Event::Status { block, id, record }),
            None => Some(Event::StatusRemoved { block, id }),
        },
        "program_status_removed" | "program_status_cleared" => {
            Some(Event::StatusRemoved { block, id })
        }
        _ => None,
    }
}

/// Start `rex events` and send each parsed event, then `None` when the stream ends.
pub fn spawn<T>(cli: &rex::Cli, session: &str, tx: Sender<T>) -> Result<Child>
where
    T: From<Option<Event>> + Send + 'static,
{
    let mut child = cli
        .command()
        .args(["events", "-s", session, "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting rex events")?;
    let stdout = child.stdout.take().context("rex events has no stdout")?;
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(event) = parse(&line)
                && tx.send(Some(event).into()).is_err()
            {
                return;
            }
        }
        let _ = tx.send(None.into());
    });
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<Event> {
        let path = format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(parse)
            .collect()
    }

    #[test]
    fn parses_a_worker_lifecycle() {
        let events = fixture("lifecycle.jsonl");
        let worker = "block:w1";
        assert_eq!(events.first(), Some(&Event::BlockCreated(worker.into())));
        assert!(events.contains(&Event::BlockClosed(worker.into())));

        let meta = events
            .iter()
            .find_map(|e| match e {
                Event::Status { id, record, .. } if id == "meta" => Some(record),
                _ => None,
            })
            .expect("a meta record");
        assert_eq!(meta.app, "rexlead");
        assert!(meta.msg.as_deref().unwrap().contains("\"lead\""));

        let root_states: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Status { id, record, .. } if id.is_empty() => Some(record.state.as_str()),
                _ => None,
            })
            .collect();
        for state in ["working", "blocked", "done"] {
            assert!(root_states.contains(&state), "{state} in {root_states:?}");
        }
        assert!(events.contains(&Event::ViewChanged));
    }

    #[test]
    fn ignores_noise() {
        assert_eq!(parse("not json"), None);
        assert_eq!(
            parse(
                r#"{"type":"session_event","payload":{"event":{"type":"client_connected","payload":{}}}}"#
            ),
            None
        );
        assert_eq!(
            parse(
                r#"{"type":"session_event","payload":{"event":{"type":"block_event","payload":{"block_id":"b","name":"process_changed","payload":{}}}}}"#
            ),
            None
        );
    }

    #[test]
    fn a_status_without_a_record_is_a_removal() {
        let line = r#"{"type":"session_event","payload":{"event":{"type":"block_event","payload":{"block_id":"b","name":"program_status_changed","payload":{"id":"meta","record":null,"reason":"clear"}}}}}"#;
        assert_eq!(
            parse(line),
            Some(Event::StatusRemoved {
                block: "b".into(),
                id: "meta".into()
            })
        );
    }
}
