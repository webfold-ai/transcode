# Supply chain

How transcode's dependencies, build inputs, CI and release artifacts are controlled, and how to
check them.

## Dependencies

- **Runtime:** none. The library and the binary use only `std`.
- **Dev-dependencies:** `image-webp` (MIT OR Apache-2.0), used only in tests to check the WebP
  encoder's output with an independent decoder. It and its transitive crates appear in
  `Cargo.lock`.
- **Fuzz crate:** `fuzz/` is a separate workspace with its own `Cargo.lock`. It depends on
  `libfuzzer-sys` and on the library by path. It is not built by the root workspace.
- **Lock file:** `Cargo.lock` is committed. CI builds with `--locked`, so a dependency change that
  is not reflected in the lock file fails the build.

## Policy checks (run in CI, and locally)

| Tool | Command | What it checks |
|---|---|---|
| cargo-audit | `cargo audit --deny warnings` | RustSec advisories against `Cargo.lock` |
| cargo-deny | `cargo deny check advisories bans licenses sources` | advisories, duplicate or wildcard versions, license allow-list, and that every source is crates.io |
| cargo-cyclonedx | `cargo cyclonedx --format json --override-filename transcode.sbom` | a CycloneDX SBOM, uploaded as a CI artifact for 90 days |

`deny.toml` allows only the MIT and Apache-2.0 licenses and no git sources. Any new git or
unknown-registry source is denied until a reviewed exception is added to that file.

The `supply-chain.yml` workflow also writes a small JSON evidence record
(`ptyktos.supply_chain.audit`) with the result of each job, in the same format caretaker expects.

## Pinned build inputs

- **GitHub Actions:** every `uses:` line is pinned to a full commit SHA, with the tag in a comment.
  Dependabot (`.github/dependabot.yml`) proposes updates to these pins weekly.
- **Rust toolchain:** `dtolnay/rust-toolchain` is pinned to a commit. The toolchain version itself is
  set per job (`stable`, `nightly`, or `1.89.0` for MSRV).
- **Tool installs in CI:** `cargo install --locked` for cargo-audit, cargo-deny (version 0.19.4)
  and cargo-cyclonedx, and cargo-fuzz.

### Verifying the pins

Resolve a tag to its commit before pinning, and use the peeled `^{}` line for annotated tags:

```sh
git ls-remote --tags https://github.com/actions/checkout 'refs/tags/v7.0.1*'
```

Pins in use on 2026-11-04 (resolved with `git ls-remote` at that date):

| Action | Tag | Commit |
|---|---|---|
| actions/checkout | v7.0.1 | 3d3c42e5aac5ba805825da76410c181273ba90b1 |
| actions/upload-artifact | v7.0.1 | 043fb46d1a93c77aae656e7c1c64a875d1fc6a0a |
| github/codeql-action | v4.38.2 | 2892aa5e19bbd11bc0cff5427e3b750a04d9e3c2 |
| actions/attest-build-provenance | v4.2.2 | 4d101475d8b20a2381f78447822ac1eab6504dd8 |
| sigstore/cosign-installer | v4.1.2 | 6f9f17788090df1f26f669e9d70d6ae9567deba6 |
| ossf/scorecard-action | v2.5.0 | fa7117db83c52d787ae5a89f350999701505d78e |
| actions/dependency-review-action | v4.9.0 | 2031cfc080254a8a887f58cffee85186f0e49e48 |
| dtolnay/rust-toolchain | master (not a release tag) | 683b588c487a69d01f6c6ddf40f036ff682cc2c8 |

`dtolnay/rust-toolchain` has no release tags, so it is pinned to the `master` commit that was
current when the pin was set. Update it on the same schedule as the others.

## Release provenance

Releases are produced only by `release.yml`, which runs when a `v*` tag is pushed. A release
contains:

- `transcode-vX.Y.Z-linux-x86_64.tar.gz`, the CLI binary with `LICENSE-MIT` and `README.md`,
- `SHA256SUMS`, with a sha256 for the tarball,
- `.sig` and `.cert` files from cosign keyless signing (identity: the release workflow at the tag),
- `.intoto.jsonl`, the SLSA build-provenance attestation.

How to verify a downloaded release (replace `v0.1.0` with the version you downloaded):

```sh
sha256sum -c SHA256SUMS

cosign verify-blob \
  --certificate transcode-v0.1.0-linux-x86_64.tar.gz.cert \
  --signature transcode-v0.1.0-linux-x86_64.tar.gz.sig \
  --certificate-identity-regexp '^https://github.com/webfold-ai/transcode/\.github/workflows/release\.yml@refs/tags/v0\.1\.0$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  transcode-v0.1.0-linux-x86_64.tar.gz

gh attestation verify transcode-v0.1.0-linux-x86_64.tar.gz --repo webfold-ai/transcode
```

Those commands are written from the workflow and have not yet been run against a published
release, because no release exists yet.

## Known gaps

- The release is Linux x86_64 only.
- The Rust toolchain used to build a release is whatever `dtolnay` installs for `1.89.0`, which is
  not itself pinned by hash.
- `cargo install` of the audit tools compiles them in CI on every run. That is slow but reproducible
  from the lock file of each tool.
- No crates.io publication is configured. Nothing is uploaded to a registry.
