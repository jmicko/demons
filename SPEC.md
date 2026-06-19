# Demons — Specification (v0.1)

## 1. Overview

Demons is a single-binary CLI for running a project's full set of long-running development commands side-by-side in one terminal, as a grid of panes each backed by a real PTY. Run `demons` from a project root and every command declared in `demons.toml` starts at once.

Demons is intentionally minimal: it is **not** a session manager, a process supervisor, a build system, or a tmux replacement. It exists only while your dev session is active.

## 2. Goals

- One command (`demons`) starts the entire dev stack defined in `demons.toml`.
- One command (`demons init`) opens the configurator without starting tasks.
  After that, the user should rarely need to touch the file directly.
- Real PTYs with VT100/xterm-compatible rendering (colors, REPLs, and common
  TUI apps work).
- Real keyboard and mouse navigation.
- Restart crashed or running tasks on demand (`r`).
- Single static-ish binary, no runtime dependencies.
- Unix-only (Linux + macOS). Windows is explicitly out of scope.

## 3. Non-Goals (v1)

- Session persistence / detach-reattach.
- Production process supervision (no daemon, no health checks, no auto-restart loops).
- Distributed / remote task execution.
- Build orchestration (use `make` / `just` / `cargo` for that).
- Windows support.
- Plugin or extension system.
- File-watcher-based auto-restart (planned for v2; v1 has manual `r` restart only).
- Themeable colors (planned for v2).
- Pane output logging to file (planned for v2).

## 4. Configuration

### 4.1 File location and discovery

- Config file: `demons.toml`, at the project root.
- Discovery: `demons` walks up from CWD to the filesystem root looking for `demons.toml`. The first match wins.
- If no config is found:
  - In an interactive terminal: print `No demons.toml found in <path> or its parents. Run 'demons init' here? [Y/n]` and open the configurator on `Y` (default).
  - In a non-interactive terminal: report that no config was found and exit
    non-zero without prompting.
- Override: `demons --config <path>` (or `-c`) reads from the specified file, no walk.
- Multiple configs in a tree are allowed (a sub-project can have its own `demons.toml`). The closest one wins.

### 4.2 Schema

```toml
# Optional. Demons-level settings.
[settings]
# Layout strategy. "grid" (default) picks the closest-to-square arrangement
# based on terminal aspect ratio. v2 may add "tabs".
layout = "grid"
# Leader key to toggle command mode.
# Options: "alt-j" (default), "alt-backtick", "tab", "ctrl-b", "ctrl-q", "ctrl-\\".
leader = "alt-j"
# Reserved for v2. It must remain false in v1.
logging = false

# Tasks. One [[task]] per pane.
[[task]]
# Display name. Must be unique within the file.
name = "server"
# Command. String => run via $SHELL -c. Array => exec directly.
command = "cargo run"
# Working directory, relative to the config file's directory. May also be
# an absolute path. Default: directory of the config file.
cwd = "."
# Optional. Environment variables merged on top of the inherited env.
env = { RUST_LOG = "debug" }
# Optional. Task names that must be started before this task starts.
depends_on = ["server"]
# Optional. Delay after dependencies have started. Supports ms, s, m, and h.
start_delay = "3s"
# Optional. Glob patterns (relative to cwd) to watch. On change, the task
# is killed and respawned. (Implemented in v2. Schema is reserved.)
# watch = ["src/**/*.rs", "Cargo.toml"]
# Optional. Run mode "run-on-change" — task only runs when watched files
# change, then exits. (Implemented in v2. Schema is reserved.)
# run_on_change = ["src/**/*.rs"]
# Optional. Restart the task at this interval. (Implemented in v2.)
# repeat = "1s"
```

Validation rules (enforced at startup, fail loudly):

- At least one `[[task]]` is required.
- `name` is required and unique per file.
- `command` is required and non-empty.
- `cwd` must be a directory on disk at startup.
- No two `[[task]]` blocks may share the same `name`.
- `depends_on` entries must name existing tasks, cannot include the task
  itself, and cannot form dependency cycles.
- `start_delay` must be a non-negative integer with an optional unit of `ms`,
  `s`, `m`, or `h`; no unit means seconds.
- Unknown keys are an error (no silent ignoring — the configurator owns the schema).
- Reserved v2 fields are parseable so future files have a stable schema, but
  v1 rejects `logging = true` and any task that sets `watch`,
  `run_on_change`, or `repeat`. Reserved behavior is never silently ignored.

### 4.3 Configurator

`demons init` opens the configurator and does not start tasks. Without an
explicit `--config`, it edits the nearest existing `demons.toml` in the current
directory or its parents; if none exists, it creates `./demons.toml` when the
user saves. If stdin or stdout is not a TTY, `demons init` errors out:
`demons init requires an interactive terminal`.

