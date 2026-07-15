# Demons - Specification

## 1. Overview

Demons is a single-binary CLI for running a project's long-running development
commands and optional shell panes side-by-side in one terminal. Every pane is
backed by a real PTY. Run `demons` from a project root and every pane declared in
`demons.toml` starts at once.

Demons is intentionally minimal: it is **not** a session manager, a process supervisor, a build system, or a tmux replacement. It exists only while your dev session is active.

## 2. Goals

- One command (`demons`) starts the entire dev stack defined in `demons.toml`.
- One command (`demons init`) opens the configurator without starting tasks.
  After that, the user should rarely need to touch the file directly.
- Real PTYs with VT100/xterm-compatible rendering (colors, REPLs, and common
  TUI apps work).
- Real keyboard and mouse navigation.
- Restart crashed or running tasks on demand (`r`).
- Restart configured tasks and their dependents when watched files change.
- Single static-ish binary, no runtime dependencies.
- Unix-only (Linux + macOS). Windows is explicitly out of scope.

## 3. Non-Goals

- Session persistence / detach-reattach.
- Production process supervision (no daemon, no health checks, no auto-restart loops).
- Distributed / remote task execution.
- Build orchestration (use `make` / `just` / `cargo` for that).
- Windows support.
- Plugin or extension system.
- Automatic pane output logging to file. Manual scrollback
  export is available with `S`.

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
# Config schema version, separate from the Demons app/crate version.
# Current unversioned configs are treated as the current schema and are
# normalized after they successfully validate. Older configs migrate to v5.
schema_version = 5

# Optional. Demons-level settings.
[settings]
# Layout strategy. "grid" (default) picks the closest-to-square arrangement
# based on terminal aspect ratio. A future release may add "tabs".
layout = "grid"
# Leader key to toggle command mode.
# Options: "alt-j" (default), "alt-backtick", "tab", "ctrl-b", "ctrl-q", "ctrl-\\".
leader = "alt-j"
# Double/triple-click selection threshold in milliseconds.
multi_click_ms = 500
# Reserved for a future release. It must remain false.
logging = false
# Optional project-scoped MCP access: "off" (default), "read_only", or "full".
mcp_access = "off"
# Show a compact MCP activity row while access is enabled.
mcp_status_bar = true
# Generated and managed by Demons when MCP access is enabled.
# mcp_scope_id = "3f4a7f63-2492-477a-ae7f-92bffab78fa4"
# File watcher backend: "auto" (default), "native", or "polling".
watch_mode = "auto"
# Polling cadence when polling is selected or automatic fallback is active.
watch_poll_interval = "1s"

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
depends_on = []
# Optional. Delay after dependencies have started. Supports ms, s, m, and h.
start_delay = "3s"
# Optional literal files or directories, relative to cwd or absolute.
# Directories are watched recursively.
watch = ["src", "Cargo.toml"]
# Optional files or directory trees to exclude. These may not exist yet.
watch_ignore = ["target", "src/generated"]
# Optional trailing debounce after the newest change. Default: 250ms.
watch_delay = "250ms"
# Optional. Run mode "run-on-change" — task only runs when watched files
# change, then exits. Reserved; not implemented.
# run_on_change = ["src/**/*.rs"]
# Optional. Restart the task at this interval. Reserved; not implemented.
# repeat = "1s"

