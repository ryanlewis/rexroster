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
    format!("NOTIFY = \"{}\"\n{SCRIPT}", notify)
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
    use crate::events::{self, Event};

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
    }

    /// Events, as `rex events --json` lines, and what the watcher does with
    /// each: "float", "sink" or "ignore". The first lines were captured from
    /// Rex (testdata/program-status.jsonl); the rest are shapes src/events.rs
    /// reads that Rex has not been seen to send.
    fn parity_cases() -> Vec<(String, &'static str)> {
        let captured = include_str!("../testdata/program-status.jsonl");
        let expected = [
            "float",  // root blocked
            "sink",   // root cleared: ids [""]
            "ignore", // subagent set
            "ignore", // subagent cleared: ids ["sub"]
            "float",  // root blocked
            "ignore", // subagent set
            "sink",   // everything cleared: ids ["", "a"]
            "sink",   // root working
            "sink",   // program exited: ids [""], reason child_exited
            "sink",   // block closed
        ];
        assert_eq!(captured.lines().count(), expected.len());
        let status = |name: &str, payload: &str| {
            format!(
                r#"{{"type":"session_event","payload":{{"session_id":"session:test","event":{{"type":"block_event","payload":{{"block_id":"block:w1","name":"{name}","payload":{payload}}}}}}}}}"#
            )
        };
        let defensive = [
            (status("program_status_cleared", r#"{"ids":[""]}"#), "sink"),
            (status("program_status_cleared", "{}"), "sink"),
            (
                status("program_status_cleared", r#"{"ids":["a"]}"#),
                "ignore",
            ),
            (status("program_status_removed", "{}"), "sink"),
            (
                status("program_status_changed", r#"{"id":"","record":null}"#),
                "sink",
            ),
            (
                status("program_status_changed", r#"{"record":null}"#),
                "sink",
            ),
            (
                status("program_status_changed", r#"{"id":"meta","record":null}"#),
                "ignore",
            ),
            (
                status("program_status_changed", r#"{"id":"","record":{}}"#),
                "sink",
            ),
        ];
        captured
            .lines()
            .map(str::to_owned)
            .zip(expected)
            .chain(defensive)
            .collect()
    }

    /// The watcher's rule, applied to what src/events.rs makes of a line: it
    /// acts only on the root record (id ""), and a removal sinks the block
    /// only if it names the root.
    fn watcher_action(line: &str) -> &'static str {
        match events::parse(line) {
            Some(Event::Status { id, record, .. }) if id.is_empty() => {
                if record.state == "blocked" {
                    "float"
                } else {
                    "sink"
                }
            }
            Some(Event::StatusRemoved { ids, .. }) if ids.iter().any(String::is_empty) => "sink",
            Some(Event::BlockClosed(_)) => "sink",
            _ => "ignore",
        }
    }

    #[test]
    fn events_rs_and_the_watcher_agree_on_status_events() {
        for (line, want) in parity_cases() {
            assert_eq!(watcher_action(&line), want, "{line}");
        }
    }

    /// Runs the real Lua through `rex do -e`, with the rex API stubbed, over
    /// the same cases. Each case runs once from rest (does it float?) and once
    /// after a float (does it sink?).
    #[test]
    #[ignore = "needs rex and a running Rex server: cargo test -- --ignored"]
    fn the_lua_watcher_agrees_with_events_rs() {
        let cases = parity_cases();
        let lines: Vec<_> = cases.iter().map(|(l, _)| l.as_str()).collect();
        let lines = serde_json::to_string(&lines).unwrap();
        assert!(!lines.contains("]==]"));
        let lua = format!(
            r#"
local real, real_print = rex, print
local handlers, log = {{}}, {{}}
local function ok() return {{}} end
rex = setmetatable({{
  on = function(name, f) handlers[name] = f end,
  log = function() end,
  session = {{
    describe_block = function() return {{ block = {{ placement = "detached", label = "w" }} }} end,
    view = function()
      return {{ active_window_id = "w", windows = {{ {{ window_id = "w",
        layers = {{ {{ kind = "tiled", blocks = {{ {{ block_id = "anchor" }} }} }} }} }} }} }}
    end,
    move_block = ok, float_block = ok, detach_block = ok,
  }},
}}, {{ __index = real }})
print = function(s) log[#log + 1] = s end
{script}
local out = {{}}
for _, line in ipairs(rex.json.decode([==[{lines}]==])) do
  local e = rex.json.decode(line).payload.event
  local ctx = {{ session_id = "session:test", block_id = e.payload.block_id }}
  local name, ev = e.type, e.payload
  if name == "block_event" then name, ev = "terminal." .. e.payload.name, e.payload.payload end
  local function after(floated)
    handlers.block_closed(ctx, {{ block_id = ctx.block_id }})
    if floated then
      handlers["terminal.program_status_changed"](ctx, {{ id = "", record = {{ state = "blocked" }} }})
    end
    log = {{}}
    if handlers[name] then handlers[name](ctx, ev) end
    return log[1] or ""
  end
  local action = "ignore"
  if after(false):find("^floated") then action = "float"
  elseif after(true):find("^sank") then action = "sink" end
  out[#out + 1] = action
end
real_print("PARITY " .. rex.json.encode(out))
"#,
            script = script(Notify::None),
        );
        let out = crate::rex::Cli::new()
            .command()
            .args(["do", "-e", &lua])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = stdout
            .lines()
            .find_map(|l| l.strip_prefix("PARITY "))
            .unwrap_or_else(|| panic!("no result in {stdout}"));
        let got: Vec<String> = serde_json::from_str(got).unwrap();
        assert_eq!(got.len(), cases.len());
        for ((line, want), got) in cases.iter().zip(&got) {
            assert_eq!(got, want, "{line}");
        }
    }
}