The runtime menu is opened with `?` in command mode or by clicking the footer's
`? menu` button. The menu has top tabs:

- **Help** — command reference.
- **Tasks** — task list. Enter or click a task to edit name, command, cwd, env,
  dependencies, and start delay. Dependencies are selected from a checkbox list
  of other tasks. Working-directory edits validate immediately and support Tab
  completion for directories relative to the config file.
- **Settings** — app-level settings that can apply immediately, such as the
  leader key.
- **Exit** — discard, save without restarting, save and restart affected, or
  save and restart all. In `demons init`, save/discard closes the configurator
  without starting tasks.

Keyboard behavior follows common TUI menu conventions: arrows move, Enter
activates, Space toggles dependency checkboxes, Esc backs out one level, and
text fields support cursor movement and basic line editing. Tab completes
directories while editing a task's working directory.

## 5. CLI

```
demons                         # Run all tasks from the nearest demons.toml.
demons init                    # Open the configurator without starting tasks.
demons --config <path>         # Use a specific config file.
demons -c <path>               # Short form.
demons --help                  # Show usage.
demons --version               # Print version.
```

Reserved for v2 (not in v1):

```
demons "cmd1" "cmd2"           # One-off multi-pane run, no config.
demons --no-watch              # Disable file watching for this run.
```

## 6. Runtime

### 6.1 Layout

Calculated at startup and on terminal resize.

- Goal: readable, close-to-square panes weighted by the terminal's aspect
  ratio, without creating excessive empty cells.
- Algorithm:
  1. `terminal_aspect = cols / rows`.
  2. Account for terminal cells being roughly twice as tall as they are wide:
     `target_grid_aspect = terminal_aspect / 2`.
  3. Enumerate `columns` from `1..=N`, with
     `rows = ceil(N / columns)`.
  4. Score each candidate by logarithmic distance from the target grid aspect,
     plus `empty / N` as an empty-cell penalty. Pick the lowest score.
  5. Tile panes left-to-right, top-to-bottom, in declaration order. Empty cells in the last row are unused.
- `N = 1` → `1×1` (full screen).
- For `N > 9` in v1 we still grid them; a 10-pane grid is awkward, and that's fine for now. v2 may add a tabbed fallback.
- Fullscreen mode shows only the focused pane in the full pane area. Other
  tasks keep running and keep their last PTY size until grid mode is restored.

### 6.2 Pane

Each pane has:

- A 1-line header with: task name, status icon (`●` running, `✓` exited 0,
  `✗` exited N, `⏸` not yet started, `⏱` waiting on dependencies or delay),
  and a clickable `[↻]` restart button on the right.
- A scrollback buffer (default 10,000 lines; configurable in v2).
- A pane-local text selection buffer derived from task output, used for deep
  scrollback selection and clipboard copy.
- A PTY-backed child process.
- Visible focus state: the selected pane's border is green in input mode, red
  in command mode, and yellow in search mode.
- A footer shows the current mode and available controls, wrapping command
  buttons to additional lines when the terminal is narrow.
- A fixed-width button at the left of the footer displays and toggles the
  current mode.
- Footer command buttons are clickable. Paired shortcuts such as `y` / `Y` and
  `r` / `R` are rendered as separate buttons.

### 6.3 Navigation

The configured leader key (default `Alt+J`) toggles between two modes.

**Input mode**:

- Keyboard input goes to the selected pane.
- Mouse scroll: scrolls the pane under the pointer.
- Mouse drag: selects text in the pane under the pointer when the child is not
  using mouse reporting. If the child is using mouse reporting, `Shift`-drag
  selects text and plain mouse events continue to go to the child.
- Dragging above or below the selected pane scrolls that pane's history and
  never expands the selection into neighboring panes.
- Right-click or `Ctrl+Shift+C`: copies the current selection. Copy uses OSC 52
  for terminals that support clipboard writes and also stores an internal
  Demons clipboard.
- `Ctrl+Shift+V`, middle-click, or right-click without a current selection:
  pastes the internal Demons clipboard into the selected pane in input mode.
- `q` or `Ctrl+C` opens quit confirmation only when the focused pane can no
  longer accept input; otherwise those keys are sent to the child.
- Click on a pane: selects that pane without changing modes.
- Click on a `[↻]` button: restarts that pane.
- Mouse events inside a child that has enabled terminal mouse reporting are
  forwarded to that child.
- Press the leader or click the footer mode button → enter command mode.

**Command mode** (default after startup):

- Keyboard is captured by demons.
- `Tab` / `Shift+Tab`: cycle panes, except that `Tab` toggles modes when it is
  configured as the leader. Arrow keys and `h j k l` remain available.
