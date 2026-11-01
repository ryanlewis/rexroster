//! `rexroster tag`: mark this terminal as a lead's worker, then run the command.
//!
//! The mark is a program-status sub-record sent with OSC 7501. Only the
//! program in a block can set records on it, so the worker's launcher sends it.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The OSC 7501 sequence that records `{lead, task}` as the block's `meta` record.
///
/// Uses `idle` so the record never changes the block's visible state, and ST
/// (`ESC \`) as the terminator.
pub fn meta_record(lead: &str, task: Option<&str>) -> Vec<u8> {
    let json = serde_json::json!({ "lead": lead, "task": task }).to_string();
    let msg = STANDARD.encode(json);
    format!("\x1b]7501;state=idle:id=meta:app=rexlead:msg={msg}\x1b\\").into_bytes()
}

/// Write the meta record to the terminal, then replace this process with `cmd`.
/// Returns only when `cmd` can't be run.
///
/// The record goes to `/dev/tty` when it can be opened, so it never ends up in
/// a redirected stdout, and to stdout otherwise. Writing it is best-effort:
/// the command runs whether or not the tag landed.
pub fn run(lead: &str, task: Option<&str>, cmd: &[OsString]) -> Result<()> {
    let Some((program, args)) = cmd.split_first() else {
        bail!("tag needs a command after --");
    };
    let record = meta_record(lead, task);
    let wrote = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .and_then(|mut tty| tty.write_all(&record).and_then(|()| tty.flush()));
    if wrote.is_err() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&record).and_then(|()| out.flush());
    }
    let err = Command::new(program).args(args).exec();
    Err(err).with_context(|| format!("running {}", program.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(record: &[u8]) -> serde_json::Value {
        let s = std::str::from_utf8(record).unwrap();
        let msg = s
            .strip_prefix("\x1b]7501;state=idle:id=meta:app=rexlead:msg=")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("framing");
        serde_json::from_slice(&STANDARD.decode(msg).unwrap()).unwrap()
    }

    #[test]
    fn exact_bytes() {
        // base64 of {"lead":"L","task":"t"}
        assert_eq!(
            meta_record("L", Some("t")),
            b"\x1b]7501;state=idle:id=meta:app=rexlead:msg=eyJsZWFkIjoiTCIsInRhc2siOiJ0In0=\x1b\\"
        );
    }

    #[test]
    fn task_is_null_when_absent() {
        assert_eq!(
            decode(&meta_record("block:1", None)),
            serde_json::json!({ "lead": "block:1", "task": null })
        );
    }

    #[test]
    fn quotes_newlines_and_colons_survive() {
        let task = "fix \"login\"\nredirect: a=b; c";
        let v = decode(&meta_record("block:01a1", Some(task)));
        assert_eq!(v["task"], task);
        assert_eq!(v["lead"], "block:01a1");
    }
}
