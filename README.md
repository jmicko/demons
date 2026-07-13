# Demons

Demons starts all of a project's long-running development commands in one
terminal. Each command gets a real PTY and a pane in a small, purpose-built
multiplexer.

```text
$ cd my-project
$ demons
```

It is designed for the common case where a project needs a server, frontend,
worker, log tail, or similar commands running together. Demons is not a
general-purpose terminal multiplexer or production process supervisor.

## Install

Demons supports Linux and macOS.

Install the published crate:

```sh
cargo install demons --locked
```

Or install from the source tree:

```sh
cargo install --path . --locked
```

To build without installing:

```sh
cargo build --release --locked
```

The binary will be at `target/release/demons`.

## Quick Start

Run the configurator from a project root, or from a subdirectory of a project
that already has a `demons.toml`:

```sh
demons init
```

`demons init` opens the same menu UI used at runtime, but it does not start any
tasks. Use the Tasks tab to add or edit tasks, Settings for app-level options,
and Exit to save or discard changes. When an existing config is found in the
current directory or a parent directory, `demons init` edits it in place.
Working-directory fields validate before they are applied, Tab completes
directory names relative to the config file, and task or terminal environment
variables are edited as key/value rows.

If an existing config is parseable TOML but does not match the current schema,
`demons init` recovers the pieces it understands into an editable draft. Red
problem markers show fields that must be fixed before saving, and gold markers
show fields Demons repaired or ignored so you can review them. The Exit tab has
a Problems section whose rows jump to the affected setting. The original file is
not rewritten until you explicitly save. Bare missing assignment values such as
`command =` are recovered as empty strings so the menu can mark the field red.
If the TOML is too broken to recover, Demons opens a fresh draft that can
overwrite the broken file only when you save. Configs from unsupported future
schema versions still fail instead of being guessed at.

Regular `demons` startup uses the same recovery path for recoverable config
problems in an interactive terminal: it opens the menu without starting tasks,
then starts them after you fix red problems and save.

Or create `demons.toml` yourself:

```toml
schema_version = 4

[settings]
layout = "grid"
leader = "alt-j"
multi_click_ms = 500
logging = false
mcp_access = "off"
mcp_status_bar = true

[[task]]
name = "server"
command = "cargo run"
cwd = "."
depends_on = []

[task.env]

[[task]]
name = "web"
command = "npm run dev -- --host 0.0.0.0"
cwd = "./web"
depends_on = []

[task.env]
BROWSER = "none"

[[terminal]]
name = "scratch"
cwd = "."
```

Then run:

```sh
demons
```

Demons searches the current directory and its parents for the nearest
`demons.toml`. Use `demons --config path/to/file.toml` to select one directly.

## Controls

Demons has two modes:

* **Input mode**: keyboard and child mouse input goes to the selected pane.
* **Command mode**: keyboard input controls Demons.

Demons starts in command mode. Press `Alt+J` or click the fixed mode button at
the left of the footer to switch between command mode and input mode. Clicking
a pane selects it without changing modes.

| Key | Command mode |
| --- | --- |
| Arrow keys or `h j k l` | Move focus |
| `Tab` / `Shift+Tab` | Cycle panes |
| `f` | Toggle fullscreen for the focused pane |
| `PageUp` / `PageDown` | Scroll the focused pane by one page |
| `Home` / `End` | Jump to the top or bottom of focused pane history |
| `y` | Copy the current selection |
| `Y` | Copy the focused pane's full scrollback |
| `S` | Save the focused pane's full scrollback to a temp log file |
| `/` | Search the focused pane's scrollback |
| `t` | Add a temporary terminal pane for this session |
| `x` | Close the focused temporary terminal pane |
| `r` | Restart the focused pane and any task dependents |
| `R` | Restart every pane |
| `c` | Clear the focused pane and its scrollback |
| `?` | Open the menu |
| `q` or `Ctrl+C` | Ask to close Demons |
| Leader | Return to input mode |

Click a pane to focus it. Click `[↻]` in a pane header to restart that pane.
The mouse wheel scrolls pane history unless the child application has enabled
terminal mouse reporting in input mode. Footer command buttons are clickable;
paired commands like `y` / `Y` and `r` / `R` are shown as separate buttons.
The `? menu` button opens a tabbed menu with Help, Tasks, Settings, and Exit
sections.

