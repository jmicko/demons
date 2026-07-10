# Release Audit for 0.3.0

This checklist tracks the release-gate review performed on 2026-07-09. Mark an
item complete only after its implementation, focused regression tests, and
relevant broader checks pass.

## Release Blockers

- [x] **1. Prevent stale PTY events from targeting rebuilt panes.**
  - Severity: high.
  - Evidence: `stop_tasks_for_rebuild` discards old runtimes, while replacement
    runtimes restart their generation counter at zero. Late events are keyed
    only by pane index and generation, so an old generation-1 event can match a
    new generation-1 process.
  - Risk: a stale exit event can take the replacement PID, remove it from the
    process registry, mark the new pane exited, and potentially leave the new
    process unmanaged.
  - Required: process identities must remain unique across rebuilds, and tests
    must prove stale output/exit events cannot mutate replacement runtimes.
  - Completed: process generations now come from one app-wide monotonic
    allocator. `rebuilt_runtimes_reject_events_from_previous_generation` covers
    stale output and exit delivery after replacement; all 194 tests pass.

- [x] **2. Honor structural save choices and preserve session terminals.**
  - Severity: high.
  - Evidence: adding a configured pane and choosing "Save without restarting"
    restarted existing tasks and removed the session-only terminal in a live
    test.
  - Required: preserve compatible running runtimes and session terminals; start
    added panes, stop removed panes, and obey Save affected/all/without restart
    semantics without silently rebuilding everything.
  - Completed: runtime reconciliation now preserves matching configured panes
    and session terminals, remaps live event routing/focus/selection, and applies
    only added/removed/changed deltas. Unit coverage handles add/remove/move, and
    a live Save without restarting test retained task PIDs and shell state.

- [x] **3. Finalize process exit only after PTY output reaches EOF.**
  - Severity: medium.
  - Evidence: independent output and wait threads can deliver `Exited` before
    final output.
  - Risk: final bytes may appear after the exit marker or be discarded by an
    immediate restart.
  - Required: coordinate reader EOF and child exit before finalizing a pane,
    with tests for final output ordering and immediate restart.
  - Completed: the reader now emits an explicit EOF event and child status is
    held until both halves complete. Regressions cover either event order and
    prove a requested restart cannot replace the pane before its final output.

- [x] **4. Update the audited dependency set.**
  - Severity: release hygiene.
  - Evidence: `cargo audit -D warnings` rejects `anyhow 1.0.102` for
    RUSTSEC-2026-0190. The patched release is 1.0.103.
  - Required: update the lockfile, run the full release check, and make the
    denied-warning audit pass.
  - Completed: `anyhow` is locked to 1.0.103. The full locked test suite passes
    and `cargo audit -D warnings` reports no vulnerabilities or warnings.

## Correctness and Security

- [x] **5. Harden bracketed-paste forwarding.**
  - Severity: medium; final severity depends on an end-to-end reproduction.
  - Evidence: `paste_text_to_task` wraps an `Event::Paste` payload without
    neutralizing embedded bracketed-paste start/end sequences.
  - Risk: crafted clipboard content may end protected paste mode early and turn
    trailing bytes into live shell input.
  - Required: define and test safe handling of embedded paste delimiters without
    breaking ordinary multiline and Unicode paste.
  - Completed: child-bound paste payloads neutralize embedded start/end
    delimiters. Host paste events are held for a 25 ms quiet window so bytes
    Crossterm splits after a forged end delimiter are discarded instead of
    becoming live key events. Tests cover the split-event attack plus unchanged
    ordinary multiline and Unicode input; all 203 tests and clippy pass.

