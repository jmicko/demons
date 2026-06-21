# Changelog

All notable user-facing changes should be recorded here before a release.

## Unreleased

- Added `schema_version = 1` to `demons.toml`; unversioned valid configs are
  normalized automatically after validation.
- Changed `demons init` and interactive startup to recover parseable broken
  configs into an editable draft with red blocking problem markers, gold
  recovery markers, and an Exit-tab problem list.
- Added recovery for bare missing config assignment values such as `command =`;
  they open in the menu as empty fields with blocking problem markers.
- Changed unrecoverable malformed TOML in the configurator to open a fresh draft
  that overwrites the broken file only when saved.
- Fixed recovery warnings so generic root notices do not linger as menu
  problems, while concrete ignored keys still appear with useful locations.
- Changed runtime config saves so added, removed, or renamed tasks rebuild and
  restart the task set in place instead of requiring a Demons restart.
- Changed the task environment editor from a comma-separated text field to a
  nested key/value row editor with add, rename, value edit, and delete actions.
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
- Added clickable, wrapping footer command buttons and the Christmas color
  theme.
- Added close confirmation that works consistently from command mode and from
  input mode once the focused pane can no longer accept input.