# Optional. Regular shell panes. These start the user's $SHELL directly.
[[terminal]]
name = "scratch"
cwd = "."
env = { RUST_LOG = "debug" }
```

Validation rules (enforced at startup, fail loudly):

- `schema_version` must be `5` for this release. Missing `schema_version` is
  treated as the current schema for compatibility with existing configs.
  Older supported schema versions migrate to version 5.
- At least one `[[task]]` or `[[terminal]]` is required.
- Task and terminal `name` values are required and unique per file.
- `command` is required and non-empty.
- `cwd` must be a directory on disk at startup.
- `depends_on` entries must name existing tasks, cannot include the task
  itself, and cannot form dependency cycles.
- `start_delay` must be a non-negative integer with an optional unit of `ms`,
  `s`, `m`, or `h`; no unit means seconds.
- Every `watch` entry must be a unique, existing file or directory. Relative
  paths resolve from the task's `cwd`; directories are recursive.
- `watch_ignore` entries must be unique but may be missing. An ignored
  directory excludes its descendants.
- `watch_delay` must be between `25ms` and `60s`. The default is `250ms`.
- `settings.watch_mode` must be `auto`, `native`, or `polling`, and
  `settings.watch_poll_interval` must be between `250ms` and `60s`.
- `settings.multi_click_ms` must be between 150 and 1000 milliseconds.
- `settings.mcp_access` must be `off`, `read_only`, or `full`. Read-only and
  full access require a valid UUID in `settings.mcp_scope_id`; the
  configurator generates it when needed.
- Unknown keys are an error (no silent ignoring — the configurator owns the schema).
- Reserved fields are parseable so future files have a stable schema, but
  schema v5 rejects `logging = true` and any task that sets `run_on_change` or
  `repeat`. Reserved behavior is never silently ignored.

### 4.3 Configurator

`demons init` opens the configurator and does not start tasks. Without an
explicit `--config`, it edits the nearest existing `demons.toml` in the current
directory or its parents; if none exists, it creates `./demons.toml` when the
user saves. If stdin or stdout is not a TTY, `demons init` errors out:
`demons init requires an interactive terminal`.

If an existing config is parseable TOML but schema-invalid or contains
validation errors, the configurator recovers the understood fields into an
editable draft. Red `!` markers show problems that block saving. Gold `!`
markers show fields Demons repaired or ignored for review. Markers bubble from
field to task to tab, and the Exit tab lists all problems with jump targets. The
original file is not rewritten while red problems remain; after all red
problems are fixed, writing happens only when the user saves. Bare missing
assignment values such as `command =` recover as empty strings and then surface
as field-level problems. If TOML is too malformed to recover structurally, the
configurator opens a fresh draft that overwrites the broken file only if the
user saves. Unsupported future `schema_version` values remain hard errors.

When regular `demons` startup finds recoverable config problems in an
interactive terminal, it opens the configurator without starting tasks. Saving a
valid config starts the pane set in the same session. Saving task-list changes
while panes are already running reconciles the draft with the active session:
compatible panes retain their processes and scrollback, removed panes stop,
added panes start, and the selected restart policy applies only where requested.
Temporary session terminals are preserved.

The runtime menu is opened with `?` in command mode or by clicking the footer's
`? menu` button. The menu has top tabs:

- **Help** — command reference.
- **Tasks** - configured task and terminal list. Enter or click a task to edit
  name, command, cwd, env, dependencies, start delay, watched paths, ignored
  paths, and watch delay. Persistent terminals expose name, cwd, and env.
  Environment variables use a nested key/value list with add, key edit, value
  edit, and delete actions. Dependencies are selected from a checkbox list of
  other tasks. Working-directory edits validate immediately and support Tab
  completion for directories relative to the config file. Watch path editors
  support files and directories relative to the task cwd.
- **Settings** — app-level settings such as the leader key,
  double/triple-click timing, project-scoped MCP access, and MCP activity-bar
  visibility, plus watcher mode and polling interval.
- **Exit** — discard, save without restarting, save and restart affected, save
  and restart all, and a Problems section when the draft has config problems.
  In `demons init`, save/discard closes the configurator without starting
  tasks.

Keyboard behavior follows common TUI menu conventions: arrows move, Enter
activates, Space toggles dependency checkboxes, Left/Right adjust sliders, Esc
backs out one level, and text fields support cursor movement and basic line
editing. Tab completes directories while editing a working directory and files
or directories while editing a watch path.

### 4.4 Project-scoped MCP integration

On Unix platforms, Settings exposes three MCP access levels:

- **Off**: no control listener. Saving removes a registration previously
  managed by Demons without modifying a user-owned entry.
- **Read only**: allows project/pane discovery, bounded history reads, literal
  history search, output/status waits, command completion waits, and synthetic
  TUI capture.
- **Full**: also allows visible agent command panes, explicit pane input,
  interrupts, task restarts, and closing agent-owned command panes.

Saving an enabled level writes a managed `mcp_servers.demons` entry to
`.codex/config.toml` beside the active `demons.toml`. The generated stdio
command includes the config's absolute path and `mcp_scope_id`. Discovery must
match both values exactly; if more than one live instance matches, callers must
select an explicit instance ID. Demons never scans or exposes unrelated
project scopes through that adapter.

Codex ignores project-local configuration until the project is trusted, and a
running Codex process must restart after registration changes. Demons does not
depend on that restart for enforcement: selecting a lower access level stops
or restricts the live control listener immediately, and each request is
authorized again inside the running TUI. The Unix socket and discovery files
must be private to the current user.

History APIs return process text from the pane history model and exclude
application UI composition. Visual diagnosis uses an explicit synthetic PNG
capture of the terminal cell grid. `workspace` capture omits an active menu or
dialog; `full` capture includes it. Capture is bounded by terminal dimensions,
font lookup is lazy, and the response reports rendering metadata and missing
glyphs.

When `settings.mcp_status_bar` is true and MCP access is enabled, the runtime
reserves one row above the command footer for the latest privacy-filtered
activity summary. A right-edge arrow expands up to four recent entries above
that row. Expansion grows upward so the arrow stays at the same terminal
coordinate. The in-memory history is bounded and never includes command text,
input contents, search terms, or pane output.

## 5. CLI

```
demons                         # Run all tasks from the nearest demons.toml.
demons init                    # Open the configurator without starting tasks.
demons --config <path>         # Use a specific config file.
demons -c <path>               # Short form.
demons --no-watch              # Disable file watching for this run.
demons --config <path> mcp serve --scope <uuid>
                                # Managed stdio adapter; normally generated.
