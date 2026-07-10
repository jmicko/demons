# Releasing Demons

Demons releases are manual for now. Keep release commits small and avoid mixing
feature work with version bumps.

## Versioning

Use SemVer. While Demons is `0.x`, incompatible user-facing behavior can ship in
a minor release, and patch releases should be limited to fixes and documentation.

Update the crate version with:

```sh
scripts/set-version.sh 0.3.0
```

The script updates `Cargo.toml` and the root `demons` entry in `Cargo.lock`,
then runs `cargo check --locked`.

## Crates.io Setup

Before publishing from a machine for the first time:

1. Log in with a crates.io API token:

   ```sh
   cargo login
   ```

2. Confirm the crate name is available and the package metadata is correct:
   `description`, `license`, `readme`, `repository`, `homepage`, `keywords`,
   and `categories`.
3. Run `cargo package --locked --list` and check that no local-only files are
   included.

## Checklist

1. Start from a clean working tree on the branch intended for release. Confirm
   its history and target branch before changing the version.
2. Run `scripts/set-version.sh <version>`.
3. Move the relevant `CHANGELOG.md` entries from `Unreleased` to the target
   version and date, then use those notes for the GitHub release draft.
4. Run `make release-check`.
5. Run a crates.io publish dry run after the version bump:

   ```sh
   cargo publish --dry-run --locked
   ```

6. Do a short manual TUI smoke test:

   - Run `demons init` in a temporary directory, add a task, save, and verify
     the generated `demons.toml`.
   - Run a two-task config, switch modes, open the menu with `?`, edit and
     discard a setting, and confirm the panes keep running.
   - Select and copy multi-line pane output with the mouse, including dragging
     beyond the top or bottom of the pane to scroll history.
   - Add a temporary terminal with `t`, use it in input mode, then focus it in
     command mode and close it with `x`. Confirm configured panes stay running.
   - Add a persistent terminal and environment variable in the Tasks tab, save,
     and confirm the shell receives that value.
   - Restart a task with a dependent task and confirm the dependent restarts
     after its configured delay.
   - Quit with two `q` or `Ctrl+C` confirmations and verify no child process
     groups remain.

7. Inspect the package contents if anything about packaging changed:

   ```sh
   cargo package --locked --allow-dirty --list
   ```

8. Commit the version bump:

   ```sh
   git commit -am "chore(release): v<version>"
   ```

9. Tag the commit:

   ```sh
   git tag -a "v<version>" -m "v<version>"
   ```

10. Push the release commit and tag:

   ```sh
   git push origin HEAD
   git push origin "v<version>"
   ```

11. Publish the exact pushed source:

   ```sh
   cargo publish --locked
   ```

12. Verify installation from crates.io after the index updates:

    ```sh
    cargo install demons --version <version> --locked
    ```
