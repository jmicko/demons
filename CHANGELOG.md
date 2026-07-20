# Changelog

All notable user-facing changes should be recorded here before a release.

## Unreleased

## 0.4.0 - 2026-07-20

- Added config schema v5 with per-task literal file and recursive directory
  watches, ignored paths, and trailing restart debounce.
- Added native file-system events with automatic polling fallback, configurable
  watcher mode and polling cadence, ignored-tree pruning, and bounded queues.
- Added task-menu editors with file/directory Tab completion for watched and
  ignored paths, plus live watcher settings that revert on Discard.
- Changed relative watched and ignored paths to resolve from the project config
  directory while keeping the task working directory as the editor default.
- Added `--no-watch` to disable all configured watchers for one session.
- Changed watched restarts to use the existing dependency graph and start
  delays, including restarting exited tasks and coalescing change bursts.
- Fixed watcher registration refreshes so file events arriving during a refresh
  are preserved instead of leaving a task stopped without a replacement.
- Added config schema v4 with Off, Read only, and Full project-scoped MCP
  access levels managed from the Settings tab.
- Added an optional one-line MCP activity bar with a fixed-position control
  that expands bounded, privacy-filtered history upward.
- Fixed `list_instances` so every instance returned by discovery records the
  call in its MCP activity bar.
- Added a Codex project registration that is bound to the exact active config
  path and generated project scope, refuses to replace user-owned entries, and
  shuts off live access immediately when disabled.
- Added MCP tools for bounded pane history, literal search, output/status waits,
  pane status, task control, visible agent command panes, explicit input, and
  synthetic PNG captures of the current terminal layout.
- Fixed incremental MCP reads and waits so output written into the terminal's
  mutable tail line is not skipped and old history is not repeatedly scanned.
- Reduced idle MCP adapter overhead, bounded concurrent local clients, and made
  fragmented control frames survive read timeouts without losing alignment.
- Marked MCP mutations for Codex approval handling and restricted local control
  sockets and discovery metadata to the current user.
- Changed MCP uninstall to remove an otherwise empty managed Codex config and
  avoid creating `.codex` when no registration exists.
- Added an explicit save-time repair for a zero-byte `.codex` file while
  leaving nonempty files, symlinks, cancellation, and discarded edits intact.
- Fixed terminal resizing so the layout redraws immediately without weakening
  detached-terminal cleanup.

## 0.3.0 - 2026-07-09

- Added config schema v2 with persistent `[[terminal]]` shell panes, plus a
  session-only `t terminal` command for ad-hoc shells.
- Added `x close` for the focused session-only terminal while protecting
  configured panes from accidental removal.
- Changed unversioned and schema v1 configs to migrate to schema v2 after
  validation.
- Changed `demons init` and interactive startup to recover parseable broken
  configs into an editable draft with red blocking problem markers, gold
  recovery markers, and an Exit-tab problem list.
- Added recovery for bare missing config assignment values such as `command =`;
  they open in the menu as empty fields with blocking problem markers.
- Changed unrecoverable malformed TOML in the configurator to open a fresh draft
  that overwrites the broken file only when saved.
- Fixed recovery warnings so generic root notices do not linger as menu
  problems, while concrete ignored keys still appear with useful locations.
- Changed runtime config saves to reconcile panes in place, preserving compatible
  task processes, scrollback, and session terminals while applying the selected
  restart policy only where requested.
- Changed task and terminal environment editing to a nested key/value row editor
  with add, rename, value edit, and delete actions.
- Changed `y` to copy only the current selection and added a system clipboard
  fallback. Large copies remain available internally and through the system
  clipboard without sending oversized OSC 52 escapes.
- Expanded pane scrollback to a bounded, streaming, Unicode-width-aware archive
  that preserves ANSI colors and cursor-redrawn output during deep scrolling.
- Hardened bracketed paste handling against embedded delimiters and split host
  events while preserving ordinary multiline and Unicode paste.
- Fixed stale PTY events and final-output races during restart and config saves.
- Changed shell-pane shutdown to use SIGHUP and fixed detached-terminal cleanup.
- Tightened start-delay validation and protected scheduling from duration
  overflow.
- Fixed no-op command edits so direct command arrays are not rewritten as shell
  strings.
- Hardened saved scrollback logs on Unix by using a per-user temp directory and
  rejecting symlinked or incorrectly owned log directories.
- Raised the minimum supported Rust version to 1.88 and updated the terminal UI
  stack to `ratatui` 0.30 / `crossterm` 0.29.

## 0.2.0 - 2026-06-19

- Added pane-local mouse text selection with scrollback autoscroll and
  right-click copy/paste support.
- Added double-click word selection, triple-click line selection, word/line
  drag expansion, and configurable multi-click timing.
- Added a tabbed runtime menu with Help, Tasks, Settings, and Exit sections.
- Added task-menu state restoration and cwd Tab completion/validation.
- Changed startup to command mode and replaced leader-key cycling with a picker.
- Changed search mode so text input accepts `n`, mouse movement keeps search
  open, and Tab/Shift+Tab or clicking another pane retargets the active search.
- Added live search updates and current/total match counts in the search
  footer.
- Fixed search result rendering so visible-pane matches do not force scrollback
  layout.
- Fixed Vite-style home-clear redraws so the first scroll does not jump to
  stale pre-clear output.
- Changed the first pane-height of scrollback to render from the terminal
  parser, preserving colors and terminal layout while scrolling.
- Fixed drag-selection highlighting so it stays attached to parser-rendered
  scrollback while the pane auto-scrolls.
- Changed triple-click line selection to highlight the full pane row,
  including blank cells after the text.
- Changed double-click word selection so clicking blank space still enters
  word-selection mode for subsequent dragging.
- Changed `demons init` to open the configurator without starting tasks.
- Added task dependencies and `start_delay`, including dependent restarts.
- Added centered pane countdowns for delayed starts.
- Added clickable, wrapping footer command buttons and refreshed terminal
  colors.
- Added close confirmation that works consistently from command mode and from
  input mode once the focused pane can no longer accept input.
