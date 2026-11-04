# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/) once it reaches 1.0.

## [Unreleased]

### Changed

- The binary uses the library's codec modules instead of compiling its own copies.
- `Cargo.toml` records the license, repository, readme and keywords, and sets `rust-version = 1.89`.
- The library and binary are built with `#![forbid(unsafe_code)]`.

### Fixed

- Debug builds no longer panic on the AV1 boolean and coefficient decoding paths. Release
  output is unchanged.
- `Cargo.lock` now includes the `image-webp` dev-dependency, so `--locked` builds succeed.

## [0.1.0]

First version recorded in this repository. Contains the codecs listed in the README's status table.
