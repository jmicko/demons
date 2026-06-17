# Demons — Specification (v0.1)

## 1. Overview

Demons is a single-binary CLI for running a project's full set of long-running development commands side-by-side in one terminal, as a grid of panes each backed by a real PTY. Run `demons` from a project root and every command declared in `demons.toml` starts at once.

Demons is intentionally minimal: it is **not** a session manager, a process supervisor, a build system, or a tmux replacement. It exists only while your dev session is active.

## 2. Goals

- One command (`demons`) starts the entire dev stack defined in `demons.toml`.
- One command (`demons init`) creates the config via an interactive wizard. After that, the user should rarely need to touch the file.
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
  - In an interactive terminal: print `No demons.toml found in <path> or its parents. Run 'demons init' here? [Y/n]` and run `init` on `Y` (default).
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
# Options: "alt-j" (default), "tab", "ctrl-b", "ctrl-q", "ctrl-\".
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
- Unknown keys are an error (no silent ignoring — the wizard owns the schema).
- Reserved v2 fields are parseable so future files have a stable schema, but
  v1 rejects `logging = true` and any task that sets `watch`,
  `run_on_change`, or `repeat`. Reserved behavior is never silently ignored.

### 4.3 `demons init` wizard

`demons init` is an interactive wizard. It uses terminal prompts rather than a
full TUI, so it works in ordinary terminals while still supporting line editing
keys for text entry. Every prompt shows the current value (if any) as the
default, and `Enter` accepts it. Fixed option prompts are presented as numbered
choices while still accepting their textual values.

If stdin or stdout is not a TTY, `demons init` errors out: `init requires an interactive terminal`.

#### 4.3.1 New config (no file present)

1. **Project settings** — `layout` and `leader`. Fixed options are selected from numbered choices. Reserved settings are not prompted.
2. **First task** — if common project files are detected, offer starter task
   defaults such as `cargo run`, a package-manager `dev` script, or `make`.
   Then prompt for name, command, cwd, and env. (v1 skips `watch` /
   `run_on_change` / `repeat`; the wizard will add them in v2.) Each shows a
   default (cwd default: `.`; env: skip).
3. **Add another task?** `[Y/n]`. If yes, loop to step 2.
4. **Review** — print the resulting `demons.toml` and ask `Write to ./demons.toml? [Y/n]`. On yes, write. On no, exit without writing.
5. **Next step** — print `Run 'demons' to start.` and prompt `Start demons now? [Y/n]`. On yes, start the just-written config.

#### 4.3.2 Existing config (file present)

1. Parse the existing file. On error, print a clear message with line number and exit.
2. Ask the user to choose from `Edit existing`, `Fresh start`, and `Abort`. `Edit existing` walks through every existing value as a default; `Fresh start` runs the new-config flow (overwriting); `Abort` exits without changes.
3. In `Edit` mode, after the per-task walkthrough, offer `Add a new task? [Y/n]`.
   New tasks may use the same detected starters as the new-config flow, excluding
   starter names already present. Then offer a numbered task-removal list.
   Removals may be entered as comma-separated numbers or names, and blank keeps
   all tasks.
4. Review and write, then offer to start (same as new config).

## 5. CLI

```
demons                         # Run all tasks from the nearest demons.toml.
demons init                    # Run the interactive init wizard.
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

- A 1-line header with: task name, status icon (`●` running, `✓` exited 0, `✗` exited N, `⏸` not yet started), and a clickable `[↻]` restart button on the right.
- A scrollback buffer (default 10,000 lines; configurable in v2).
- A pane-local text selection buffer derived from task output, used for deep
  scrollback selection and clipboard copy.
- A PTY-backed child process.
- Visible focus state: the selected pane's border is cyan in input mode and
  yellow in command mode.
- A one-line footer shows the current mode and available controls.
- A fixed-width button at the left of the footer displays and toggles the
  current mode.

### 6.3 Navigation

The configured leader key (default `Alt+J`) toggles between two modes.

**Input mode** (default after startup):

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
- Click on a pane: selects that pane without changing modes.
- Click on a `[↻]` button: restarts that pane.
- Mouse events inside a child that has enabled terminal mouse reporting are
  forwarded to that child.
- Press the leader or click the footer mode button → enter command mode.

**Command mode**:

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
- `/`: search the focused pane's scrollback and jump to the newest matching
  line.
- `n` / `N`: repeat the previous search older / newer in the focused pane.
- `r`: restart focused pane.
- `R`: restart all panes.
- `c`: clear the focused pane and its scrollback.
- `q` or `Ctrl+C`: quit (sends SIGTERM, waits 2s, then SIGKILL).
- Press the leader, click the footer mode button, or press `Esc` → return to
  input mode.
- Clicking a pane selects it without leaving command mode.

The leader is configurable in `[settings].leader`. Allowed values: `alt-j`
(default), `tab`, `ctrl-b`, `ctrl-q`, `ctrl-\`. The leader is intercepted in
input mode and cannot be sent to the child.

### 6.4 Process lifecycle

For each task:

- Spawn the command in a new session / process group (so SIGTERM propagates to the whole tree, not just the immediate child).
- Inherit parent env, then merge task-level `env` on top.
- Stream stdout + stderr into the pane's scrollback, interleaved.
- Manual restart (`r` or click `[↻]`): kill the process group, wait for it to exit, respawn.
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
- Should the wizard's `Edit` flow offer a "diff against current" view before writing? Default: no, just show the full file.
- Tabs for `N > 9` panes (v2).
- The v1 footer is always visible when the terminal is tall enough to spare
  one row; discoverability is more important than reclaiming that row.
