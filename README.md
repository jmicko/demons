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
Working-directory fields validate before they are applied, and Tab completes
directory names relative to the config file.

Or create `demons.toml` yourself:

```toml
[[task]]
name = "server"
command = "cargo run"

[[task]]
name = "web"
command = "npm run dev -- --host 0.0.0.0"
cwd = "./web"
env = { BROWSER = "none" }
```

Then run:

```sh
demons
```

Demons searches the current directory and its parents for the nearest
`demons.toml`. Use `demons --config path/to/file.toml` to select one directly.

## Controls

Demons has two modes:

* **Input mode**: keyboard and child mouse input goes to the selected task.
* **Command mode**: keyboard input controls Demons.

Demons starts in input mode. Press `Alt+J` or click the fixed mode button at
the left of the footer to switch modes. Clicking a pane selects it without
changing modes.

| Key | Command mode |
| --- | --- |
| Arrow keys or `h j k l` | Move focus |
| `Tab` / `Shift+Tab` | Cycle panes |
| `f` | Toggle fullscreen for the focused pane |
| `PageUp` / `PageDown` | Scroll the focused pane by one page |
| `Home` / `End` | Jump to the top or bottom of focused pane history |
| `y` | Copy the focused pane's visible text |
| `Y` | Copy the focused pane's full scrollback |
| `S` | Save the focused pane's full scrollback to a temp log file |
| `/` | Search the focused pane's scrollback |
| `r` | Restart the focused task and its dependents |
| `R` | Restart every task |
| `c` | Clear the focused pane and its scrollback |
| `?` | Open the menu |
| `q` or `Ctrl+C` | Ask to close Demons |
| Leader or `Esc` | Return to input mode |

Click a pane to focus it. Click `[↻]` in a pane header to restart that task.
The mouse wheel scrolls pane history unless the child application has enabled
terminal mouse reporting in input mode. Footer command buttons are clickable;
paired commands like `y` / `Y` and `r` / `R` are shown as separate buttons.
The `? menu` button opens a tabbed menu with Help, Tasks, Settings, and Exit
sections.

Drag inside a pane to select text. If the child application has enabled mouse
reporting, use `Shift`-drag to select instead of sending the drag to the child.
Dragging above or below the pane scrolls that pane's history while keeping the
selection inside the original pane. Right-click or press `Ctrl+Shift+C` to copy
the selection; terminals that support OSC 52 receive it on the system
clipboard. `Ctrl+Shift+V`, middle-click, or right-click with no active
selection pastes the last copied Demons selection back to the focused pane in
input mode.

In command mode, `y` copies the focused pane's visible text and `Y` copies its
full scrollback. `S` saves the focused pane's full scrollback to a temp log file
and copies the file path. `/` opens a focused-pane search prompt; press Enter to
search older, `Shift+Enter` to search newer, or `Esc` to leave search mode.

Closing Demons is confirmed: press `q` or `Ctrl+C`, then press it again to
close, or `Esc` to cancel. In input mode, those keys still go to a running
child process; once the focused pane can no longer accept input, they open the
same close confirmation.

Because the leader is intercepted, it cannot be sent to a child while in input
mode. Set a different leader if an application or window manager needs
`Alt+J`. The Settings tab can cycle the leader at runtime, and the Exit tab can
save or discard that change. `Alt+Backtick` is available for one-hand use, but
some desktops use it for window switching, so it is not the default.

```toml
[settings]
leader = "alt-backtick" # also: "tab", "ctrl-b", "ctrl-q", "ctrl-\\"
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
[[task]]
name = "api"
command = "RUST_LOG=debug cargo run"
```

Array commands execute directly, without shell parsing:

```toml
[[task]]
name = "api"
command = ["cargo", "run", "--bin", "api"]
cwd = "."
env = { RUST_LOG = "debug" }
```

Tasks can depend on other tasks. A dependent task starts only after all of its
dependencies have started, then waits its own optional `start_delay`. Restarting
a task also restarts its dependents. While a task is waiting for a delayed
start, the pane body shows the countdown to launch.

```toml
[[task]]
name = "server"
command = "cargo run"

[[task]]
name = "web"
command = "npm run dev"
depends_on = ["server"]
start_delay = "3s"
```

Task names must be unique. Working directories are resolved relative to the
directory containing the config file. Unknown keys and invalid directories are
reported before any task starts.

`logging`, `watch`, `run_on_change`, and `repeat` are reserved schema fields.
Demons rejects reserved task fields when set, and rejects `logging = true`, so
a configuration never silently promises behavior that is not implemented.

## Process Behavior

Tasks start concurrently in separate process groups. Restart and shutdown
signals apply to each full task process tree. On quit, Demons sends `SIGTERM`,
waits up to two seconds, then sends `SIGKILL` to anything still running.
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
See [SPEC.md](SPEC.md) for the v1 behavior and product boundaries.
