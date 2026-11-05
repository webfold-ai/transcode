# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Changed

- Licensed under the Apache License, Version 2.0 (`LICENSE`). The MIT licence text and the
  `LICENSE-MIT` file are removed.
- The binary uses the library's codec modules instead of compiling its own copies.
- `Cargo.toml` records the license, repository, readme and keywords, and sets `rust-version = 1.89`.
- The library and binary are built with `#![forbid(unsafe_code)]`.
- README and docs/STANDARDS.md state the patent position of each format. The licence grants no rights under third-party patents.

### Fixed

- The JPEG and FLAC decoders no longer panic or allocate without bound on malformed input; regression tests are in `tests/fuzz_regressions.rs`.
- Debug builds no longer panic on the AV1 boolean and coefficient decoding paths. Release
  output is unchanged.
- Malformed input can no longer make the decoders panic, hang or over-allocate. Four cases found
  by fuzzing are fixed: a JPEG APP14 segment read past the end of the input, a FLAC metadata loop
  that never ended on truncated input, a SLAC header frame count that requested about 17 GB, and an
  AAC fill element with a zero escape byte that underflowed.
- `Cargo.lock` now includes the `image-webp` dev-dependency, so `--locked` builds succeed.

## [0.1.0]

First version recorded in this repository. Contains the codecs listed in the README's status table.
