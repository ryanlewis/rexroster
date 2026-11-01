//! `rexroster open` and `rexroster spawn`.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::lock::{self, Lock};
use crate::rex::{self, Rex, SHELL_FLAVOR};

/// Where the roster opens: top right of the lead's tab.
pub const ROSTER_BOUNDS: [f64; 4] = [0.72, 0.02, 0.27, 0.12];

/// The roster's block label: `roster-` and the last 6 characters of the lead's id.
pub fn roster_label(lead: &str) -> String {
    let tail: String = lead
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("roster-{tail}")
}

/// The lock that `open` holds while it checks for the roster and creates it.
pub fn open_lock(dir: &Path, session: &str, lead: &str) -> std::path::PathBuf {
    dir.join(format!(
        "open-{}.lock",
        lock::key(&[&lock::server(), session, &roster_label(lead)])
    ))
}

/// Open the lead's roster unless one is already open. Returns the new layer's id.
/// `flags` are passed on to the roster's command line after `--lead <lead>`.
/// A lock in `lock_dir` keeps two opens at once from both creating a roster.
pub fn open(
    rex: &dyn Rex,
    session: &str,
    lead: &str,
    exe: &Path,
    flags: &[String],
    lock_dir: &Path,
) -> Result<Option<String>> {
    let _lock = Lock::acquire(&open_lock(lock_dir, session, lead))?;
    let label = roster_label(lead);
    if rex::list_blocks(rex, session)?
        .iter()
        .any(|b| b.label == label)
    {
        return Ok(None);
    }
    let view = rex.call("session.view", json!({ "session_id": session }))?;
    let window = rex::window_of(&view, lead)
        .with_context(|| format!("lead {lead} is not in a window of this session"))?;
    let mut command = vec![utf8(exe.as_os_str())?, "--lead".to_owned(), lead.to_owned()];
    command.extend(flags.iter().cloned());
    let out = rex.call(
        "session.new_layer",
        json!({
            "session_id": session,
            "window_id": window,
            "bounds": ROSTER_BOUNDS,
            "focus": false,
            "layout": { "block": {
                "flavor": SHELL_FLAVOR,
                "label": label,
                "options": { "shell": "none", "command": command },
            }},
        }),
    )?;
    Ok(out["layer_id"].as_str().map(str::to_owned))
}

/// The worker block's command: `<exe> tag <lead> [--task <task>] -- <cmd...>`.
pub fn worker_command(
    exe: &Path,
    lead: &str,
    task: Option<&str>,
    cmd: &[OsString],
) -> Result<Vec<String>> {
    let mut argv = vec![utf8(exe.as_os_str())?, "tag".to_owned(), lead.to_owned()];
    if let Some(task) = task {
        argv.extend(["--task".to_owned(), task.to_owned()]);
    }
    argv.push("--".to_owned());
    for arg in cmd {
        argv.push(utf8(arg)?);
    }
    Ok(argv)
}

pub struct Spawn<'a> {
    pub lead: &'a str,
    pub name: &'a str,
    pub task: Option<&'a str>,
    pub cwd: Option<&'a Path>,
    pub cmd: &'a [OsString],
}

pub struct Spawned {
    /// The JSON line spawn prints: `{"block_id":…,"roster_layer_id":…}`.
    pub json: Value,
    /// Why the roster couldn't be opened. The worker is running regardless.
    pub open_error: Option<String>,
}

/// Start a detached worker block, then open the roster.
pub fn spawn(
    rex: &dyn Rex,
    session: &str,
    spec: &Spawn,
    exe: &Path,
    path: Option<&OsString>,
    lock_dir: &Path,
) -> Result<Spawned> {
    let mut options = json!({
        // No shell: it would print a prompt, and Rex drops idle records at a
        // new prompt, which would lose the meta record.
        "shell": "none",
        "command": worker_command(exe, spec.lead, spec.task, spec.cmd)?,
    });
    // Without a shell the block gets only the server's minimal PATH, so pass
    // ours on for the worker to find its tools.
    if let Some(path) = path {
        let dirs: Vec<String> = std::env::split_paths(path)
            .filter_map(|p| p.to_str().map(str::to_owned))
            .filter(|p| !p.is_empty())
            .collect();
        options["path_prepend"] = json!(dirs);
    }
    if let Some(cwd) = spec.cwd {
        let cwd = std::path::absolute(cwd).context("resolving --cwd")?;
        options["cwd"] = json!(utf8(cwd.as_os_str())?);
    }
    let out = rex.call(
        "session.new_block",
        json!({
            "session_id": session,
            "flavor": SHELL_FLAVOR,
            "label": spec.name,
            "options": options,
        }),
    )?;
    let block = out["block_id"]
        .as_str()
        .context("session.new_block returned no block id")?;
    // The block exists now, so its id must reach the caller even if open fails.
    let (layer, open_error) = match open(rex, session, spec.lead, exe, &[], lock_dir) {
        Ok(layer) => (layer, None),
        Err(err) => (None, Some(format!("{err:#}"))),
    };
    Ok(Spawned {
        json: json!({ "block_id": block, "roster_layer_id": layer }),
        open_error,
    })
}

