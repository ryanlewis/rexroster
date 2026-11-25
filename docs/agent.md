# rexroster: running worker sessions from a lead agent

You are a lead: an agent in a Rex terminal that starts other agent sessions
(workers) to do parts of a task. rexroster runs each worker in a detached Rex
block, which takes no space on screen, and lists your workers with their state
in a small floating pane (the roster) at the top right of your pane. The user
looks in on a worker from the roster; you read its state with `rex`.
`rexroster skill` prints this guide.

Use rexroster when you start more than one worker and the user should not have
to arrange panes for them. For a single helper the user wants to watch, a plain
`rex` split is simpler.

`$REX_BLOCK` is your own block id, and the lead id in every rexroster command;
`$REX_SESSION` is your session. If either is unset or not found, run
`rex whoami --json`. `rex` means the `rex` command; if `command -v rex` finds
nothing, call `"$REX_BIN_DIR/rex"`, which Rex sets in every terminal.

## Start a worker

One worker per command:

```sh
rexroster spawn "$REX_BLOCK" <name> --task '<the task in one line>' --cwd <dir> -- \
  "$(command -v claude)" <args...>
```

- `<name>` is the block label shown in the roster. Name it after the work.
- `--task` is shown in the roster until the worker reports a message.
- `--cwd` is the worker's working directory. Without it the worker starts in
  the user's home directory.
- The command runs with `exec`, not through a shell, so give an absolute path
  (as above) or a name on the server's `PATH`. Pipes, `&&` and `~` are not
  expanded; wrap them in `sh -c '…'` if you need them.

It prints one line of JSON:

```json
{"block_id":"block:…","roster_layer_id":"layer:…"}
```

Record `block_id`: every later call about this worker needs it. The worker has
no layer. `roster_layer_id` is `null` when the roster was already open. If the
roster could not open, a warning goes to stderr and the worker runs anyway.
`spawn` also starts the watcher (below) if none is running and the config
allows it.

## Read a worker's state

