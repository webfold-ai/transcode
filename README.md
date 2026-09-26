# transcode

Sovereign, ffmpeg-free audio + image codec crate — pure Rust, zero
non-Ptyktos dependencies.

## What it is

transcode is the Ptyktos fold-extracted codec layer. PNG, JPEG, AAC, AV1,
H.264, FLAC, MP3, MP4, HEIF, WEBP, NIfTI, HDF5 — all decoded and encoded
without leaving the ptyktos substrate (no ffmpeg, no libpng, no libjpeg).

Lives on the ptyktos substrate because Ptyktos's image + audio pipelines
(`memoryfold`, the audio-folders, the `glossa` localization engine, the
peregrine-ng extraction service) all want to handle media without
shipping ffmpeg.

## What lives here

| Surface | Path |
|---|---|
| Crate | `src/lib.rs` |
| PNG decoder/encoder | `src/codec/png/` |
| JPEG decoder/encoder | `src/codec/jpeg/` |
| AAC decoder/encoder | `src/codec/aac/` |
| AV1 decoder/encoder | `src/codec/av1/` |
| H.264 decoder/encoder | `src/codec/h264/` |
| MP4 muxer | `src/mp4/` |

## Build

```sh
cargo build --release
```

## See also

- [`Ptyktos/sangsec`](https://github.com/Ptyktos/sangsec) — uses transcode
  for embedded-image inspection in agent tool calls
- [`Ptyktos/glossa`](https://github.com/Ptyktos/glossa) — uses transcode
  for localization-asset hashing

## License

SPDX: `MIT OR Apache-2.0`. Ptyktos substrate (fold-extracted).
