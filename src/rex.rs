//! Talking to Rex. Everything goes through the `rex` CLI: `rex api call` and
//! `rex block call` for one-off requests, `rex events` for the stream.
//!
//! The [`Rex`] trait is the only IO the model and the commands see, so tests
//! can swap in a fake.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// One-off requests to the Rex server.
pub trait Rex {
    /// `rex api call <method> <params>`.
    fn call(&self, method: &str, params: Value) -> Result<Value>;
    /// `rex block call -s <session> -b <block> <method> '{}'`.
    fn block_call(&self, session: &str, block: &str, method: &str) -> Result<Value>;
}

/// The real `rex` CLI.
pub struct Cli {
    bin: PathBuf,
}

impl Cli {
    /// `$REX_BIN_DIR/rex` when it exists (Rex sets it in every block), else `rex` on `PATH`.
    pub fn new() -> Self {
        let bin = std::env::var_os("REX_BIN_DIR")
            .map(|dir| PathBuf::from(dir).join("rex"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("rex"));
        Self { bin }
    }

    pub fn command(&self) -> Command {
        Command::new(&self.bin)
    }

    fn run(&self, args: &[&str]) -> Result<Value> {
        let out = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !out.status.success() {
            bail!(
                "rex {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if out.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&out.stdout).context("parsing rex output")
    }
}

impl Rex for Cli {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.run(&["api", "call", method, &params.to_string()])
            .with_context(|| format!("calling {method}"))
    }

    fn block_call(&self, session: &str, block: &str, method: &str) -> Result<Value> {
        self.run(&["block", "call", "-s", session, "-b", block, method, "{}"])
            .with_context(|| format!("calling {method} on {block}"))
    }
}

/// A block from `session.list_blocks`.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockInfo {
    pub id: String,
    pub label: String,
    /// `placed` blocks are in a window; `detached` ones are not.
    pub placed: bool,
}

pub fn list_blocks(rex: &dyn Rex, session: &str) -> Result<Vec<BlockInfo>> {
    let v = rex.call("session.list_blocks", json!({ "session_id": session }))?;
    Ok(parse_blocks(&v))
}

pub fn parse_blocks(v: &Value) -> Vec<BlockInfo> {
    v["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(BlockInfo {
                id: b["block_id"].as_str()?.to_owned(),
                label: b["label"].as_str().unwrap_or_default().to_owned(),
                placed: b["placement"].as_str() != Some("detached"),
            })
        })
        .collect()
}

/// The window that holds a block, from a `session.view` snapshot.
pub fn window_of(view: &Value, block: &str) -> Option<String> {
    view["windows"].as_array()?.iter().find_map(|w| {
        let holds = w["layers"]
            .as_array()?
            .iter()
            .any(|l| layer_holds(l, block));
        holds.then(|| w["window_id"].as_str().map(str::to_owned))?
    })
}

/// The floating layer that holds a block, with its bounds `[x, y, w, h]`.
pub fn floating_layer_of(view: &Value, block: &str) -> Option<(String, [f64; 4])> {
    view["windows"].as_array()?.iter().find_map(|w| {
        w["layers"].as_array()?.iter().find_map(|l| {
            if l["kind"].as_str() == Some("tiled") || !layer_holds(l, block) {
                return None;
            }
            let b = l["bounds"].as_array()?;
            let mut bounds = [0.0; 4];
            for (slot, v) in bounds.iter_mut().zip(b) {
                *slot = v.as_f64()?;
            }
            Some((l["layer_id"].as_str()?.to_owned(), bounds))
        })
    })
}

fn layer_holds(layer: &Value, block: &str) -> bool {
    layer["blocks"]
        .as_array()
        .is_some_and(|bs| bs.iter().any(|b| b["block_id"].as_str() == Some(block)))
}

/// The height, as a fraction of the window, of the layout cell showing `block`.
pub fn rect_height_of(view: &Value, block: &str) -> Option<f64> {
    match view {
        Value::Object(map) => {
            if map.get("block_id").and_then(Value::as_str) == Some(block)
                && let Some(h) = map
                    .get("rect")
                    .and_then(|r| r.get(3))
                    .and_then(Value::as_f64)
            {
                return Some(h);
            }
            map.values().find_map(|v| rect_height_of(v, block))
        }
        Value::Array(items) => items.iter().find_map(|v| rect_height_of(v, block)),
        _ => None,
    }
}

/// A terminal's size from its `size` method. `None` until a client has drawn
/// it: a new pane reports 80×24 before then, which is not its real size.
pub fn drawn_rows(size: &Value) -> Option<u16> {
    let drawn = size["desired"].as_array().is_some_and(|d| !d.is_empty());
    let rows = size["rows"].as_u64()?;
    if drawn {
        u16::try_from(rows).ok()
    } else {
        None
    }
}

#[cfg(test)]
pub mod fake {
    //! A [`Rex`] that answers from canned responses and records every call.

    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    pub struct Fake {
        pub responses: RefCell<HashMap<String, Value>>,
        pub calls: RefCell<Vec<(String, Value)>>,
    }

    impl Fake {
        pub fn on(&self, method: &str, response: Value) {
            self.responses
                .borrow_mut()
                .insert(method.to_owned(), response);
        }

        pub fn methods(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|(m, _)| m.clone()).collect()
        }

        pub fn params(&self, method: &str) -> Vec<Value> {
            self.calls
                .borrow()
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl Rex for Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.borrow_mut().push((method.to_owned(), params));
            Ok(self
                .responses
                .borrow()
                .get(method)
                .cloned()
                .unwrap_or(Value::Null))
        }

        fn block_call(&self, _session: &str, block: &str, method: &str) -> Result<Value> {
            let key = format!("{block} {method}");
            self.calls
                .borrow_mut()
                .push((key.clone(), Value::String(block.to_owned())));
            Ok(self
                .responses
                .borrow()
                .get(&key)
                .cloned()
                .unwrap_or(Value::Null))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Value {
        serde_json::from_str(include_str!("../testdata/view.json")).unwrap()
    }

    #[test]
    fn finds_the_window_of_a_tiled_block() {
        assert_eq!(
            window_of(&view(), "block:lead").as_deref(),
            Some("window:1")
        );
        assert_eq!(window_of(&view(), "block:nope"), None);
    }

    #[test]
    fn finds_the_floating_layer_of_a_block() {
        let (id, bounds) = floating_layer_of(&view(), "block:roster").unwrap();
        assert_eq!(id, "layer:float");
        assert_eq!(bounds, [0.72, 0.02, 0.27, 0.12]);
        // The lead is only in the tiled layer.
        assert_eq!(floating_layer_of(&view(), "block:lead"), None);
    }

    #[test]
    fn finds_the_rect_height_in_a_nested_layout() {
        assert_eq!(rect_height_of(&view(), "block:lead"), Some(0.5));
    }

    #[test]
    fn undrawn_pane_has_no_rows() {
        let fresh = json!({"columns": 80, "rows": 24, "desired": []});
        assert_eq!(drawn_rows(&fresh), None);
        let drawn = json!({"columns": 187, "rows": 62, "desired": [{"rows": 62}]});
        assert_eq!(drawn_rows(&drawn), Some(62));
    }

    #[test]
    fn parses_placement() {
        let v = json!({"blocks": [
            {"block_id": "block:a", "label": "a", "placement": "placed"},
            {"block_id": "block:b", "label": "b", "placement": "detached"},
        ]});
        let blocks = parse_blocks(&v);
        assert!(blocks[0].placed);
        assert!(!blocks[1].placed);
    }
}
