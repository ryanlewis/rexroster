//! `rexroster watch`: float detached workers that are blocked on a prompt
//! into view, and sink them again when they move on.
//!
//! The work is done by a Lua script built into the binary, run with
//! `rex do -e <script> --all` so it hears every session on the server. The
//! script is passed as an argument rather than through stdin or a temp file:
//! nothing to clean up, nothing on disk for another process to swap, and
//! stdin stays free. It is a few kilobytes, well inside any argument limit.
//!
//! One watcher runs per user per Rex server. It holds a lock file for as long
//! as it runs, kept across the exec into `rex do`, and a second watcher that
//! finds the lock taken exits at once. The watcher runs until `rex do` ends:
//! when the Rex server goes away, or when it is killed.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::config::Notify;
use crate::lock::{self, Lock};

const SCRIPT: &str = include_str!("../lua/blocked-float.lua");

/// The script to run: a line that sets `NOTIFY`, then the built-in script.
pub fn script(notify: Notify) -> String {
    format!("NOTIFY = \"{}\"\n{SCRIPT}", notify.as_str())
}

/// The watcher's lock file for the current Rex server.
pub fn lock_path() -> PathBuf {
    lock_path_in(&lock::dir(), &lock::server())
}

fn lock_path_in(dir: &Path, server: &str) -> PathBuf {
    dir.join(format!("watch-{}.lock", lock::key(&[server])))
}

/// `rex do`'s arguments.
fn args(notify: Notify) -> Vec<String> {
    vec!["do".into(), "-e".into(), script(notify), "--all".into()]
}

/// Take the lock and become `rex do`. Returns `Ok(())` straight away if
/// another watcher holds the lock; otherwise returns only if exec fails.
pub fn run(mut rex: Command, notify: Notify, lock: &Path) -> Result<()> {
    let Some(held) = Lock::try_acquire(lock)? else {
        return Ok(());
    };
    held.keep_across_exec()?;
    // --all hears every session; drop the caller's session and block so
    // nothing narrows that.
    let err = rex
        .args(args(notify))
        .env_remove("REX_SESSION")
        .env_remove("REX_BLOCK")
        .exec();
    drop(held);
    Err(err).context("running rex do")
}

/// Start `<exe> watch` in its own session with no terminal, unless a watcher
/// already holds `lock`. Returns whether it started one. The check is only
/// there to save a process: the new watcher's own lock is what keeps it to one.
pub fn start(exe: &Path, lock: &Path) -> Result<bool> {
    match Lock::try_acquire(lock)? {
        None => return Ok(false),
        Some(probe) => drop(probe),
    }
    let mut cmd = Command::new(exe);
    // From /, so it doesn't hold on to a directory the caller later removes.
    cmd.arg("watch")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, as pre_exec needs.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("starting {} watch", exe.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preamble_sets_notify() {
        let s = script(Notify::None);
        assert!(
            s.starts_with("NOTIFY = \"none\"\n-- Float detached workers"),
            "{s}"
        );
        assert!(script(Notify::Cases).starts_with("NOTIFY = \"cases\"\n"));
        // The script reads the global and only calls cases when told to.
        assert!(SCRIPT.contains("local NOTIFY = NOTIFY or \"none\""));
        assert!(SCRIPT.contains("if NOTIFY ~= \"cases\" then return nil end"));
        assert_eq!(args(Notify::None)[..2], ["do", "-e"]);
        assert_eq!(args(Notify::None)[3], "--all");
    }

    #[test]
    fn one_lock_per_server() {
        let d = Path::new("/run/x");
        assert_eq!(lock_path_in(d, "a"), lock_path_in(d, "a"));
        assert_ne!(lock_path_in(d, "a"), lock_path_in(d, "b"));
        assert!(lock_path_in(d, "").starts_with("/run/x"));
    }

    #[test]
    fn run_exits_quietly_when_a_watcher_holds_the_lock() {
        let dir = lock::temp_dir("watch-run");
        let path = dir.join("watch.lock");
        let _held = Lock::try_acquire(&path).unwrap().unwrap();
        // The command would fail if it ran; it must not run.
        let rex = Command::new("/nonexistent/rex");
        run(rex, Notify::None, &path).unwrap();
        assert!(!start(Path::new("/nonexistent/rexroster"), &path).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