fn utf8(s: &std::ffi::OsStr) -> Result<String> {
    s.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not valid UTF-8", s.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::fake::Fake;

    const LEAD: &str = "block:0000-lead-a1b2c3";

    /// A lock directory for one test, removed when the test ends.
    struct Locks(std::path::PathBuf);

    impl std::ops::Deref for Locks {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Locks {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn locks() -> Locks {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Locks(
            std::env::temp_dir().join(format!("rexroster-launch-test-{}-{n}", std::process::id())),
        )
    }

    fn view() -> Value {
        json!({"windows": [{"window_id": "window:w", "layers": [
            {"layer_id": "layer:t", "kind": "tiled", "blocks": [{"block_id": LEAD}]}
        ]}]})
    }

    #[test]
    fn label_uses_the_last_six_characters() {
        assert_eq!(roster_label(LEAD), "roster-a1b2c3");
        assert_eq!(roster_label("abc"), "roster-abc");
    }

    #[test]
    fn open_creates_the_layer_top_right_without_focus() {
        let rex = Fake::default();
        rex.on("session.list_blocks", json!({"blocks": []}));
        rex.on("session.view", view());
        rex.on("session.new_layer", json!({"layer_id": "layer:new"}));
        let id = open(&rex, "s", LEAD, Path::new("/bin/rexroster"), &[], &locks()).unwrap();
        assert_eq!(id.as_deref(), Some("layer:new"));
        let p = &rex.params("session.new_layer")[0];
        assert_eq!(p["window_id"], "window:w");
        assert_eq!(p["bounds"], json!(ROSTER_BOUNDS));
        assert_eq!(p["focus"], false);
        let block = &p["layout"]["block"];
        assert_eq!(block["label"], "roster-a1b2c3");
        assert_eq!(
            block["options"]["command"],
            json!(["/bin/rexroster", "--lead", LEAD])
        );
    }

    #[test]
    fn open_is_idempotent() {
        let rex = Fake::default();
        rex.on(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:r", "label": "roster-a1b2c3", "placement": "placed"}]}),
        );
        assert_eq!(
            open(&rex, "s", LEAD, Path::new("/x"), &[], &locks()).unwrap(),
            None
        );
        assert_eq!(rex.methods(), ["session.list_blocks"]);
    }

    #[test]
    fn worker_command_wraps_with_tag() {
        let cmd: Vec<OsString> = vec!["claude".into(), "--flag".into()];
        assert_eq!(
            worker_command(Path::new("/bin/rr"), "L", Some("fix it"), &cmd).unwrap(),
            [
                "/bin/rr", "tag", "L", "--task", "fix it", "--", "claude", "--flag"
            ]
        );
        assert_eq!(
            worker_command(Path::new("/bin/rr"), "L", None, &cmd[..1]).unwrap(),
            ["/bin/rr", "tag", "L", "--", "claude"]
        );
    }

    #[test]
    fn spawn_creates_a_detached_block_then_opens() {
        let rex = Fake::default();
        rex.on("session.new_block", json!({"block_id": "block:w"}));
        rex.on("session.list_blocks", json!({"blocks": []}));
        rex.on("session.view", view());
        rex.on("session.new_layer", json!({"layer_id": "layer:r"}));
        let cmd: Vec<OsString> = vec!["sleep".into(), "1".into()];
        let spec = Spawn {
            lead: LEAD,
            name: "w1",
            task: Some("t"),
            cwd: Some(Path::new("/tmp")),
            cmd: &cmd,
        };
        let path = OsString::from("/opt/bin:/usr/bin");
        let out = spawn(
            &rex,
            "s",
            &spec,
            Path::new("/bin/rr"),
            Some(&path),
            &locks(),
        )
        .unwrap()
        .json;
        assert_eq!(
            out,
            json!({"block_id": "block:w", "roster_layer_id": "layer:r"})
        );
        let p = &rex.params("session.new_block")[0];
        assert_eq!(p["flavor"], SHELL_FLAVOR);
        assert_eq!(p["label"], "w1");
        assert_eq!(p["options"]["shell"], "none");
        assert_eq!(p["options"]["cwd"], "/tmp");
        assert_eq!(
            p["options"]["path_prepend"],
            json!(["/opt/bin", "/usr/bin"])
        );
        assert_eq!(
            p["options"]["command"],
            json!(["/bin/rr", "tag", LEAD, "--task", "t", "--", "sleep", "1"])
        );
    }

    #[test]
    fn spawn_prints_null_when_the_roster_is_already_open() {
        let rex = Fake::default();
        rex.on("session.new_block", json!({"block_id": "block:w"}));
        rex.on(
            "session.list_blocks",
            json!({"blocks": [{"block_id": "block:r", "label": "roster-a1b2c3"}]}),
        );
        let spec = Spawn {
            lead: LEAD,
            name: "w",
            task: None,
            cwd: None,
            cmd: &["true".into()],
        };
        let out = spawn(&rex, "s", &spec, Path::new("/bin/rr"), None, &locks()).unwrap();
        assert_eq!(
            out.json,
            json!({"block_id": "block:w", "roster_layer_id": null})
        );
        assert!(out.open_error.is_none());
        assert!(
            rex.params("session.new_block")[0]["options"]
                .get("cwd")
                .is_none()
        );
    }

    #[test]
    fn spawn_still_reports_the_block_when_open_fails() {
        // The lead is not in a window, so open fails after
        // the worker block already exists.
        let rex = Fake::default();
        rex.on("session.new_block", json!({"block_id": "block:w"}));
        rex.on("session.list_blocks", json!({"blocks": []}));
        rex.on("session.view", json!({"windows": []}));
        let spec = Spawn {
            lead: LEAD,
            name: "w",
            task: None,
            cwd: None,
            cmd: &["true".into()],
        };
        let out = spawn(&rex, "s", &spec, Path::new("/bin/rr"), None, &locks()).unwrap();
        assert_eq!(
            out.json,
            json!({"block_id": "block:w", "roster_layer_id": null})
        );
        assert!(out.open_error.unwrap().contains("not in a window"));
    }

    #[test]
    fn open_passes_display_flags_to_the_roster() {
        let rex = Fake::default();
        rex.on("session.list_blocks", json!({"blocks": []}));
        rex.on("session.view", view());
        let flags = ["--border".to_owned(), "rounded".to_owned()];
        open(&rex, "s", LEAD, Path::new("/bin/rr"), &flags, &locks()).unwrap();
        assert_eq!(
            rex.params("session.new_layer")[0]["layout"]["block"]["options"]["command"],
            json!(["/bin/rr", "--lead", LEAD, "--border", "rounded"])
        );
    }

    /// A server whose blocks are shared state, slow to list, so two opens
    /// at once both see "no roster" unless something orders them.
    #[derive(Default)]
    struct Slow {
        labels: std::sync::Mutex<Vec<String>>,
    }

    impl Rex for Slow {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            Ok(match method {
                "session.list_blocks" => {
                    let blocks: Vec<Value> = self
                        .labels
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|l| json!({"block_id": "block:r", "label": l}))
                        .collect();
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    json!({ "blocks": blocks })
                }
                "session.view" => view(),
                "session.new_layer" => {
                    let label = params["layout"]["block"]["label"].as_str().unwrap();
                    self.labels.lock().unwrap().push(label.to_owned());
                    json!({"layer_id": "layer:new"})
                }
                other => panic!("unexpected {other}"),
            })
        }

        fn block_call(&self, _: &str, _: &str, _: &str) -> Result<Value> {
            unreachable!()
        }
    }

    #[test]
    fn two_opens_at_once_make_one_roster() {
        let dir = locks();
        let rex = Slow::default();
        let opened: Vec<Option<String>> = std::thread::scope(|s| {
            let a = s.spawn(|| open(&rex, "s", LEAD, Path::new("/x"), &[], &dir).unwrap());
            let b = s.spawn(|| open(&rex, "s", LEAD, Path::new("/x"), &[], &dir).unwrap());
            vec![a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(*rex.labels.lock().unwrap(), ["roster-a1b2c3"]);
        assert_eq!(opened.iter().filter(|o| o.is_some()).count(), 1);
    }
}
