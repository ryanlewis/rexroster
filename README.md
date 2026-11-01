# rexroster

A small terminal UI that lists a Rex lead's worker sessions. Workers run as detached Rex blocks. The roster sits in a floating pane in the top-right corner of the lead's tab and shows each worker's state. From the list you can show or hide a worker.

It works on macOS and Linux and needs the `rex` CLI on `PATH`.

## Install

```sh
cargo install --path .
```

## Commands

Every command reads the session from `$REX_SESSION` and stops with an error if it is not set.

| Command | What it does |
| --- | --- |
| `rexroster [--lead <block-id>]` | Runs the roster for a lead. `--lead` defaults to `$REXROSTER_LEAD`. |
| `rexroster open <lead-block-id>` | Opens the lead's roster in a floating layer, unless one is already open. Prints the new layer id, or nothing. |
| `rexroster spawn <lead-block-id> <name> [--task <text>] [--cwd <dir>] -- <cmd> [args...]` | Starts a detached worker block that runs `<cmd>` through `rexroster tag`, then does what `open` does. Prints `{"block_id":…,"roster_layer_id":…}`. |
| `rexroster tag <lead-block-id> [--task <text>] -- <cmd> [args...]` | Marks the terminal as the lead's worker, then replaces itself with `<cmd>`. |

Only `tag` works so far. The others print "not implemented yet" and exit with status 1.

`tag` writes this sequence before it runs the command:

```text
ESC ] 7501 ; state=idle:id=meta:app=rexlead:msg=<base64 of {"lead":…,"task":…}> ESC \
```

A roster lists a block only when this record names its own lead.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check
```

Licensed under either of MIT or Apache-2.0, at your option.
