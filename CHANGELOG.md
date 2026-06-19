# Changelog

All notable user-facing changes should be recorded here before a release.

## Unreleased

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
- Changed `demons init` to open the configurator without starting tasks.
- Added task dependencies and `start_delay`, including dependent restarts.
- Added centered pane countdowns for delayed starts.
- Added clickable, wrapping footer command buttons and the Christmas color
  theme.
- Added close confirmation that works consistently from command mode and from
  input mode once the focused pane can no longer accept input.
