# Contributing to transcode

Thanks for looking at transcode. It is a from-scratch codec crate with no runtime
dependencies, so changes are checked closely for correctness and for the bit-exact
behaviour the tests depend on.

## Before you start

- Read [README.md](README.md), especially the patents and licensing section.
- Check [ROADMAP.md](ROADMAP.md) for what is planned and what is intentionally unfinished.
- For anything larger than a bug fix, open an issue first so the approach can be agreed.

## Development setup

```sh
cargo build --locked --all-targets
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

Install ffmpeg as well if you want the ffmpeg-backed tests to run rather than print `SKIP`.

Supply-chain checks, which CI also runs:

```sh
cargo deny check advisories bans licenses sources
cargo audit
```

Fuzzing needs nightly and cargo-fuzz:

```sh
cargo +nightly fuzz run fuzz_png --fuzz-dir fuzz -- -max_total_time=60
```

## What a good change looks like

- **Keep the output bit-exact.** Changes to a codec must not alter the bytes it produces unless
  that is the point of the change. If the output changes, say so in the pull request and update
  the test that pins it.
- **Add a test that fails before your change.** For a decoder, add a regression input to the
  fuzz corpus or a unit test, and include the input bytes or a generator.
- **No `unsafe`.** The crate is built with `#![forbid(unsafe_code)]`.
- **No new runtime dependencies** without an issue first. Dev-dependencies need the same
  review because they enter `Cargo.lock`.
- **Codecs need evidence.** A new encoder or decoder should come with an external check
  (for example an ffmpeg round-trip) where one exists, and a note on its patent status.

## Commits and pull requests

- Keep commits focused. Separate formatting-only commits from behaviour changes.
- Describe the change and why it is correct. Mention any output-format change.
- Fill in the pull request template.
- A maintainer reviews every change. Automated tooling does not approve or merge.

## Reporting bugs and security issues

- Ordinary bugs: open an issue with the bug report template.
- Security issues, including a decoder that panics, hangs or allocates without bound on
  crafted input: do **not** open a public issue. Follow [SECURITY.md](SECURITY.md).

## Code of conduct

Participation is governed by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

By contributing you agree that your contribution is licensed under the same terms as the
project (MIT; see [LICENSE-MIT](LICENSE-MIT)).