- [x] **6. Make deep scrollback terminal-correct and memory-conscious.**
  - Severity: medium.
  - Evidence: `TextHistory` decodes each PTY chunk independently with lossy
    UTF-8, treats each Unicode scalar as one cell, supports only a small CSI
    subset, and duplicates the 10,000-line `vt100` history.
  - Risk: split UTF-8, wide/combining text, and cursor-based redraws can diverge
    after the renderer switches to deep history; many wide panes can consume
    substantial memory.
  - Required: use a streaming, width-aware authoritative history model or define
    a narrower supported behavior with regression tests and bounded memory.
  - Completed: the 10,000-row line-oriented archive now decodes UTF-8 across
    chunks, tracks terminal columns for wide and combining text, preserves ANSI
    styles, and resnapshots cursor-addressed screens from `vt100`. Duplicate
    fully styled parser history is capped at 512 rows. Unit tests cover each
    boundary; a live 700-line test retained colors and Unicode at line 1,
    copied a deep multi-page selection, and shut down without descendants.

- [x] **7. Recover malformed current-schema declarations without losing data.**
  - Severity: medium.
  - Evidence: `0`, string-valued, and bare `schema_version` declarations can
    bypass field salvage or force a fresh draft.
  - Required: recover invalid current/legacy declarations with a warning while
    continuing to reject valid numeric future schema versions.
  - Completed: zero, negative, boolean, string-valued, and bare declarations
    now produce a gold root warning and preserve recoverable tasks without
    rewriting the source. Numeric future versions still hard-fail. Focused and
    full-suite tests cover both sides of the boundary.

- [ ] **8. Complete terminal-pane management.**
  - Severity: medium.
  - Evidence: persistent terminal `env` is supported by the schema but cannot be
    edited in the configurator. Session-only terminals cannot be closed
    individually and do not appear in the Tasks menu.
  - Required: expose persistent terminal environment editing and provide a clear
    close/remove workflow for session terminals.

## Hardening and Polish

- [ ] **9. Bound OSC 52 writes for selected text.**
  - Severity: low.
  - Evidence: full-history copy has a 512 KiB OSC 52 limit, but selection copy
    always synchronously encodes and writes the full selection.
  - Required: apply a consistent limit and preserve the internal/system
    clipboard fallback with a clear notice.

- [ ] **10. Tighten start-delay validation and scheduling.**
  - Severity: low.
  - Evidence: zero-valued delays return before unit validation, so values such
    as `0anything` are accepted. Very large durations may overflow `Instant`
    scheduling on some platforms.
  - Required: validate units regardless of amount and reject delays that cannot
    be safely scheduled.

- [ ] **11. Clip every decorative scene to its assigned rectangle.**
  - Severity: low.
  - Evidence: skating pines use a helper that clips to the complete frame rather
    than the scene, allowing edge sprites to overwrite a pane border or neighbor.
  - Required: enforce scene-local clipping and add an edge-position regression
    test.

- [ ] **12. Synchronize user-facing documentation and help.**
  - Severity: low.
  - Evidence: README, specification, and in-app Help still describe `y` as
    copying visible text instead of the selection; Help omits `t`; the packaged
    example uses schema v1; and the specification still identifies itself as
    v0.1.
  - Required: make README, specification, in-app Help, examples, changelog, and
    release instructions agree with implemented 0.3.0 behavior.

- [x] **13. Stabilize detached-terminal hangup detection.**
  - Severity: low; discovered while running the item 7 full suite.
  - Evidence: polling a pipe with an empty event mask did not reliably surface
    `POLLHUP`, making both the regression and terminal-detach path timing
    dependent.
  - Completed: attachment probes now request `POLLIN` while still checking
    `POLLHUP`, `POLLERR`, and `POLLNVAL`. The regression passed 50 consecutive
    runs before the full suite and clippy were rerun.

## Final Verification

- [ ] Focused regression tests pass for every item above.
- [ ] `cargo fmt -- --check` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --all-targets --all-features --locked` passes on stable.
- [ ] The same test suite passes on Rust 1.88.
- [ ] `cargo build --release --locked` passes.
- [ ] `cargo audit -D warnings` passes.
- [ ] `cargo package --locked --allow-dirty` passes and contents are reviewed.
- [ ] Live TUI smoke tests cover structural saves, session terminals, selection,
  paste, restart, and shutdown without descendant processes remaining.
