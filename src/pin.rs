//! Keeping the lead's session name while its workers run.
//!
//! Workers are blocks in the lead's session, so a worker that renames "its"
//! session renames the lead's. `spawn` pins the session's label before the
//! first worker starts, and the roster puts it back when it changes. The pin
//! is a file next to the lock files, one per session and lead; the roster
//! removes it when its workers are gone, so a later rename sticks.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::lock;
use crate::rex::Rex;

/// The most renames the roster reverts in [`WINDOW_SECS`]. Past that,
/// someone is renaming on purpose, so it stops fighting until things settle.
pub const MAX_REVERTS: usize = 5;
pub const WINDOW_SECS: u64 = 60;

/// The pin file for a lead's session.
pub fn path(dir: &Path, session: &str, lead: &str) -> PathBuf {
    dir.join(format!(
        "pin-{}",
        lock::key(&[&lock::server(), session, lead])
    ))
}

/// The pinned label, if there is one.
pub fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|s| !s.is_empty())
}

/// Write the pin to a temp file beside it and rename it into place, so a
/// reader sees the old label or the new one, never part of one.
pub fn write(path: &Path, label: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        lock::make_dir(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let res = std::fs::write(&tmp, label)
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("writing {}", path.display()));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Drop the pin. A missing file is fine.
pub fn release(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The session's current label, from `session.view`.
pub fn session_label(rex: &dyn Rex, session: &str) -> Result<Option<String>> {
    let view = rex.call("session.view", json!({ "session_id": session }))?;
    Ok(view["label"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

pub fn rename(rex: &dyn Rex, session: &str, label: &str) -> Result<()> {
    rex.call(
        "session.set_label",
        json!({ "session_id": session, "label": label }),
    )?;
    Ok(())
}

/// What to do about a new session label.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Leave it: no pin, no live workers, or it is the pinned label (which
    /// is also what our own revert fires).
    Keep,
    /// Rename the session back to this.
    Revert(String),
    /// Too many reverts lately; leave this one and say so.
    GiveUp,
}

/// Decides reverts, and counts them so it never fights a person for long.
#[derive(Debug, Default)]
pub struct Guard {
    reverts: VecDeque<u64>,
}

impl Guard {
    pub fn decide(&mut self, label: &str, pin: Option<&str>, workers: bool, now: u64) -> Decision {
        let Some(pin) = pin.filter(|p| workers && *p != label) else {
            return Decision::Keep;
        };
        while self
            .reverts
            .front()
            .is_some_and(|&t| now.saturating_sub(t) >= WINDOW_SECS)
        {
            self.reverts.pop_front();
        }
        if self.reverts.len() >= MAX_REVERTS {
            return Decision::GiveUp;
        }
        self.reverts.push_back(now);
        Decision::Revert(pin.to_owned())
    }

    /// Don't count the last revert: it failed, so nobody was fought.
    pub fn forget_last(&mut self) {
        self.reverts.pop_back();
    }
}

/// `rexroster pin`: pin `label`, renaming the session to it, or with no
/// label pin the session's current one. Returns the pinned label.
pub fn run(
    rex: &dyn Rex,
    session: &str,
    lead: &str,
    label: Option<&str>,
    dir: &Path,
) -> Result<String> {
    let label = match label {
        Some(l) if !l.is_empty() => l.to_owned(),
        Some(_) => anyhow::bail!("the label is empty"),
        None => session_label(rex, session)?.context("the session has no label to pin")?,
    };
    // Pin first, so the roster sees our rename as the pinned label.
    let pin = path(dir, session, lead);
    let before = read(&pin);
    write(&pin, &label)?;
    let renamed = session_label(rex, session).and_then(|current| match current {
        Some(c) if c == label => Ok(()),
        _ => rename(rex, session, &label),
    });
    if let Err(err) = renamed {
        // The session keeps its old name, so it keeps its old pin too.
        match before {
            Some(b) => write(&pin, &b)?,
            None => release(&pin),
        }
        return Err(err);
    }
    Ok(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::fake::Fake;

    #[test]
    fn reverts_a_different_label_while_workers_run() {
        let mut g = Guard::default();
        assert_eq!(
            g.decide("worker-name", Some("lead"), true, 0),
            Decision::Revert("lead".into())
        );
    }

    #[test]
    fn keeps_the_pinned_label_so_its_own_revert_does_not_loop() {
        let mut g = Guard::default();
        assert_eq!(g.decide("lead", Some("lead"), true, 0), Decision::Keep);
    }

    #[test]
    fn keeps_any_label_without_live_workers_or_a_pin() {
        let mut g = Guard::default();
        assert_eq!(g.decide("x", Some("lead"), false, 0), Decision::Keep);
        assert_eq!(g.decide("x", None, true, 0), Decision::Keep);
    }

    #[test]
    fn gives_up_after_too_many_reverts_then_recovers() {
        let mut g = Guard::default();
        for i in 0..MAX_REVERTS {
            assert_eq!(
                g.decide("x", Some("lead"), true, i as u64),
                Decision::Revert("lead".into())
            );
        }
        assert_eq!(g.decide("x", Some("lead"), true, 10), Decision::GiveUp);
        // Matching labels never count.
        assert_eq!(g.decide("lead", Some("lead"), true, 10), Decision::Keep);
        // Once the first revert is a window old, one more is allowed.
        assert_eq!(
            g.decide("x", Some("lead"), true, WINDOW_SECS),
            Decision::Revert("lead".into())
        );
        assert_eq!(
            g.decide("x", Some("lead"), true, WINDOW_SECS),
            Decision::GiveUp
        );
    }

    #[test]
    fn pin_files_round_trip_and_release() {
        let dir = lock::temp_dir("pin");
        let p = path(&dir, "s", "L");
        assert_ne!(p, path(&dir, "s", "M"));
        assert_eq!(read(&p), None);
        write(&p, "my lead").unwrap();
        assert_eq!(read(&p).as_deref(), Some("my lead"));
        write(&p, "new").unwrap();
        assert_eq!(read(&p).as_deref(), Some("new"));
        // The temp file was renamed into place, not left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        release(&p);
        assert_eq!(read(&p), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pin_with_a_label_renames_and_without_adopts_the_current_one() {
        let dir = lock::temp_dir("pin-run");
        let rex = Fake::default();
        rex.on("session.view", json!({"label": "worker-name"}));
        assert_eq!(run(&rex, "s", "L", Some("mine"), &dir).unwrap(), "mine");
        assert_eq!(read(&path(&dir, "s", "L")).as_deref(), Some("mine"));
        assert_eq!(
            rex.params("session.set_label"),
            [json!({"session_id": "s", "label": "mine"})]
        );
        assert_eq!(run(&rex, "s", "L", None, &dir).unwrap(), "worker-name");
        assert_eq!(read(&path(&dir, "s", "L")).as_deref(), Some("worker-name"));
        // Already that label: no second rename.
        assert_eq!(rex.params("session.set_label").len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_rename_puts_the_old_pin_back() {
        let dir = lock::temp_dir("pin-run-fail");
        let p = path(&dir, "s", "L");
        let rex = Fake::default();
        rex.on("session.view", json!({"label": "worker-name"}));
        rex.fail("session.set_label");
        assert!(run(&rex, "s", "L", Some("mine"), &dir).is_err());
        assert_eq!(read(&p), None);
        write(&p, "old").unwrap();
        assert!(run(&rex, "s", "L", Some("mine"), &dir).is_err());
        assert_eq!(read(&p).as_deref(), Some("old"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