To wait on your workers, follow them with `rexroster events` (see
[Follow your workers](#follow-your-workers)) rather than reading their state in
a loop.

Claude Code reports its state to Rex. Read that instead of the screen:

```sh
rex block call -s "$REX_SESSION" -b <block_id> program_status '{}' \
  | jq -r '.records[] | select(.app == "claude-code" and (.id // "") == "")
           | "\(.state) \(.kind // "") \(.msg // "")"'
```

Use only the record with no `id`: records with an `id` are the worker's
subagents, and their state is not the worker's. The state is one of:

- `idle`: waiting for input. A new worker is ready for its first message.
- `working`: busy. `msg` is the current step.
- `done`: finished its turn.
- `blocked`: waiting on a permission prompt, question or dialog in the pane.
- `error`: it hit an error.

No `claude-code` record means Claude Code has not started yet, has exited, or
is showing a startup dialog. `rex capture -s "$REX_SESSION" -b <block_id> --trim`
shows the screen when the state is not enough. The `rexlead` record with
`id: "meta"` is rexroster's own; ignore it.

## Show and hide a worker

The user can show a worker by selecting it in the roster and pressing Enter,
and hide it the same way. To show one yourself, place it beside your block,
then float it:

```sh
rex api call session.move_block "$(jq -nc --arg s "$REX_SESSION" --arg b <block_id> --arg a "$REX_BLOCK" \
  '{session_id:$s, block_id:$b, anchor_block_id:$a}')"
rex api call session.float_block "$(jq -nc --arg s "$REX_SESSION" --arg b <block_id> \
  '{session_id:$s, block_id:$b, bounds:[0.1,0.08,0.8,0.8]}')"
```

Neither call moves focus. To hide it again, detach it:

```sh
rex api call session.detach_block "$(jq -nc --arg s "$REX_SESSION" --arg b <block_id> \
  '{session_id:$s, block_id:$b}')"
```

Never close a worker's pane to hide it: that ends the worker.

## Close a worker

When a worker's work is finished and you no longer need it:

```sh
rex api call block.close "$(jq -nc --arg s "$REX_SESSION" --arg b <block_id> \
  '{session_id:$s, block_id:$b}')"
```

This ends the program in the block. The roster keeps `done` workers listed
until their block closes, and exits by itself once its list is empty.

## Bring the roster back

If the user closed the roster, or it exited before you started more workers:

```sh
rexroster open "$REX_BLOCK"
```

It prints the new layer id, or nothing if one is already open.

## The session name

Your workers are blocks in your session, so a worker that renames "its"
session renames yours. `spawn` pins your session's name before your first
worker starts, and while you have live workers the roster puts that name back
whenever the session is renamed. The pin is released when your last worker's
block closes. To change the name on purpose during a run:

```sh
rexroster pin "$REX_BLOCK" '<new name>'   # pin it and rename the session
rexroster pin "$REX_BLOCK"                # keep the session's current name
```

Without `pin`, a rename during a run is reverted. After 5 reverts in a minute
the roster stops reverting for a while and shows why.

The user may have rexroster's palette actions loaded (`rexroster lua` prints
them; `rexroster lua --loader` prints what Rex's init.lua loads them with): "Roster: Rename session…" renames through the pin,
and "Roster: Show worker…" shows one of your workers. You don't need them;
use `rexroster pin` and the commands above.

## Follow your workers

`rexroster events` prints one line each time one of your workers needs you,
and nothing while they work. Run it under your harness's background monitor
(Claude Code's Monitor tool), so each line wakes you and you never poll:

```sh
rexroster events "$REX_BLOCK" --reports <dir>
```

| Line | Means |
| --- | --- |
| `<name> ready` | a new worker reached its prompt: brief it |
| `<name> done: <first line> (<path>)` | it stopped, and wrote that report this turn |
| `<name> done: no report` | it stopped without one: read its screen |
| `<name> blocked: <msg>` | a prompt is waiting; read the screen and answer it or raise it |
| `<name> error: <msg>` | Claude Code reported an error |
| `<name> exited` | its program exited; the block is still open |
| `<name> closed` | its block closed |

`idle` may stand in for `done`. A worker waiting on its own subagent counts as
working, so a long review is not a stop. With `--reports`, have each worker
write its final report to `<dir>/<name>#<n>.md`, first line the verdict, and
not send it as a message: the stop line then carries the verdict, and one
turn wakes you once. Start it after your first `spawn`; it exits by itself,
status 0, once you have no workers left. A line `rexroster events: the rex
event stream ended` and status 1 means the Rex server went away. A stop line
waits until the worker has stayed stopped for 10 s, so a subagent starting or
finishing does not wake you. A worker already blocked when it starts is
reported straight away. With `--reports`, it keeps what it knew in
`<dir>/.rexroster-events.json`, so after a restart (say, a monitor timeout) it
prints the stops and closes it missed, once, and repeats none. Without
`--reports`, a restart reports only workers blocked or in error.

## The watcher

`rexroster watch` runs in the background, one per user per Rex server; `spawn`
and `open` start it unless the config sets `watch = false`. When a detached worker turns `blocked`, it floats the
worker into the session's active tab so the user sees the prompt. When the
worker moves to any other state, it detaches it again. If you answer a prompt
yourself, the watcher hides the worker; don't detach it yourself. Workers that
are already shown are left alone.

`notify` in `~/.config/rexroster/config.toml` (or under `$XDG_CONFIG_HOME`)
controls notifications. `"none"`, the default, only floats and hides.
`"cases"` also runs a separate `cases` CLI that rexroster does not provide: it
opens a "stuck" case when a worker floats and withdraws it when the worker
moves on. The watcher reads the config when it starts.

## Starting workers some other way

The roster lists a block only when the block carries a meta record naming its
lead. The easy way to add one is to make `rexroster tag` the block's command,
with no shell in front of it:

```sh
rexroster tag "$REX_BLOCK" --task '<the task>' -- <absolute path to command> <args...>
```

`tag` writes the record to the terminal, then replaces itself with the
command. A program can also write the record itself:

```text
ESC ] 7501 ; state=idle:id=meta:app=rexlead:msg=<base64 of {"lead":"<lead block id>","task":"…","cwd":"…"}> ESC \
```

Rex drops idle records when a shell prints a new prompt, so the record must
come from the program that runs in the block, not from a shell before it.

## Traps

- Closing a shown worker's pane ends the worker. Hide it with
  `session.detach_block`, or let the watcher do it.
- Detached blocks live only in the Rex server's memory. A server restart loses
  every worker, and the roster with them.
- A pane has no real size until a client draws it. A detached block's `size`
  reports 80×24 with an empty `desired` list; don't trust it.
- `session.float_block` and `session.focus_block` fail on a detached block.
  Place it with `session.move_block` first.
- Pass absolute command paths to `spawn` and `tag`. The worker gets your
  `PATH` in front of the server's, but no shell, aliases or functions.
