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
| AAC-LC encode (128 kbps) | ~15 ms | ~656× realtime · 10.5 Msamp/s | 7.52× |
| AAC-LC decode | ~27 ms | ~372× realtime · 6.0 Msamp/s | — |
| SLAC encode (lossless) | ~3.9 ms | ~2580× realtime · 41 Msamp/s | 6.06× |
| SLAC decode | ~2.1 ms | ~4750× realtime · 77 Msamp/s | — |

"× realtime" = seconds of audio processed per second of wall-clock; higher is faster.

## How to read these

- **FLAC / SLAC / resample are very fast** (hundreds-to-thousands × realtime) — the
  fixed-point integer paths are the design's strong suit, fast enough that codec time
  is negligible next to disk/model time in the STT data pipeline.
- **AAC-LC is now ~656× realtime encode / ~372× decode** — up from ~4× before the
  rate control went **closed-form** (one B÷U scalefactor solve, no bisection search)
  and the MDCT/IMDCT were **factored through one N-point FFT** (pre-twiddle → radix-2
  FFT → post-twiddle, so the transform is O(N log N) rather than the naive O(N²)
  cosine multiply). The factorization is exact, not lossy — it is the same cosine sum
  re-grouped via the chirp identity (n+n0)(k+½) = nk + n/2 + n0(k+½), verified to
  ~1e-11 by `--aac-fft-test`. TDAC reconstruction actually *improved* (long block
  −244 → −267 dB) because the FFT's log-depth butterfly accumulates less round-off
  than the dense dot-product. Remaining optimization → see the [roadmap](ROADMAP.md).

## vs ffmpeg (head-to-head)

`ffmpeg 7.1.4-0+deb13u1`, same box, **5 min mono @16 kHz tone**, full CLI wall-clock
(best of 3 runs, includes process startup + I/O):

| op | transcode | ffmpeg | result |
|---|--:|--:|---|
| FLAC encode (wav→flac) | **0.148s** | 0.234s | transcode 1.6× faster |
| FLAC decode (flac→wav) | **0.088s** | 0.201s | transcode 2.3× faster |
| AAC encode (wav→aac, 128k) | **0.325s** | 3.299s | transcode 10× faster |
| AAC decode (aac→wav) | 0.319s | **0.185s** | ffmpeg 1.7× faster |

Two caveats so this isn't misread. **(1)** The AAC *encode* gap is partly real
(closed-form rate control vs ffmpeg's iterative two-loop) but partly that this crate
does no psychoacoustic modeling, so on a pure tone — which has almost nothing to
quantize — the encoder undershoots toward the minimum scalefactor; ffmpeg fills toward
a real 128 kbps with noise shaping. The size column tells that story honestly:
`ff.aac` 1.67 MB vs `tc.aac` 0.29 MB — ffmpeg is doing more *work*, not just slower
work. On real music the gap will narrow. **(2)** AAC *decode* is the fair, like-for-like
comparison (no psy model either side), and 1.7× is the honest "how close" number. The
remaining gap is mostly ffmpeg's SIMD/asm plus its packed N/4 real-FFT (we use a
zero-padded full-N FFT, ~8× more FFT work than optimal — see roadmap item #1).

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