Drag inside a pane to select text. Double-click selects the word under the
mouse; hold and drag after the second click to expand by whole words.
Triple-click selects the visible line; hold and drag after the third click to
expand by whole lines. If the child application has enabled mouse reporting,
use `Shift`-drag to select instead of sending the drag to the child. Dragging
above or below the pane scrolls that pane's history while keeping the selection
inside the original pane. Right-click or press `Ctrl+Shift+C` to copy the
selection. Demons also attempts to write it to the system clipboard and, up to
512 KiB, sends OSC 52 for compatible host terminals.
`Ctrl+Shift+V`, middle-click, or right-click with no active selection pastes the
last copied Demons selection back to the focused pane in input mode.

In command mode, `y` copies the active selection and is disabled when nothing is
selected. `Y` copies the focused pane's full scrollback. `S` saves the full
scrollback to a temp log file and copies the file path. On Unix, these logs are
written under a per-user temp directory with restricted permissions. `/` opens
a focused-pane search prompt; typing jumps to the newest match and shows the
current/total match count. Press Enter to go to the previous match,
`Shift+Enter` to go to the next match, or `Esc` to leave search mode.
Press `Tab`/`Shift+Tab` or click another pane while the prompt is open to
search that pane instead.

Press `t` to add a regular shell for the current Demons session. When that pane
is focused, the footer shows `x close`; pressing it removes only that temporary
pane. Persistent terminal panes are added, edited, or removed in the Tasks tab.

Each pane retains up to 10,000 rows. The live screen and nearby scrollback use
full terminal emulation; the deeper archive is optimized for line-oriented
build and server output while preserving streamed UTF-8, terminal-width text,
and ANSI colors. Cursor-addressed redraws are snapshotted from the live parser
so interactive output does not silently diverge when it enters the archive.

Closing Demons is confirmed: press `q` or `Ctrl+C`, then press it again to
close, or `Esc` to cancel. In input mode, those keys still go to a running
child process; once the focused pane can no longer accept input, they open the
same close confirmation.

Because the leader is intercepted, it cannot be sent to a child while in input
mode. Set a different leader if an application or window manager needs
`Alt+J`. The Settings tab can change the leader at runtime, and the Exit tab
can save or discard that change. `Alt+Backtick` is available for one-hand use,
but some desktops use it for window switching, so it is not the default.

```toml
schema_version = 4

[settings]
leader = "alt-backtick" # also: "tab", "ctrl-b", "ctrl-q", "ctrl-\\"
multi_click_ms = 500    # double/triple-click timing, 150-1000
```

## Testing The Configurator

To test the complete no-config flow without writing into a real project:

```sh
cargo build
repo=$PWD
scratch=$(mktemp -d)
(cd "$scratch" && "$repo/target/debug/demons")
```

Demons will offer to open the configurator and will write only inside the
temporary directory. A simple test command for a task is:

```sh
while true; do date; sleep 1; done
```

## Configuration

String commands run through `$SHELL -c` (falling back to `/bin/sh`):

```toml
schema_version = 4

[[task]]
name = "api"
command = "RUST_LOG=debug cargo run"
cwd = "."
depends_on = []

[task.env]
```

Array commands execute directly, without shell parsing:

```toml
schema_version = 4

[[task]]
name = "api"
command = ["cargo", "run", "--bin", "api"]
cwd = "."
depends_on = []

[task.env]
RUST_LOG = "debug"
```

Tasks can depend on other tasks. A dependent task starts only after all of its
dependencies have started, then waits its own optional `start_delay`. Restarting
a task also restarts its dependents. While a task is waiting for a delayed
start, the pane body shows the countdown to launch.

```toml
schema_version = 4

[[task]]
name = "server"
command = "cargo run"
cwd = "."
depends_on = []

[task.env]

[[task]]
name = "web"
command = "npm run dev"
cwd = "."
depends_on = ["server"]
start_delay = "3s"

[task.env]
```

Use `[[terminal]]` for a regular shell pane that starts alongside tasks:

```toml
schema_version = 4

[[terminal]]
name = "scratch"
cwd = "."

[terminal.env]
RUST_LOG = "debug"
```

Task and terminal names share one namespace. Working directories are resolved
relative to the directory containing the config file. Unknown keys and invalid
directories are reported before any task starts. Saving task-list changes from
the runtime menu reconciles them in the current session: compatible panes stay
running, added panes start, removed panes stop, and temporary terminals remain.
The selected save action controls whether affected or all retained panes also
restart. The command footer has `t terminal` for adding a temporary shell pane
that is not written to the config. Focus that pane and use `x close` to remove
it.

`schema_version` is the Demons config schema version, not the Demons app or
crate version. Existing unversioned configs are treated as the current schema
and are normalized after they successfully validate. Schema versions 1 through
3 migrate to version 4 when they are next saved.

`logging`, `watch`, `run_on_change`, and `repeat` are reserved schema fields.
Demons rejects reserved task fields when set, and rejects `logging = true`, so
a configuration never silently promises behavior that is not implemented.

## Codex Integration

On Linux and macOS, the Settings tab can expose the current Demons project to
Codex through a project-scoped MCP server:

| MCP access | Behavior |
| --- | --- |
| **Off** | No live control socket. Saving removes only the Codex registration managed by Demons. |
| **Read only** | Codex can list panes, read/search/wait for process output, inspect status, and request a rendered TUI image. |
| **Full** | Adds commands, input, interrupts, restarts, and closing agent-owned command panes. |

Saving Read only or Full creates a managed `mcp_servers.demons` entry in
`<project>/.codex/config.toml`. The entry contains the absolute path to this
project's `demons.toml` and an opaque project scope ID generated by Demons.
Demons refuses to overwrite a user-owned server with the same name. Access is
limited to running instances with both that exact config path and scope ID, so
another MCP-enabled Demons project is not discoverable through this entry.
If `.codex` is instead a zero-byte regular file, Demons offers to replace it
with the required directory when you save. Nonempty files, symlinks, and other
unexpected filesystem entries remain blocked and untouched.

Codex loads project-local `.codex/config.toml` files only for trusted projects.
Trust the project when Codex prompts, then restart Codex after installing or
removing the registration. Lowering MCP access takes effect immediately inside
Demons, even if an already-running Codex session has not restarted. Write tools
are also marked as mutations so Codex can apply its normal approval policy.

The output tools return bounded plain-text process history, not Demons UI
chrome or other visual-only content. `capture_tui` is available when layout
matters; it renders Demons' current terminal cell grid into a PNG without using
an operating-system screenshot. Its `workspace` view shows the panes beneath
dialogs, while `full` includes the current menu or confirmation overlay.

When MCP access is enabled, `mcp_status_bar = true` shows a single activity row
above the command footer. It reports action summaries without command text,
input contents, search terms, or pane output. Click the arrow at the right to
expand a bounded recent history upward; the arrow remains fixed in place.

The generated config fields normally do not need to be edited by hand:

```toml
schema_version = 4

[settings]
mcp_access = "read_only" # also: "off", "full"
mcp_status_bar = true
mcp_scope_id = "3f4a7f63-2492-477a-ae7f-92bffab78fa4"
```

## Process Behavior

Panes start concurrently in separate process groups. Restart and shutdown
signals apply to each full process tree. On quit, Demons sends `SIGTERM` to
configured tasks and `SIGHUP` to shell panes, waits up to two seconds, then
sends `SIGKILL` to anything still running.
External `SIGINT`, `SIGTERM`, and `SIGHUP` signals trigger the same cleanup.

The VT renderer supports colors, cursor movement, alternate screens, bracketed
paste, application cursor keys, and common terminal mouse protocols. Terminal
features outside the VT100/xterm model, such as graphics protocols, are not
rendered.

## Development

```sh
make test      # format, clippy, and tests
make build     # release build
```

Before publishing or cutting a release, run the packaging check. It runs the
full test suite and then validates `cargo package` in an isolated target
directory so the workspace `target/` is never polluted by the package build:

```sh
make release-check
```

See [RELEASING.md](RELEASING.md) for the version bump and publish checklist.
See [SPEC.md](SPEC.md) for the current behavior and product boundaries.