- Arrow keys or `h j k l`: move focus.
- `f`: toggle fullscreen for the focused pane. While fullscreen is active,
  arrows and `h j k l` cycle the focused pane shown fullscreen.
- `PageUp` / `PageDown`: scroll the focused pane history by one page.
- `Home` / `End`: jump to the top or bottom of focused pane history.
- `y`: copy the focused pane's visible text.
- `Y`: copy the focused pane's full scrollback.
- `S`: save the focused pane's full scrollback to a temp log file and copy the
  file path.
- `/`: open focused-pane search mode. In search mode, typing updates the first
  match immediately, Enter moves to the previous match, Shift+Enter moves to
  the next match, Esc leaves search mode, and clicking another pane or pressing
  Tab/Shift+Tab retargets the active search.
- `r`: restart focused pane and its dependents.
- `R`: restart all panes.
- `c`: clear the focused pane and its scrollback.
- `?`: open the menu.
- `q` or `Ctrl+C`: show quit confirmation. Press `q` or `Ctrl+C` again to
  quit (sends SIGTERM, waits 2s, then SIGKILL) or `Esc` to cancel.
- Press the leader or click the footer mode button → return to input mode.
- Clicking a pane selects it without leaving command mode.
- Clicking a command footer button runs that button's visible action.

The leader is configurable in `[settings].leader`. Allowed values: `alt-j`
(default), `alt-backtick`, `tab`, `ctrl-b`, `ctrl-q`, `ctrl-\`. The leader is
intercepted in input mode and cannot be sent to the child.

### 6.4 Process lifecycle

For each task:

- Spawn the command in a new session / process group (so SIGTERM propagates to the whole tree, not just the immediate child).
- Inherit parent env, then merge task-level `env` on top.
- Stream stdout + stderr into the pane's scrollback, interleaved.
- Manual restart (`r` or click `[↻]`): kill the process group and any dependent
  task process groups, wait for exits, then respawn them in dependency order.
  Each dependent's `start_delay` starts after its dependencies have started.
- While a task is waiting on `start_delay`, the pane body shows a countdown to
  launch.
- v2: file-watch-driven restart, `run_on_change`, and `repeat` — schema reserved.
- On demons quit: send SIGTERM to every process group, wait 2s, send SIGKILL to any that are still alive. Exit only when all children are reaped.
- External `SIGINT`, `SIGTERM`, and `SIGHUP` signals trigger the same graceful
  shutdown path.
- On panic in demons: a panic hook best-effort kills all child process groups before re-raising.

### 6.5 ANSI and resize

- PTY output is parsed by `vt100`, which maintains a screen model for colors,
  cursor movement, alternate screens, application cursor mode, bracketed
  paste, and common mouse protocols.
- On terminal resize: recompute layout, then propagate `ioctl(TIOCSWINSZ)` to each child PTY.

## 7. Tech stack

- **Language**: Rust, edition 2024.
- **TUI**: `ratatui` + `crossterm` (mouse, resize, ANSI rendering all first-class).
- **PTY**: `portable-pty`. We are Unix-only so we don't need its Windows support, but the abstraction is convenient.
- **CLI**: `clap` v4.
- **Config**: `toml` + `serde`.
- **Terminal emulation**: `vt100`.
- **Build**: standard `cargo build --release`. A `Makefile` provides `make build` and `make install` targets.
- **Distribution**: `cargo install --path .` or `make install` to copy the release binary into `~/.cargo/bin/`.

## 8. Out of scope (recap)

- Session persistence.
- Detach / reattach.
- File-watch-based auto-restart (v2).
- `run_on_change` (v2).
- `repeat`-interval tasks (v2).
- Pane output logging to file (v2).
- Themes / colors (v2).
- Distributed execution.
- Windows.
- Daemon / background mode.
- Plugin / extension API.

## 9. Example `demons.toml`

```toml
[settings]
layout = "grid"
leader = "alt-j"

[[task]]
name = "server"
command = "cargo run"
cwd = "."

[[task]]
name = "web"
command = "npm run dev -- --host 0.0.0.0"
cwd = "./web"
env = { BROWSER = "none" }

[[task]]
name = "tail-logs"
command = "tail -f /tmp/app.log"
cwd = "."
```

## 10. Open questions (low priority)

- Should `r` on a running pane prompt for confirmation, or just kill and respawn immediately? Default: immediate.
- Should the clickable restart button be visible always, or only on mouse hover? Default: always visible.
- Should the configurator offer a diff against the saved file before writing?
  Default: no, keep save flow direct.
- Tabs for `N > 9` panes (v2).
- The v1 footer is always visible when the terminal is tall enough to spare
  one row; discoverability is more important than reclaiming that row.
