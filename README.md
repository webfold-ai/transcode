# transcode

[![CI](https://github.com/webfold-ai/transcode/actions/workflows/ci.yml/badge.svg)](https://github.com/webfold-ai/transcode/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/webfold-ai/transcode/badge)](https://scorecard.dev/viewer/?uri=github.com/webfold-ai/transcode)

Pure-Rust audio, image and video codecs, written from scratch. The library uses only `std`:
no ffmpeg, libpng, libjpeg, zlib or other C libraries at runtime.

**Status: 0.1.0, pre-release.** Some formats are complete in both directions and others are
partial or encode-only. The per-format table below and [ROADMAP.md](ROADMAP.md) are the
authoritative status. Nothing here is a claim of conformance with a specification beyond what
the tests check.

## Patents and licensing

The source is published under the Apache License, Version 2.0 (see [License](#license)). No patent search, licensing opinion or counsel review has been carried out for the formats in this crate. Several of them are covered by patents in some jurisdictions. H.264 and AAC are the main examples, and both are licensed through patent pools; royalties can apply. MP3 patents have largely expired, but the position varies by country. The MP4, MKV and HEIF writers carry H.264, so the same applies to their output. Container licensing has not been reviewed. AV1 is described by its licensors as royalty-free under published terms, which is not the same as a statement that no patent applies.

This repository grants no patent rights beyond those in the Apache License, Version 2.0, and that licence covers only patents held by contributors in respect of their own contributions. Anyone who distributes or uses an encoder or decoder for these formats is responsible for checking the patent and licence position that applies to them. The crate makes no claim that it is free of patent encumbrance.

## What works today

Status is taken from [ROADMAP.md](ROADMAP.md) and the source. "Encode" means the crate's own
encoder. Round-trip and external-decoder checks are listed in the Verification section.

| Format | Decode | Encode | Notes |
|---|:--:|:--:|---|
| PNG | RGBA8 | RGBA8 | 8-bit, non-interlaced only (`src/png.rs`) |
| JPEG | Baseline | Baseline | Sequential Huffman only. Progressive files return an error (`src/jpeg.rs`) |
| WebP | - | Lossless (VP8L) | Encoder output has no predictive transforms or colour cache, so files are large (`src/webp.rs`) |
| RIFF WAV | yes | yes | CLI path; 8/16/24/32-bit integer and 32-bit float (ROADMAP.md) |
| FLAC | yes | yes | Standard `.flac` output (ROADMAP.md) |
| SLAC | yes | yes | This crate's own lossless container (ROADMAP.md) |
| MP3 (Layer III) | yes | no | Decode to PCM; no encoder planned (ROADMAP.md) |
| AAC-LC (ADTS) | yes | yes | MDCT, Huffman, closed-form rate control (ROADMAP.md) |
| Opus | partial | no | Ogg demux, OpusHead and TOC parsing. No packet-to-PCM decode yet (ROADMAP.md) |
| H.264 | no | intra and P-skip | Annex-B output. Used by the MP4, MKV and HEIF muxers (`src/h264.rs`) |
| AV1 | no | keyframes | Experimental. Covers synthetic 64x64 keyframes (`src/av1_frame.rs`) |
| MP4, MKV, HEIF, Motion-JPEG AVI | no | muxers | Container writers only (`src/mp4.rs`, `src/mkv.rs`, `src/heif.rs`, `src/mjpeg.rs`) |

Other modules: `preview` renders a decoded RGBA image as a grayscale text preview
(`src/preview.rs`).

## Standards and licensing

Each format above follows a published specification, listed with its standards body in
[docs/STANDARDS.md](docs/STANDARDS.md). The table there gives the licensing and patent position as
understood for each one, with a link to check it.

- **Described as royalty-free by their publishers:** PNG (W3C), FLAC (Xiph), Opus (IETF RFC 6716),
  AV1 (AOMedia royalty-free patent licence), WebP (Google royalty-free patent licence).
- **Patent-pool or unclear:** H.264 and AAC (patent pools; royalties can apply), MP3 (core patents
  understood to have expired, but the position varies by country), and JPEG (baseline understood
  to be royalty-free; to be confirmed).
- **Containers:** the MP4, Matroska and HEIF writers carry H.264 here, so the H.264 position applies
  to their output. The Motion-JPEG AVI writer is not an ISO standard and was not reviewed.

An open standard is not automatically royalty-free. The publication status of each row has not
been checked against its primary source yet; see the "Verified" column in docs/STANDARDS.md.

This crate is a codec implementation. It is not certified against any conformance suite and is
not a licensed product from any codec vendor.

## Build and test

```sh
cargo build --release
cargo test --locked
```

Some integration tests use ffmpeg as an external decoder, so their output is checked against an
independent implementation. They print `SKIP` and pass when ffmpeg is not installed. CI installs
ffmpeg, so those checks always run there. The H.264 size report is `#[ignore]`d; run it with
`cargo test --release --test h264_bitexact -- --ignored --nocapture report_sizes`.

Minimum supported Rust version: **1.89** (`rust-version` in `Cargo.toml`, checked in CI).

## Library use

```rust
use transcode::{flac, png};

let image = png::decode(&bytes)?;           // image.width, image.height, image.pixels (RGBA8)
let (samples, rate, channels, _bps) = flac::decode(&bytes)?;
let png_bytes = png::encode(width, height, &rgba_pixels, true)?;
```

Decoders return `Result<_, String>`. The design goal is that they never panic on malformed
input; the fuzz targets in `fuzz/` exist to test that goal, and it is not yet proven. The public
modules are listed in `src/lib.rs`.

## Command line

```sh
transcode in.wav out.wav --rate 8000 --bits 16 --channels 1   # resample / depth / channels
transcode --selftest          # resampler, depth and channel-mixing checks
transcode --codec-selftest    # SLAC lossless round-trip
transcode --bench 10          # time each operation on a synthetic signal
```

`src/main.rs` also has flags for individual codec stages (`--from-flac`, `--to-aac`,
`--from-mp3`, `--png-to-mp4`, and others). Most are self-tests for one stage of one codec.

## Verification

- Unit tests in `src/` cover the entropy coders, transforms and container writers.
- Integration tests in `tests/` check AV1 and H.264 output against ffmpeg (`tests/av1_*.rs`,
  `tests/h264_bitexact.rs`).
- `fuzz/` contains one libFuzzer target per decoder that accepts untrusted bytes: PNG, JPEG,
  FLAC, SLAC, MP3, AAC and Opus. They run for 180 seconds per target on pull requests and daily in
  CI. No long-running campaign has been completed yet.
- Clippy correctness and suspicious lints are denied in CI. Style, complexity and perf lints are
  allowed for now; see the `[lints.clippy]` note in `Cargo.toml`.
- `cargo deny` and `cargo audit` run in CI against `Cargo.lock`.

Performance numbers are in [BENCHMARKS.md](BENCHMARKS.md). They come from one machine and mostly
from a synthetic tone. Read that file before quoting any number.

## Security

- The crate forbids `unsafe` code: `#![forbid(unsafe_code)]` is set on the library and the binary.
- The library has no runtime dependencies.
- Please report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Contributions that add a codec need test vectors,
an external-decoder check where one exists, and a note on patent status for review.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).

The licence covers this code only. It grants no rights under any patent that applies to a format
this crate decodes or encodes; see the patent note above.
