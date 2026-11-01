//! `rexroster open` and `rexroster spawn`.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

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

/// Open the lead's roster unless one is already open. Returns the new layer's id.
pub fn open(rex: &dyn Rex, session: &str, lead: &str, exe: &Path) -> Result<Option<String>> {
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
    let exe = utf8(exe.as_os_str())?;
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
                "options": { "shell": "none", "command": [exe, "--lead", lead] },
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

/// Start a detached worker block, then open the roster. Returns the contract's JSON line.
pub fn spawn(
    rex: &dyn Rex,
    session: &str,
    spec: &Spawn,
    exe: &Path,
    path: Option<&OsString>,
) -> Result<Value> {
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
    let layer = open(rex, session, spec.lead, exe)?;
    Ok(json!({ "block_id": block, "roster_layer_id": layer }))
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

    const LEAD: &str = "block:01a196c9-510e-754b-9fac-c08f575f61f0";

    fn view() -> Value {
        json!({"windows": [{"window_id": "window:w", "layers": [
            {"layer_id": "layer:t", "kind": "tiled", "blocks": [{"block_id": LEAD}]}
        ]}]})
    }

    #[test]
    fn label_uses_the_last_six_characters() {
        assert_eq!(roster_label(LEAD), "roster-5f61f0");
        assert_eq!(roster_label("abc"), "roster-abc");
    }

    #[test]
    fn open_creates_the_layer_top_right_without_focus() {
        let rex = Fake::default();
        rex.on("session.list_blocks", json!({"blocks": []}));
        rex.on("session.view", view());
        rex.on("session.new_layer", json!({"layer_id": "layer:new"}));
        let id = open(&rex, "s", LEAD, Path::new("/bin/rexroster")).unwrap();
        assert_eq!(id.as_deref(), Some("layer:new"));
        let p = &rex.params("session.new_layer")[0];
        assert_eq!(p["window_id"], "window:w");
        assert_eq!(p["bounds"], json!(ROSTER_BOUNDS));
        assert_eq!(p["focus"], false);
        let block = &p["layout"]["block"];
        assert_eq!(block["label"], "roster-5f61f0");
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
            json!({"blocks": [{"block_id": "block:r", "label": "roster-5f61f0", "placement": "placed"}]}),
        );
        assert_eq!(open(&rex, "s", LEAD, Path::new("/x")).unwrap(), None);
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
        let out = spawn(&rex, "s", &spec, Path::new("/bin/rr"), Some(&path)).unwrap();
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
            json!({"blocks": [{"block_id": "block:r", "label": "roster-5f61f0"}]}),
        );
        let spec = Spawn {
            lead: LEAD,
            name: "w",
            task: None,
            cwd: None,
            cmd: &["true".into()],
        };
        let out = spawn(&rex, "s", &spec, Path::new("/bin/rr"), None).unwrap();
        assert_eq!(out, json!({"block_id": "block:w", "roster_layer_id": null}));
        assert!(
            rex.params("session.new_block")[0]["options"]
                .get("cwd")
                .is_none()
        );
    }
}
