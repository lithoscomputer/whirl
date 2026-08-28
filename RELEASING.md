# Releasing Whirl

1. Update the version in `Cargo.toml` (`[workspace.package]`) and move the
   `CHANGELOG.md` "unreleased" heading to the release date. Commit.
2. Sanity-check locally: `mise run check`, then `mise run release` and
   inspect the archive it prints under `dist-release/`.
3. Tag and push:

   ```console
   $ git tag vX.Y.Z
   $ git push origin main vX.Y.Z
   ```

4. The Release workflow builds archives for macOS arm64, Linux x86_64,
   and Linux arm64 (each via `mise run release`, which rebuilds the shim
   from source with the pinned toolchain per the repository-owned-tasks
   ADR) and creates a **draft** GitHub release with the archives and
   their `.sha256` files.
5. Review the draft and publish it.
6. Update the Homebrew tap (lithoscomputer/homebrew-tap): copy
   `packaging/homebrew/whirl.rb` to the tap's `Formula/whirl.rb`, set the
   new version, and fill each `sha256` from the release's `.sha256`
   assets. Commit and push the tap.
7. Verify: `brew install lithoscomputer/tap/whirl` on a clean machine,
   then `whirl install` and `whirl examples/example-com.whirl`.
