# Changelog

All notable user-facing changes should be recorded here before a release.

## Unreleased

- Added pane-local mouse text selection with scrollback autoscroll and
  right-click copy/paste support.
- Added a tabbed runtime menu with Help, Tasks, Settings, and Exit sections.
- Added task-menu state restoration and cwd Tab completion/validation.
- Changed startup to command mode and replaced leader-key cycling with a picker.
- Changed `demons init` to open the configurator without starting tasks.
- Added task dependencies and `start_delay`, including dependent restarts.
- Added centered pane countdowns for delayed starts.
- Added clickable, wrapping footer command buttons and the Christmas color
  theme.
- Added close confirmation that works consistently from command mode and from
  input mode once the focused pane can no longer accept input.
