# Benchmarks

Reproduce with the built-in, dependency-free harness:

```bash
cargo build --release
./target/release/transcode --bench 10        # 10 s of audio (default)
./target/release/transcode --selftest         # correctness: resample + depth + remap
./target/release/transcode --codec-selftest   # SLAC lossless round-trip
```

`--bench` synthesizes the signal, then times each operation with
`std::time::Instant` — no external benchmarking crate, consistent with the crate's
zero-dependency rule.

## Results

Measured on a **13th Gen Intel Core i5-1340P**, `rustc 1.94.0`, `--release`
(`opt-level=3`), single thread. Workload: **10 s mono @ 16 kHz** = 160,000 samples
(312 KiB raw `i16`). Numbers are stable to ±10% across runs.

| Operation | Time | Throughput | Ratio |
|---|--:|--:|--:|
| resample 16k→8k | ~5.8 ms | ~1700× realtime · 27 Msamp/s | — |
| FLAC encode | ~7.6 ms | ~1300× realtime · 21 Msamp/s | 6.86× |
| FLAC decode | ~2.9 ms | ~3400× realtime · 55 Msamp/s | — |
| AAC-LC encode (128 kbps) | ~167 ms | ~60× realtime · 1.0 Msamp/s | 7.52× |
| AAC-LC decode | ~217 ms | ~46× realtime · 0.7 Msamp/s | — |
| SLAC encode (lossless) | ~3.9 ms | ~2580× realtime · 41 Msamp/s | 6.06× |
| SLAC decode | ~2.1 ms | ~4750× realtime · 77 Msamp/s | — |

"× realtime" = seconds of audio processed per second of wall-clock; higher is faster.

## How to read these

- **FLAC / SLAC / resample are very fast** (hundreds-to-thousands × realtime) — the
  fixed-point integer paths are the design's strong suit, fast enough that codec time
  is negligible next to disk/model time in the STT data pipeline.
- **AAC-LC is now ~60× realtime (encode) / ~46× (decode)** — up from ~4× before the
  rate control went **closed-form** (one B÷U scalefactor solve, no bisection search)
  and the MDCT/IMDCT moved to a **precomputed cosine basis** (no per-sample `cos()` in
  the hot loop, bit-identical to the naive transform). It is still slower than FLAC
  because the transform remains an O(N²) basis multiply; a split-radix FFT-based MDCT
  is the remaining step — see the [roadmap](ROADMAP.md). For the STT pipeline this does
  not matter (FLAC is the format in use); it matters only if AAC becomes a throughput
  path.

## Caveats (read before quoting these)

- **Synthetic signal.** The benchmark uses a pure 440 Hz tone. Compression ratios are
  therefore **best-case** — a single tone is maximally predictable. Real speech and
  music compress less (expect FLAC/SLAC closer to 1.5–2.5× on speech). **Throughput
  is representative** since it is largely content-independent for these codecs.
- **MP3 and Opus decode are not benchmarked** — there is no MP3/Opus *encoder* in this
  crate, so the harness cannot round-trip from synthesized audio. Benchmarking those
  decoders needs a real `.mp3` / `.opus` input file. (Opus decode-to-PCM is in any
  case still partial — see the roadmap.)
- **Single-run, single-thread, warm cache.** No averaging across many iterations, no
  multi-threading. These are order-of-magnitude figures for the dev box, not a
  competitive codec shootout.