demons --help                  # Show usage.
demons --version               # Print version.
```

Reserved for a future release:

```
demons "cmd1" "cmd2"           # One-off multi-pane run, no config.
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
- For `N > 9` the current release still grids them. A future release may add a
  tabbed fallback.
- Fullscreen mode shows only the focused pane in the full pane area. Other
  tasks keep running and keep their last PTY size until grid mode is restored.

### 6.2 Pane

Each pane has:

- A 1-line header with: pane name, status icon (`●` running, `✓` exited 0,
  `✗` exited N, `⏸` not yet started, `⏱` waiting on dependencies or delay),
  and a clickable `[↻]` restart button on the right.
- A scrollback buffer capped at 10,000 rows. A future release may make the cap
  configurable.
- A pane-local text selection buffer derived from task output, used for deep
  scrollback selection and clipboard copy.
- Mouse selection supports drag selection, double-click word selection with
  word-granularity dragging, and triple-click visible-line selection with
  line-granularity dragging within one pane at a time.
- A PTY-backed child process.
- Visible focus state: the selected pane's border is green in input mode and
  gold in command/search modes.
- A footer shows the current mode and available controls, wrapping command
  buttons to additional lines when the terminal is narrow.
- An optional MCP activity row sits immediately above the footer. Its fixed
  right-edge arrow expands and collapses bounded history upward.
- A fixed-width button at the left of the footer displays and toggles the
  current mode.
- Footer command buttons are clickable. Paired shortcuts such as `y` / `Y` and
  `r` / `R` are rendered as separate buttons. `x close` is shown only for a
  focused temporary terminal pane.

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
- Right-click or `Ctrl+Shift+C`: copies the current selection to the internal
  and system clipboards. Copies up to 512 KiB also use OSC 52 for compatible
  host terminals.
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
- `y`: copy the current selection; no action is available without a selection.
- `Y`: copy the focused pane's full scrollback.
- `S`: save the focused pane's full scrollback to a temp log file and copy the
  file path.
- `/`: open focused-pane search mode. In search mode, typing updates the newest
  match immediately, Enter moves to the previous match, Shift+Enter moves to
  the next match, Esc leaves search mode, and clicking another pane or pressing
  Tab/Shift+Tab retargets the active search.
- `t`: add a temporary shell pane for the current session.
- `x`: close the focused temporary shell pane. Configured panes cannot be
  removed with this shortcut.
