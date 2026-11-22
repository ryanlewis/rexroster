//! `rexroster lua`: the Lua snippet that adds rexroster's actions to Rex's
//! command palette. The user saves it and loads it from Rex's init.lua.

use std::path::Path;

const SCRIPT: &str = include_str!("../lua/palette.lua");

/// The snippet: a line that sets `REXROSTER` to `exe`, so the actions run
/// this binary, then the built-in script. Without a usable path the line is
/// left out and the actions find rexroster on `PATH`.
pub fn script(exe: Option<&Path>) -> String {
    match exe.and_then(Path::to_str) {
        Some(exe) => format!("local REXROSTER = {}\n{SCRIPT}", lua_string(exe)),
        None => SCRIPT.to_owned(),
    }
}

/// `s` as a Lua 5.1 string literal. Quotes, backslashes and control
/// characters are escaped; other bytes are fine as they are.
fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u8)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snippet_defines_both_actions() {
        for name in [
            "name = \"roster_rename_session\"",
            "title = \"Roster: Rename session…\"",
            "name = \"roster_show_worker\"",
            "title = \"Roster: Show worker…\"",
        ] {
            assert!(SCRIPT.contains(name), "no {name}");
        }
        // The script reads the preamble's local and falls back to PATH.
        assert!(SCRIPT.contains("local REXROSTER = REXROSTER or \"rexroster\""));
        // It floats a worker where the roster does.
        let [x, y, w, h] = crate::tui::SHOWN_BOUNDS;
        let bounds = format!("local SHOWN_BOUNDS = {{ {x}, {y}, {w}, {h} }}");
        assert!(SCRIPT.contains(&bounds), "no {bounds}");
    }

    #[test]
    fn the_preamble_names_this_binary() {
        let s = script(Some(Path::new("/opt/bin/rexroster")));
        assert!(
            s.starts_with("local REXROSTER = \"/opt/bin/rexroster\"\n-- rexroster's"),
            "{s}"
        );
        assert_eq!(script(None), SCRIPT);
    }

    /// Runs one action with `rex do <file> --action`, the snippet loaded
    /// behind a stub of the layout and `rex.exec` that prints each call as
    /// `CALL [method, args]`. With `workers` false the session has none.
    /// Returns whether it succeeded, the calls, and the action's result.
    fn run_action(workers: &str, action: &str, arg: &str) -> (bool, Vec<String>, String) {
        let stub = format!(
            r#"
local real = rex
local WORKERS = "{workers}"
local function call(method, args)
  print("CALL " .. real.json.encode({{ method, args }}))
  return {{}}
end
local function meta(lead, task)
  return {{ id = "meta", app = "rexlead", state = "idle",
    msg = real.json.encode({{ lead = lead, task = task }}) }}
end
local T = "com.superlogical.terminal"
local blocks = {{
  {{ block_id = "block:lead", label = "lead", placement = "placed", creator_name = T }},
  {{ block_id = "block:w1", label = "fix-login", placement = "detached", creator_name = T }},
  {{ block_id = "block:w2", label = "docs", placement = "placed", creator_name = T }},
  {{ block_id = "block:web", label = "web", placement = "placed", creator_name = "com.example.web" }},
}}
local status = {{
  ["block:lead"] = {{ records = {{ {{ app = "claude-code", state = "working" }} }} }},
  ["block:w1"] = {{ records = {{ {{ app = "claude-code", state = "idle" }},
    {{ id = "sub", app = "claude-code", state = "blocked" }}, meta("block:lead", "fix it") }} }},
  ["block:w2"] = {{ records = {{ meta("block:lead", nil) }} }},
}}
if WORKERS == "none" then status["block:w1"], status["block:w2"] = nil, nil end
if WORKERS == "two" then
  blocks[#blocks + 1] = {{ block_id = "block:lead2", label = "lead2", placement = "placed", creator_name = T }}
  blocks[#blocks + 1] = {{ block_id = "block:w3", label = "w3", placement = "detached", creator_name = T }}
  status["block:w3"] = {{ records = {{ meta("block:lead2", nil) }} }}
end
rex = setmetatable({{
  log = function() end,
  call = function() return {{ listeners = {{ {{ primary = true, endpoint = "unix:///s" }} }} }} end,
  exec = function(argv, opts) call("exec", {{ argv = argv, env = opts.env }}) return {{ ok = true, code = 0, stdout = "", stderr = "" }} end,
  block = {{ call = function(creator, method, args)
    assert(creator == T and method == "program_status")
    return status[args.block_id] or {{ records = {{}} }}
  end }},
  session = setmetatable({{
    list_blocks = function() return {{ blocks = blocks }} end,
    view = function() return {{ label = "old" }} end,
  }}, {{ __index = function(_, method) return function(args) return call(method, args) end end }}),
}}, {{ __index = real }})
"#
        );
        let dir = crate::lock::temp_dir(&format!("palette-{action}-{workers}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("palette.lua");
        let exe = Path::new("/opt/bin/rexroster");
        std::fs::write(&file, format!("{stub}\n{}", script(Some(exe)))).unwrap();
        let out = crate::rex::Cli::new()
            .command()
            .arg("do")
            .arg(&file)
            .args(["--action", action, arg])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut calls = Vec::new();
        let mut result = String::new();
        for line in stdout.lines() {
            match line.strip_prefix("CALL ") {
                Some(c) => calls.push(c.to_owned()),
                None => result.push_str(line),
            }
        }
        if result.is_empty() {
            result = String::from_utf8_lossy(&out.stderr).into_owned();
        }
        (out.status.success(), calls, result)
    }

    #[test]
    #[ignore = "needs rex and a running Rex server: cargo test -- --ignored"]
    fn the_actions_run_without_a_person() {
        let env = |k| std::env::var(k).unwrap_or_default();

        // A lead with workers: the rename goes through its pin.
        let (ok, calls, result) = run_action("one", "roster_rename_session", "name= new name ");
        assert!(ok, "{result}");
        let pin = serde_json::json!(["exec", {
            "argv": ["/opt/bin/rexroster", "pin", "block:lead", "new name"],
            "env": { "REX_SESSION": env("REX_SESSION"), "REX_BLOCK": false, "REX_SERVER": env("REX_SERVER") },
        }]);
        assert_eq!(calls, [pin.to_string()]);
        assert!(result.contains(r#""leads":["block:lead"]"#), "{result}");

        // Two leads with workers: both pins take the name, or the other
        // lead's roster reverts it.
        let (ok, calls, result) = run_action("two", "roster_rename_session", "name=new");
        assert!(ok, "{result}");
        let argv: Vec<_> = calls
            .iter()
            .map(|c| serde_json::from_str::<serde_json::Value>(c).unwrap()[1]["argv"].clone())
            .collect();
        assert_eq!(
            argv,
            [
                serde_json::json!(["/opt/bin/rexroster", "pin", "block:lead", "new"]),
                serde_json::json!(["/opt/bin/rexroster", "pin", "block:lead2", "new"]),
            ]
        );

        // No workers: a plain rename.
        let (ok, calls, _) = run_action("none", "roster_rename_session", "name=new");
        assert!(ok);
        assert_eq!(calls, [r#"["set_label",{"label":"new"}]"#]);

        // A hidden worker, by label, floats beside its lead and gets focus.
        let (ok, calls, result) = run_action("one", "roster_show_worker", "block=fix-login");
        assert!(ok, "{result}");
        assert_eq!(
            calls,
            [
                r#"["move_block",{"anchor_block_id":"block:lead","block_id":"block:w1"}]"#,
                r#"["float_block",{"block_id":"block:w1","bounds":[0.1,0.08,0.8,0.84]}]"#,
                r#"["focus_block",{"block_id":"block:w1"}]"#,
            ]
        );

        // A shown worker, by id, is left alone.
        let (ok, calls, result) = run_action("one", "roster_show_worker", "block=block:w2");
        assert!(ok && calls.is_empty(), "{calls:?} {result}");
        assert!(result.contains(r#""shown":false"#), "{result}");

        // Not a worker: the lead itself, or no such block.
        for arg in ["block=lead", "block=nope"] {
            let (ok, calls, result) = run_action("one", "roster_show_worker", arg);
            assert!(!ok && calls.is_empty(), "{arg}: {calls:?} {result}");
            assert!(result.contains("no worker"), "{result}");
        }
    }

    #[test]
    fn paths_are_quoted_for_lua() {
        assert_eq!(lua_string(r#"/a "b"\c"#), r#""/a \"b\"\\c""#);
        assert_eq!(lua_string("a\nb\x7f"), r#""a\010b\127""#);
        assert_eq!(lua_string("/Users/é/x y"), "\"/Users/é/x y\"");
    }
}
