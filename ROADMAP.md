# Roadmap & status

`transcode` is a from-scratch, zero-dependency audio codec crate. The table below is
the **honest** state of each path — what round-trips today vs. what is partial.

## Status by format

| Format | Decode → PCM | Encode | Notes |
|---|:--:|:--:|---|
| **RIFF WAV** | ✅ | ✅ | 8/16/24/32-bit int + 32-bit float; the canonical I/O |
| **FLAC** | ✅ | ✅ | real `.flac`, decodable by any FLAC decoder; the STT data path |
| **SLAC** | ✅ | ✅ | own lossless container; `decode∘encode = identity` (verified) |
| **MP3 (Layer III)** | ✅ | ❌ | full decoder; no encoder planned (decode-only is the need) |
| **AAC-LC (ADTS)** | ✅ | ✅ | MDCT + Huffman + **closed-form** rate control (no search loop); basis-cached transform |
| **Opus** | 🔨 partial | ❌ | range coder, Laplace/PVQ, coarse/fine energy, CELT alloc, TOC + container parse — **no full frame→PCM decode yet** |

✅ done · 🔨 in progress · ❌ not implemented

## What each piece means

- **Resampler** — polyphase rational L/M with a fixed-point Q15 windowed-sinc lowpass.
  Only phase-aligned taps are multiplied (`step_by(L)`, no wasted zero-MACs). All math
  is integer (`i64` accumulate), so any-rate→any-rate is bit-exact reproducible.
- **Canonical sample model** — every depth is carried internally as full-scale signed
  `i32` (±2³¹). Resampling and mixing always happen at full precision; depth is applied
  only at write time. This is why "any depth → any depth" needs no special cases.
- **FLAC** — Free Lossless Audio Codec: linear-predictor + Rice-coded residual. Both
  directions implemented; output is standard `.flac`.
- **SLAC** — this crate's own lossless container (predictor + residual coding), used
  when there is no need for FLAC interop and we want the simplest sovereign round-trip.
- **MP3 Layer III decode** — Huffman → requantize → reorder → IMDCT → polyphase
  synthesis filterbank; handles MPEG-1/2/2.5, the bit reservoir, and VBR.
- **AAC-LC decode/encode** — ADTS framing, Huffman codebooks, inverse/forward MDCT with
  TDAC overlap-add, scale-factor requantization. The encoder picks the global
  scalefactor in **closed form** (residual ÷ analytic rate–scalefactor slope, one B/U
  injection) rather than the classic iterative two-loop search. The MDCT/IMDCT share a
  precomputed cosine basis (no per-sample `cos()`), keeping both directions fast.
- **Opus** — the entropy layer (range coder + Laplace + PVQ pulse coding) and the CELT
  bit-allocation / energy-envelope machinery are built and unit-probed; the SILK and
  full CELT synthesis stages that turn a packet into PCM are not wired yet.

## Near-term

1. **AAC: O(N log N) MDCT** — the closed-form rate control (no scalefactor search
   loop) and the precomputed cosine basis (no per-sample `cos()`) already took AAC
   from ~4× to **~60× realtime encode / ~46× decode** ([benchmarks](BENCHMARKS.md)).
   The transform is still an O(N²) basis multiply; a split-radix FFT-based MDCT would
   close the remaining gap to FLAC. Higher-risk than the table approach, so deferred.
2. **Finish Opus decode → PCM** — wire CELT synthesis (and SILK) on top of the existing
   range coder + PVQ + energy primitives so `--from-opus` / `opus::decode_pcm` work.
3. **`cargo test` coverage** — the codecs are currently verified only through the CLI
   self-test flags (`--selftest`, `--codec-selftest`, `--aac-*`, `--opus-*`, `--mp3-*`).
   Wrap these as `#[test]`s so `cargo test` gates regressions in CI.
4. **Real-corpus benchmark** — current `--bench` numbers are on a synthetic tone. Add a
   path that decodes a real LibriSpeech `.flac` / a real `.mp3` to report honest
   throughput and (for lossy) round-trip fidelity on actual audio.

## Longer-term

- Streaming / incremental decode (decode-as-you-read) instead of whole-buffer, for
  bounded memory on long files.
- SIMD or multi-threaded resampler/MDCT once the scalar paths are profiled.
- Vorbis decode (the other common container in open speech corpora).

## Non-goals

- ffmpeg / symphonia / any C library dependency — the whole point is sovereignty.
- NIST SPHERE — read by the STT harness directly (raw PCM behind an ASCII header), not
  this crate.
- A general media muxer (no MP4/MKV container handling); this is a codec layer.
