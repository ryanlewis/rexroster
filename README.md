# rexroster

A small terminal UI that lists a Rex lead's worker sessions. Workers run as detached Rex blocks, so they take no space on screen. The roster sits in a floating pane in the top-right corner of the lead's tab. It shows each worker's state, and from the list you can show or hide any worker.

It works on macOS and Linux. It needs the `rex` CLI, which it finds at `$REX_BIN_DIR/rex` (Rex sets this in every terminal) or on `PATH`.

## Install

```sh
cargo install --path .
```

## Typical use

From the lead's pane:

```sh
rexroster spawn "$REX_BLOCK" fix-login --task "fix the login redirect" --cwd ~/dev/app -- "$(command -v claude)" "…"
```

This starts the worker as a detached block and opens the roster if it isn't open already. The roster closes itself when the last worker's block closes.

## Commands

Every command reads the session from `$REX_SESSION` and stops with an error if it is not set.

| Command | What it does |
| --- | --- |
| `rexroster [--lead <block-id>] [--border none\|rounded] [--view compact\|expanded]` | Runs the roster for a lead. `--lead` defaults to `$REXROSTER_LEAD`. `--border` and `--view` override the config file for this run. |
| `rexroster open <lead-block-id> [--border …] [--view …]` | Opens the lead's roster in a floating layer at the top right of the lead's tab, without focus, unless a block labelled `roster-<last 6 characters of the lead id>` already exists. Prints the new layer id, or nothing. Exits 0 in both cases. Passes `--border` and `--view` on to the roster. |
| `rexroster spawn <lead-block-id> <name> [--task <text>] [--cwd <dir>] -- <cmd> [args...]` | Starts a detached block labelled `<name>` that runs `rexroster tag <lead> [--task <text>] -- <cmd...>`, then does what `open` does. Prints `{"block_id":"…","roster_layer_id":"…"}`, where `roster_layer_id` is `null` if the roster was already open. |
| `rexroster tag <lead-block-id> [--task <text>] -- <cmd> [args...]` | Marks the terminal as the lead's worker, then replaces itself with `<cmd>`. |

### Running the worker's command

`spawn` runs `<cmd>` directly with `exec`, not through a shell. That keeps a shell prompt from dropping the worker's meta record. So:

- Give `<cmd>` as an absolute path or a name on the server's `PATH`, for example `"$(command -v claude)"`.
- The worker also gets the `PATH` of the shell that ran `spawn`, added in front of the server's own.
- Shell syntax such as pipes, `&&` and `~` is not expanded. Wrap it in `sh -c '…'` if you need it.

### The meta record

`tag` writes this sequence to `/dev/tty` (to stdout if there is no terminal), then runs the command whether or not the write worked:

```text
ESC ] 7501 ; state=idle:id=meta:app=rexlead:msg=<base64 of {"lead":…,"task":…}> ESC \
```

The roster lists a block only when this record names its own lead.

## The roster

By default the roster has no frame or title, because Rex's pane title already names it. In the compact view (the default) each worker takes one line:

- `›` on the selected worker
- a glyph for the state of the worker's root status record (the one Claude Code reports): a turning braille spinner (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) while working, `◐` blocked, `✓` done, `✗` error, `○` idle, `·` starting
- `▸` if the worker is shown
- the label, in a column as wide as the longest label (6 to 20 characters)
- the time in the current state
- the record's message, or the task when there is no message, cut with `…` to fit

Press `v` for the expanded view. Each worker then has a line with its label and time, then its message wrapped to the pane width (at most 3 lines), then its task in dim text if that differs from the message.

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move the selection |
| `Enter`, double-click | Show the worker as a centred float beside the lead and focus it. On a shown worker, hide it again |
| `v` | Switch between the compact and expanded views (not saved) |
| `o` | Open the options panel |
| `l` | Focus the lead |
| `q`, `Ctrl-C` | Quit |

A line of key hints shows at the bottom while the roster has focus. With a border, the hints sit on the bottom border instead.

To hide a shown worker, press `Enter` on it or minimize its pane: the roster turns a minimized worker back into a hidden one. Closing a worker's pane ends the worker.

The layer's height follows the lines the workers take, plus the hints line while focused. If the roster never gets a worker, it exits after 30 seconds.

The spinner redraws every 100 ms while any worker is working. Otherwise the roster wakes once a second to update the times.

Focus key: `cmd+alt+R` (physical key `KeyR`, action `roster_toggle`) switches focus between the lead and its roster. The binding lives in Rex's `init.lua`. After you change that file, for example with a dotfiles tool such as chezmoi, run `rex config reload`.

## Options

Options live in `$XDG_CONFIG_HOME/rexroster/config.toml`, or `~/.config/rexroster/config.toml` when `XDG_CONFIG_HOME` is not set:

```toml
border = "rounded"   # "none" (default) or "rounded": a box titled "workers"
view = "expanded"    # "compact" (default) or "expanded": the view the roster starts in
```

A missing file or key means the default, and unknown keys are ignored. If the file can't be parsed, the roster shows one error line and uses the defaults. `--border` and `--view` on the command line win over the file.

Press `o` to open the options panel in the roster:

- `j`/`k` or the arrow keys move between options.
- `Enter`, `Space`, `←` or `→` change the value, as does clicking an option. Changes apply straight away.
- `Esc` or `o` closes the panel and saves the options you changed to the config file. Other keys and comments in the file are kept.

The roster writes this file itself, so don't manage it with a dotfiles tool that would overwrite it.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check
```

The tests replay `rex events --json` output recorded in `testdata/` and never call `rex`.

Licensed under either of MIT or Apache-2.0, at your option.
