# Releasing Demons

Demons releases are manual for now. Keep release commits small and avoid mixing
feature work with version bumps.

## Versioning

Use SemVer. While Demons is `0.x`, incompatible user-facing behavior can ship in
a minor release, and patch releases should be limited to fixes and documentation.

Update the crate version with:

```sh
scripts/set-version.sh 0.2.0
```

The script updates `Cargo.toml` and the root `demons` entry in `Cargo.lock`,
then runs `cargo check --locked`.

## Checklist

1. Start from a clean working tree on the release branch.
2. Run `scripts/set-version.sh <version>`.
3. Update release notes or the GitHub release draft with user-visible changes.
4. Run `make release-check`.
5. Do a short manual TUI smoke test:

   - Run `demons init` in a temporary directory, add a task, save, and verify
     the generated `demons.toml`.
   - Run a two-task config, switch modes, open the menu with `?`, edit and
     discard a setting, and confirm the panes keep running.
   - Select and copy multi-line pane output with the mouse, including dragging
     beyond the top or bottom of the pane to scroll history.
   - Restart a task with a dependent task and confirm the dependent restarts
     after its configured delay.

6. Inspect the package contents if anything about packaging changed:

   ```sh
   cargo package --locked --allow-dirty --list
   ```

7. Commit the version bump:

   ```sh
   git commit -am "chore(release): v<version>"
   ```

8. Tag the commit:

   ```sh
   git tag -a "v<version>" -m "v<version>"
   ```

9. Publish:

   ```sh
   cargo publish --locked
   ```

10. Push the branch and tag:

   ```sh
   git push origin HEAD
   git push origin "v<version>"
   ```

11. Verify installation from crates.io after the index updates:

    ```sh
    cargo install demons --version <version> --locked
    ```
