# Releasing

Only maintainers cut releases. The `release.yml` workflow does the build, signing and
attestation; the maintainer's job is to choose the version, make sure `main` is green, and push the
tag.

## Steps

1. **Pick the version.** Follow Semantic Versioning. While the major version is 0, a minor bump may
   break compatibility.
2. **Update the version and changelog.** Set `version` in `Cargo.toml` and move the `[Unreleased]`
   entries in `CHANGELOG.md` under a new heading with today's date.
3. **Check the release locally.**
   ```sh
   cargo fmt --all -- --check
   cargo test --locked --release
   cargo clippy --locked --all-targets -- -D warnings
   cargo deny check advisories bans licenses sources
   cargo audit
   ```
4. **Merge the release pull request** after review. Confirm that the `Rust checks`, `MSRV 1.89`
   and supply-chain jobs passed on the merge commit.
5. **Tag the merge commit.**
   ```sh
   git tag -a v0.1.0 -m "transcode v0.1.0"
   git push origin v0.1.0
   ```
   The tag must equal `v` plus the `version` in `Cargo.toml`. The workflow fails otherwise.
6. **Check the workflow run.** The release workflow runs the tests, builds, signs and attests. Its
   assets appear on the GitHub release page. Verify them with the commands in
   [SUPPLY-CHAIN.md](SUPPLY-CHAIN.md#release-provenance).

## Rules

- Never create a tag from a fork or a branch other than the default branch.
- Do not edit a published release. If it is wrong, publish a new version.
- Recommended: resolve the patent and licensing questions in the README before releasing binaries
  that contain codecs. Those binaries are the first thing a public release distributes.
