# Releasing rosita

Releases are automated with [`dist`](https://opensource.axo.dev/cargo-dist/).
Pushing a version tag builds prebuilt binaries for every target and publishes a
GitHub Release with installers attached. No artifacts are built by hand.

## What ships

On a `vX.Y.Z` tag, `.github/workflows/release.yml` builds and attaches:

- `rosita` for `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- `rosita-installer.sh` (macOS/Linux)
- `.tar.xz` archives, SHA-256 checksums, and a `dist-manifest.json`

Windows (`x86_64-pc-windows-msvc`) is intentionally omitted: rosita is unix-only
today (parent-process detection, unix file modes, the tailnet provider). To add
it, cfg-gate those paths for Windows, then add the target + the `powershell`
installer back to `dist-workspace.toml` and re-run `dist init`.

The build matrix and installers live in [`dist-workspace.toml`](dist-workspace.toml).

## Cutting a release

1. **Bump the version** in `Cargo.toml` (`version = "X.Y.Z"`), then refresh the
   lockfile and confirm the build is green:

   ```bash
   cargo update -p rosita        # sync Cargo.lock to the new version
   cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all --locked
   ```

2. **Update the changelog**: rename `## Unreleased` in `CHANGELOG.md` to
   `## X.Y.Z — YYYY-MM-DD`. `dist` uses that section as the release notes.

3. **Preview** what the release will produce (no network, no build):

   ```bash
   dist plan
   ```

4. **Commit, tag, push**:

   ```bash
   git commit -am "release: vX.Y.Z"
   git tag vX.Y.Z
   git push && git push --tags
   ```

   The tag push triggers the release workflow; watch it with
   `gh run watch` (or the Actions tab). When it finishes, the GitHub Release and
   its installers are live, and the `releases/latest/...` installer URLs in the
   README resolve.

## Updating `dist` itself

The CI-pinned version is `cargo-dist-version` in `dist-workspace.toml`. To move
to a newer `dist`, install it locally and re-run init so the workflow is
regenerated against the new version:

```bash
cargo install cargo-dist@<new-version> --locked
dist init --yes        # rewrites dist-workspace.toml + .github/workflows/release.yml
```

Commit the regenerated `release.yml` alongside the version bump.

## Not yet enabled (one-step adds)

- **`cargo install rosita`** (crates.io): add `publish-jobs = ["cargo"]` (or run
  `dist init` and enable the crates.io publish), add a `CARGO_REGISTRY_TOKEN`
  repo secret, and the release will `cargo publish`. Until then, the from-source
  path is `cargo install --git https://github.com/elleryfamilia/rosita`.
- **Homebrew tap** (`brew install`): create a `homebrew-rosita` tap repo, add
  `installers = [..., "homebrew"]` plus the tap + token to `dist-workspace.toml`,
  and re-run `dist init`.
