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

Run the interactive setup from a project root:

```sh
demons init
```

The wizard uses numbered choices for fixed options and supports normal line
editing keys while entering text. It also offers starter tasks when it detects
common project files such as `Cargo.toml`, `package.json`, or `Makefile`.

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
| `r` | Restart the focused task |
| `R` | Restart every task |
| `c` | Clear the focused pane and its scrollback |
| `q` or `Ctrl+C` | Stop all tasks and quit |
| Leader or `Esc` | Return to input mode |

Click a pane to focus it. Click `[↻]` in a pane header to restart that task.
The mouse wheel scrolls pane history unless the child application has enabled
terminal mouse reporting in input mode.

Drag inside a pane to select text. If the child application has enabled mouse
reporting, use `Shift`-drag to select instead of sending the drag to the child.
Dragging above or below the pane scrolls that pane's history while keeping the
selection inside the original pane. Right-click or press `Ctrl+Shift+C` to copy
the selection; terminals that support OSC 52 receive it on the system
clipboard. `Ctrl+Shift+V`, middle-click, or right-click with no active
selection pastes the last copied Demons selection back to the focused pane in
input mode.

Because the leader is intercepted, it cannot be sent to a child while in input
mode. Set a different leader if an application or window manager needs
`Alt+J`:

```toml
[settings]
leader = "ctrl-b" # also: "tab", "ctrl-q", "ctrl-\\"
```

## Testing The Wizard

To test the complete no-config flow without writing into a real project:

```sh
cargo build
repo=$PWD
scratch=$(mktemp -d)
(cd "$scratch" && "$repo/target/debug/demons")
```

Demons will offer to run `init` and will write only inside the temporary
directory. A simple test command for the wizard is:

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
