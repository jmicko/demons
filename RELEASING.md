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
5. Inspect the package contents if anything about packaging changed:

   ```sh
   cargo package --locked --allow-dirty --list
   ```

6. Commit the version bump:

   ```sh
   git commit -am "chore(release): v<version>"
   ```

7. Tag the commit:

   ```sh
   git tag -a "v<version>" -m "v<version>"
   ```

8. Publish:

   ```sh
   cargo publish --locked
   ```

9. Push the branch and tag:

   ```sh
   git push origin HEAD
   git push origin "v<version>"
   ```

10. Verify installation from crates.io after the index updates:

    ```sh
    cargo install demons --version <version> --locked
    ```