- `r`: restart focused pane and its dependents.
- `R`: restart all panes.
- `c`: clear the focused pane and its scrollback.
- `?`: open the menu.
- `q` or `Ctrl+C`: show quit confirmation. Press `q` or `Ctrl+C` again to
  start the shutdown sequence described below, or `Esc` to cancel.
- Press the leader or click the footer mode button → return to input mode.
- Clicking a pane selects it without leaving command mode.
- Clicking a command footer button runs that button's visible action.

The leader is configurable in `[settings].leader`. Allowed values: `alt-j`
(default), `alt-backtick`, `tab`, `ctrl-b`, `ctrl-q`, `ctrl-\`. The leader is
intercepted in input mode and cannot be sent to the child.

### 6.4 Process lifecycle

For each pane:

- Spawn each command or shell in a new session / process group so lifecycle
  signals reach the whole tree, not just the immediate child.
- Inherit parent env, then merge the task or terminal `env` table on top.
- Stream stdout + stderr into the pane's scrollback, interleaved.
- Manual restart (`r` or click `[↻]`): kill the process group and any dependent
  task process groups, wait for exits, then respawn them in dependency order.
  Each dependent's `start_delay` starts after its dependencies have started.
- While a task is waiting on `start_delay`, the pane body shows a countdown to
  launch.
- A watched change restarts the owning task and the union of its transitive
  dependents. An exited task starts again. Each task applies a trailing
  `watch_delay`, and repeated paths are coalesced before one restart wave.
- `auto` watcher mode uses native OS events, falls back to metadata polling if
  registration fails, and uses a low-frequency sentinel on filesystems known
  to lose native events. A detected miss switches that task to polling for the
  rest of the session. `native` has no fallback; `polling` starts directly in
  polling mode. Event and restart queues are bounded.
- Polling fingerprints file type, size, modification time, identity, and
  symlink target. Ignored directory trees are pruned before traversal, scans
  wait the configured interval after completion, and each task snapshot is
  limited to 250,000 entries.
- `run_on_change` and `repeat` remain schema-reserved.
- On Demons quit: send SIGTERM to task process groups and SIGHUP to shell pane
  process groups, wait 2s, then send SIGKILL to any that are still alive. Exit
  only when all children are reaped.
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
- **File watching**: `notify` for native events and `walkdir` for bounded,
  ignore-aware polling fallback.
- **Build**: standard `cargo build --release`. A `Makefile` provides `make build` and `make install` targets.
- **Distribution**: `cargo install --path .` or `make install` to copy the release binary into `~/.cargo/bin/`.

## 8. Out of scope (recap)

- Session persistence.
- Detach / reattach.
- `run_on_change`.
- `repeat`-interval tasks.
- Automatic pane output logging to file. Manual scrollback export is available.
- Distributed execution.
- Windows.
- Daemon / background mode.
- Plugin / extension API.

## 9. Example `demons.toml`

```toml
schema_version = 5

[settings]
layout = "grid"
leader = "alt-j"
multi_click_ms = 500
logging = false
mcp_access = "off"
mcp_status_bar = true
watch_mode = "auto"
watch_poll_interval = "1s"

[[task]]
name = "server"
command = "cargo run"
cwd = "."
depends_on = []
watch = ["src", "Cargo.toml"]
watch_ignore = ["target"]
watch_delay = "250ms"

[task.env]

[[task]]
name = "web"
command = "npm run dev -- --host 0.0.0.0"
cwd = "./web"
depends_on = []

[task.env]
BROWSER = "none"

[[task]]
name = "tail-logs"
command = "tail -f /tmp/app.log"
cwd = "."
depends_on = []

[task.env]

[[terminal]]
name = "scratch"
cwd = "."
```

## 10. Open questions (low priority)

- Should `r` on a running pane prompt for confirmation, or just kill and respawn immediately? Default: immediate.
- Should the clickable restart button be visible always, or only on mouse hover? Default: always visible.
- Should the configurator offer a diff against the saved file before writing?
  Default: no, keep save flow direct.
- Tabs for `N > 9` panes.
- The footer is always visible when the terminal is tall enough to spare
  one row; discoverability is more important than reclaiming that row.
