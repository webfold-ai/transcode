# transcode

A sovereign, ffmpeg-free audio codec crate — pure Rust, **zero external dependencies**
(std only). Decodes and encodes real-world compressed audio and transcodes PCM
(resample / bit-depth / channel remap) without linking ffmpeg, symphonia, or any C
library.

Carved out of the sanguine repo so it can be reused as a standalone crate by the
audio front-ends in the peregrine STT stack (it is how LibriSpeech `.flac` is read
into PCM for log-mel feature extraction).

## What it does

| Capability | Detail |
|---|---|
| **Decode** | FLAC, MP3 (Layer III), AAC-LC (ADTS), RIFF WAV — Opus is partial (see below) |
| **Encode** | FLAC, AAC-LC, RIFF WAV, SLAC (lossless container) |
| **Resample** | polyphase rational L/M, fixed-point Q15 windowed-sinc lowpass |
| **Bit depth** | 8 / 16 / 24 / 32-bit int, 32-bit float — any → any |
| **Channels** | mono ↔ stereo (down-mix average, up-mix duplicate) |

**Per-format status, what each piece means, and the roadmap → [ROADMAP.md](ROADMAP.md).**
Short version: WAV, FLAC, SLAC and the resampler are fully round-tripping; MP3 decodes
(no encoder); AAC-LC round-trips but is slow; **Opus is entropy-layer + container
parsing only — no full frame→PCM decode yet.**

All samples are carried internally as canonical 32-bit signed full-scale fixed-point
(`i32`, ±2³¹); every depth maps in by a shift, so resampling and mixing happen at full
precision regardless of in/out depth. The resampler is the phase-aligned polyphase
collapse (`step_by(L)`, no zero-MACs), each tap a Q15 multiply-accumulate summed in
`i64`.

Does **not** read NIST SPHERE (`.WAV` from TIMIT etc.) — that container is read by the
STT harness itself, since it is raw PCM behind an ASCII header.

## Library API

Add as a path dependency:

```toml
[dependencies]
transcode = { path = "../../transcode" }
```

Decoders return `(samples, rate, channels, …)`; the FLAC decoder also returns bits per
sample:

```rust
// FLAC → 32-bit canonical samples (the STT audio front-end path)
let (samples, rate, channels, bps) = transcode::flac::decode(&bytes)?; // (Vec<i32>, u32, u16, u32)

// MP3 → interleaved i16 PCM
let (pcm, rate, channels) = transcode::mp3::decode_pcm(&bytes)?;        // (Vec<i16>, u32, u16)
let (pcm, rate)           = transcode::mp3::decode_pcm_mono(&bytes)?;   // (Vec<i16>, u32)

// AAC-LC (ADTS) → interleaved i16 PCM
let (pcm, rate, channels) = transcode::aac::decode_pcm(&bytes)?;        // (Vec<i16>, u32, u16)

// Encoders
let flac = transcode::flac::encode(&samples, rate, channels, bps);     // Vec<u8>
let aac  = transcode::aac::encode_pcm(&pcm, rate, channels, kbps);     // Vec<u8>

// SLAC lossless container
let slac = transcode::codec::encode(&samples, rate, channels, fmt_code, shift);
let (samples, rate, channels, fmt_code) = transcode::codec::decode(&slac)?;

// Container probes (no full decode)
let info = transcode::opus::parse(&bytes)?;
let info = transcode::aac::parse(&bytes)?;
let info = transcode::mp3::parse(&bytes)?;
```

Modules: `flac`, `mp3` (+ `mp3_tables`), `aac` (+ `aac_tables`), `opus` (+ `opus_tables`),
`codec` (SLAC).

## CLI

```bash
cargo build --release            # builds the `transcode` binary
```

Transcode a WAV (resample / re-depth / remap):

```bash
transcode in.wav out.wav --rate 16000 --bits 16 --channels 1
```

Container conversions:

```bash
transcode --from-flac in.flac out.wav     transcode --to-flac in.wav out.flac
transcode --from-mp3  in.mp3  out.wav
transcode --from-aac  in.aac  out.wav     transcode --to-aac  in.wav out.aac [kbps]
transcode --encode    in.wav  out.slac    transcode --decode  in.slac out.wav
```

Probe a stream without decoding:

```bash
transcode --mp3-info in.mp3      transcode --aac-info in.aac      transcode --opus-info in.opus
```

Generate a test tone, and run the self-tests:

```bash
transcode --gen tone.wav 16000 1000 0.5   # 16 kHz, 1 kHz tone, 0.5 s
transcode --selftest                       # resample + bit-depth round-trip + channel remap
transcode --codec-selftest                 # SLAC encode/decode round-trip
transcode --bench 10                        # throughput + ratio benchmark on 10 s of audio
```

(Per-codec verification probes also exist: `--mp3-*`, `--aac-*`, `--opus-*`.)

## Performance

On a 13th Gen i5-1340P, single thread: FLAC, SLAC and the resampler run at
**~1000–4000× realtime**; AAC-LC runs at **~656× realtime encode / ~372× decode** with
closed-form rate control and an FFT-factored O(N log N) MDCT/IMDCT. In a wall-clock
comparison against ffmpeg 7.1.4 on the same box, transcode is faster on FLAC (both
directions) and on AAC encode; ffmpeg is ~1.7× faster on AAC decode. Full measured
table, methodology, ffmpeg head-to-head, and caveats → [BENCHMARKS.md](BENCHMARKS.md).
Reproduce with `cargo build --release && ./target/release/transcode --bench 10`.

## Design notes

The fixed-point kernels mirror the sanguine Lean proofs they were derived from:
`proof/PolyphaseCollapse.lean` (the resampler's phase-aligned tap sum) and
`proof/IIRFilterCollapse.lean` (the Q15 multiply-accumulate as discrete-applicative
`ZmodMul`/`ZmodAdd`). Those proofs live in the sanguine repo; this crate is the
standalone runtime.

## License / provenance

Sovereign from-scratch implementation — no third-party codec code. Internal TWN R&D.
