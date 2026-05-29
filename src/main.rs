// ═══════════════════════════════════════════════════════════════════════════
// transcode — fixed-point audio transcoder (pure Rust, no deps)
//   • resample  : polyphase rational L/M (the verified fixed-point FIR)
//   • bit depth : 8 / 16 / 24 / 32-bit int, 32-bit float  (any → any)
//   • channels  : mono ↔ stereo (down-mix average / up-mix duplicate)
// ═══════════════════════════════════════════════════════════════════════════
//
// All samples are carried internally as canonical 32-bit signed full-scale
// fixed-point (`i32`, ±2³¹). Every depth maps into it by a shift, so resampling
// and mixing happen at full precision regardless of in/out depth. The resampler
// kernel is proof/PolyphaseCollapse.lean: y[m] = Σ h[k]·x[src] over the
// phase-aligned taps (the `step_by(L)` loop = polyphase_collapse, no zero-MACs),
// each product a Q15 multiply-accumulate (the ZmodMul/ZmodAdd discrete
// applicative of proof/IIRFilterCollapse.lean) summed in i64.
// ═══════════════════════════════════════════════════════════════════════════

use std::f64::consts::PI;

mod aac;
mod aac_tables;
mod codec;
mod flac;
mod mp3;
mod mp3_tables;
mod opus;
mod opus_tables;

const FS: f64 = 2_147_483_648.0; // 2^31, canonical full scale

#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    U8,
    I16,
    I24,
    I32,
    F32,
}

impl Fmt {
    fn parse(s: &str) -> Option<Fmt> {
        Some(match s {
            "8" => Fmt::U8,
            "16" => Fmt::I16,
            "24" => Fmt::I24,
            "32" => Fmt::I32,
            "32f" | "f32" | "float" => Fmt::F32,
            _ => return None,
        })
    }
    fn label(self) -> &'static str {
        match self {
            Fmt::U8 => "8-bit",
            Fmt::I16 => "16-bit",
            Fmt::I24 => "24-bit",
            Fmt::I32 => "32-bit",
            Fmt::F32 => "32-bit float",
        }
    }
    /// Stable byte code for the SLAC container header.
    fn code(self) -> u8 {
        match self {
            Fmt::U8 => 0,
            Fmt::I16 => 1,
            Fmt::I24 => 2,
            Fmt::I32 => 3,
            Fmt::F32 => 4,
        }
    }
    fn from_code(c: u8) -> Option<Fmt> {
        Some(match c {
            0 => Fmt::U8,
            1 => Fmt::I16,
            2 => Fmt::I24,
            3 => Fmt::I32,
            4 => Fmt::F32,
            _ => return None,
        })
    }
    /// Bits to drop from canonical full-scale i32 to reach the native sample
    /// (so the codec's residuals are sized to the real bit depth).
    fn shift(self) -> u32 {
        match self {
            Fmt::U8 => 24,
            Fmt::I16 => 16,
            Fmt::I24 => 8,
            Fmt::I32 | Fmt::F32 => 0,
        }
    }
}

struct Wav {
    sample_rate: u32,
    channels: u16,
    fmt: Fmt,
    samples: Vec<i32>, // interleaved, canonical full-scale
}

// ─── canonical-scale codec ───────────────────────────────────────────────────

/// native sample → canonical full-scale i32
fn to_canon(fmt: Fmt, b: &[u8], i: usize) -> i32 {
    match fmt {
        Fmt::U8 => ((b[i] as i32) - 128) << 24,
        Fmt::I16 => (i16::from_le_bytes([b[i], b[i + 1]]) as i32) << 16,
        Fmt::I24 => {
            let raw = (b[i] as i32) | ((b[i + 1] as i32) << 8) | ((b[i + 2] as i32) << 16);
            let signed = if raw & 0x80_0000 != 0 { raw | !0xFF_FFFF } else { raw }; // sign-extend 24→32
            signed << 8
        }
        Fmt::I32 => i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]),
        Fmt::F32 => {
            let f = f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as f64;
            (f.clamp(-1.0, 1.0) * FS).round().clamp(-FS, FS - 1.0) as i32
        }
    }
}

fn bytes_per(fmt: Fmt) -> usize {
    match fmt {
        Fmt::U8 => 1,
        Fmt::I16 => 2,
        Fmt::I24 => 3,
        Fmt::I32 | Fmt::F32 => 4,
    }
}

/// canonical full-scale i32 → native bytes appended to `out`
fn from_canon(fmt: Fmt, v: i32, out: &mut Vec<u8>) {
    match fmt {
        Fmt::U8 => out.push((((v >> 24) + 128).clamp(0, 255)) as u8),
        Fmt::I16 => out.extend_from_slice(&((v >> 16) as i16).to_le_bytes()),
        Fmt::I24 => {
            let s = v >> 8; // 24-bit signed
            out.extend_from_slice(&[s as u8, (s >> 8) as u8, (s >> 16) as u8]);
        }
        Fmt::I32 => out.extend_from_slice(&v.to_le_bytes()),
        Fmt::F32 => out.extend_from_slice(&((v as f64 / FS) as f32).to_le_bytes()),
    }
}

// ─── WAV I/O ──────────────────────────────────────────────────────────────────

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn read_wav(path: &str) -> Result<Wav, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut rate, mut channels, mut bits, mut tag) = (0u32, 0u16, 0u16, 0u16);
    let mut data: Option<(usize, usize)> = None;
    let mut o = 12;
    while o + 8 <= b.len() {
        let id = &b[o..o + 4];
        let sz = rd_u32(&b, o + 4) as usize;
        let body = o + 8;
        if body + sz > b.len() {
            break;
        }
        if id == b"fmt " {
            tag = rd_u16(&b, body);
            channels = rd_u16(&b, body + 2);
            rate = rd_u32(&b, body + 4);
            bits = rd_u16(&b, body + 14);
            if tag == 0xFFFE && sz >= 24 {
                tag = rd_u16(&b, body + 24); // extensible: real format in sub-GUID
            }
        } else if id == b"data" {
            data = Some((body, sz));
        }
        o = body + sz + (sz & 1);
    }
    let fmt = match (tag, bits) {
        (1, 8) => Fmt::U8,
        (1, 16) => Fmt::I16,
        (1, 24) => Fmt::I24,
        (1, 32) => Fmt::I32,
        (3, 32) => Fmt::F32,
        _ => return Err(format!("unsupported WAV format tag={tag} bits={bits}")),
    };
    let (body, sz) = data.ok_or("no data chunk")?;
    if rate == 0 || channels == 0 {
        return Err("missing/invalid fmt chunk".into());
    }
    let bp = bytes_per(fmt);
    let mut samples = Vec::with_capacity(sz / bp);
    let mut i = body;
    while i + bp <= body + sz {
        samples.push(to_canon(fmt, &b, i));
        i += bp;
    }
    Ok(Wav { sample_rate: rate, channels, fmt, samples })
}

fn write_wav(path: &str, w: &Wav) -> Result<(), String> {
    let bp = bytes_per(w.fmt) as u32;
    let block_align = w.channels as u32 * bp;
    let data_len = (w.samples.len() as u32) * bp;
    let (tag, bits) = match w.fmt {
        Fmt::U8 => (1u16, 8u16),
        Fmt::I16 => (1, 16),
        Fmt::I24 => (1, 24),
        Fmt::I32 => (1, 32),
        Fmt::F32 => (3, 32),
    };
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&w.channels.to_le_bytes());
    out.extend_from_slice(&w.sample_rate.to_le_bytes());
    out.extend_from_slice(&(w.sample_rate * block_align).to_le_bytes());
    out.extend_from_slice(&(block_align as u16).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in &w.samples {
        from_canon(w.fmt, s, &mut out);
    }
    std::fs::write(path, out).map_err(|e| format!("write {path}: {e}"))
}

// ─── filter design: windowed-sinc lowpass, Q15 ────────────────────────────────

const Q: i64 = 15;
const HALF_TAPS_PER_PHASE: usize = 16;

fn design_lowpass(l: usize, m: usize) -> Vec<i32> {
    let len = 2 * HALF_TAPS_PER_PHASE * l + 1;
    let center = (len - 1) as f64 / 2.0;
    let fc = 0.5 / (l.max(m) as f64);
    let mut taps = Vec::with_capacity(len);
    for n in 0..len {
        let t = n as f64 - center;
        let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * t).sin() / (PI * t) };
        let win = 0.5 - 0.5 * (2.0 * PI * n as f64 / (len - 1) as f64).cos();
        let h = sinc * win * (l as f64); // DC gain L → unity per polyphase
        taps.push((h * (1i64 << Q) as f64).round() as i32);
    }
    taps
}

// ─── polyphase rational resampler (fixed-point, canonical i32) ────────────────

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn resample_channel(x: &[i32], l: usize, m: usize, taps: &[i32]) -> Vec<i32> {
    let n = x.len();
    let out_len = (n * l) / m;
    let mut y = Vec::with_capacity(out_len);
    for out in 0..out_len {
        let p = out * m;
        let phase = p % l;
        let base = p / l;
        let mut acc: i64 = 0;
        let mut i = 0usize;
        loop {
            let k = phase + i * l; // phase-aligned taps only (polyphase_collapse)
            if k >= taps.len() {
                break;
            }
            let src = base as isize - i as isize;
            if src >= 0 && (src as usize) < n {
                acc += taps[k] as i64 * x[src as usize] as i64;
            }
            i += 1;
        }
        y.push((acc >> Q).clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    }
    y
}

fn deinterleave(s: &[i32], ch: usize) -> Vec<Vec<i32>> {
    let frames = s.len() / ch;
    let mut out = vec![Vec::with_capacity(frames); ch];
    for f in 0..frames {
        for c in 0..ch {
            out[c].push(s[f * ch + c]);
        }
    }
    out
}

fn interleave(chans: &[Vec<i32>]) -> Vec<i32> {
    let frames = chans[0].len();
    let mut out = Vec::with_capacity(frames * chans.len());
    for f in 0..frames {
        for c in chans {
            out.push(c[f]);
        }
    }
    out
}

/// Full transcode: resample → channel remap → (depth applied at write time).
fn transcode(w: &Wav, rate: u32, out_ch: u16, out_fmt: Fmt) -> Wav {
    let in_ch = w.channels as usize;
    let mut chans = deinterleave(&w.samples, in_ch);

    if rate != w.sample_rate {
        let g = gcd(rate, w.sample_rate);
        let (l, m) = ((rate / g) as usize, (w.sample_rate / g) as usize);
        let taps = design_lowpass(l, m);
        chans = chans.iter().map(|c| resample_channel(c, l, m, &taps)).collect();
    }

    // channel remap
    let want = out_ch as usize;
    let chans: Vec<Vec<i32>> = if want == chans.len() {
        chans
    } else if want == 1 {
        // down-mix to mono: average all channels
        let frames = chans[0].len();
        let mut mono = Vec::with_capacity(frames);
        for f in 0..frames {
            let sum: i64 = chans.iter().map(|c| c[f] as i64).sum();
            mono.push((sum / chans.len() as i64) as i32);
        }
        vec![mono]
    } else {
        // up-mix: replicate channel 0 across `want` channels
        vec![chans[0].clone(); want]
    };

    Wav { sample_rate: rate, channels: out_ch, fmt: out_fmt, samples: interleave(&chans) }
}

// ─── synth + measurement (self-test) ──────────────────────────────────────────

fn synth_sine(rate: u32, freq: f64, secs: f64, amp_frac: f64) -> Wav {
    let n = (rate as f64 * secs) as usize;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / rate as f64;
        samples.push((amp_frac * FS * (2.0 * PI * freq * t).sin()) as i32);
    }
    Wav { sample_rate: rate, channels: 1, fmt: Fmt::I16, samples }
}

fn goertzel(x: &[i32], rate: u32, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / rate as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s0 = (v as f64 / FS) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / x.len() as f64
}

fn selftest() -> i32 {
    let mut fails = 0;
    println!("resampling (1 kHz tone, dominant-bin check):");
    for &(fin, fout) in &[(24000u32, 16000u32), (8000, 16000), (44100, 16000)] {
        let src = synth_sine(fin, 1000.0, 0.5, 0.4);
        let out = transcode(&src, fout, 1, Fmt::I16);
        let g = gcd(fin, fout);
        let exp = (src.samples.len() * (fout / g) as usize) / (fin / g) as usize;
        let sig = goertzel(&out.samples, fout, 1000.0);
        let noise = goertzel(&out.samples, fout, 5000.0).max(1e-9);
        let ok = (out.samples.len() as isize - exp as isize).abs() <= 2 && sig > 10.0 * noise;
        if !ok { fails += 1; }
        println!("  {fin:>5}->{fout:<5} out={:<6}(exp {exp})  1k/5k ratio={:>7.0}  [{}]",
            out.samples.len(), sig / noise, if ok { "ok" } else { "FAIL" });
    }
    println!("bit-depth round-trips (write → read, expect near-identity):");
    let base = synth_sine(16000, 1000.0, 0.1, 0.4);
    for &fmt in &[Fmt::U8, Fmt::I16, Fmt::I24, Fmt::I32, Fmt::F32] {
        let path = format!("/tmp/_tc_rt_{}.wav", fmt.label().replace(' ', "_").replace('-', ""));
        let w = Wav { sample_rate: 16000, channels: 1, fmt, samples: base.samples.clone() };
        write_wav(&path, &w).unwrap();
        let back = read_wav(&path).unwrap();
        // tolerance = the depth's quantisation step in canonical units
        let tol: i64 = match fmt {
            Fmt::U8 => 1 << 24,
            Fmt::I16 => 1 << 16,
            Fmt::I24 => 1 << 8,
            Fmt::F32 => 1 << 8, // f32 has a 23-bit mantissa → ~2^8 step at full scale
            Fmt::I32 => 2,
        };
        let maxerr = base.samples.iter().zip(&back.samples)
            .map(|(a, b)| (*a as i64 - *b as i64).abs()).max().unwrap_or(0);
        let ok = back.fmt == fmt && back.samples.len() == base.samples.len() && maxerr <= tol;
        if !ok { fails += 1; }
        println!("  {:<13} maxerr={maxerr:>11} (tol {tol:>11})  [{}]", fmt.label(), if ok { "ok" } else { "FAIL" });
    }
    println!("channel remap:");
    let st = Wav { sample_rate: 16000, channels: 2, fmt: Fmt::I16,
        samples: vec![100 << 16, 300 << 16, 100 << 16, 300 << 16] }; // L=100 R=300
    let mono = transcode(&st, 16000, 1, Fmt::I16);
    let mono_ok = mono.channels == 1 && mono.samples == vec![200 << 16, 200 << 16]; // avg
    let up = transcode(&mono, 16000, 2, Fmt::I16);
    let up_ok = up.channels == 2 && up.samples == vec![200 << 16, 200 << 16, 200 << 16, 200 << 16];
    if !mono_ok { fails += 1; }
    if !up_ok { fails += 1; }
    println!("  stereo->mono avg [{}]   mono->stereo dup [{}]",
        if mono_ok { "ok" } else { "FAIL" }, if up_ok { "ok" } else { "FAIL" });

    if fails == 0 { println!("\nselftest: all passed"); 0 } else { println!("\nselftest: {fails} FAILED"); 1 }
}

// ─── benchmark: throughput + compression ratio (std-only, reproducible) ────────

fn bench(secs: f64) -> i32 {
    use std::time::Instant;
    let rate = 16000u32;
    let src = synth_sine(rate, 440.0, secs, 0.4); // canonical i32, fmt I16, mono
    let n = src.samples.len();
    let audio_s = n as f64 / rate as f64;
    let raw_bytes = n * bytes_per(Fmt::I16);
    println!("bench: {audio_s:.1}s mono @ {rate} Hz = {n} samples ({} KiB raw i16)", raw_bytes / 1024);
    println!("       synthetic 440 Hz tone — compression ratios are best-case (pure tone);");
    println!("       throughput is representative. real speech/music ratios will be lower.\n");

    let report = |label: &str, elapsed: f64, enc_bytes: Option<usize>| {
        let xrt = audio_s / elapsed;
        let msps = n as f64 / elapsed / 1e6;
        match enc_bytes {
            Some(b) => {
                let ratio = raw_bytes as f64 / b as f64;
                println!("  {label:<20} {:>8.2} ms  {xrt:>8.0}x realtime  {msps:>6.1} Msamp/s  ratio {ratio:>5.2}x ({} KiB)", elapsed * 1e3, b / 1024);
            }
            None => println!("  {label:<20} {:>8.2} ms  {xrt:>8.0}x realtime  {msps:>6.1} Msamp/s", elapsed * 1e3),
        }
    };

    let t = Instant::now();
    let _ = transcode(&src, 8000, 1, Fmt::I16);
    report("resample 16k->8k", t.elapsed().as_secs_f64(), None);

    let native_i32: Vec<i32> = src.samples.iter().map(|&v| v >> 16).collect();
    let t = Instant::now();
    let flac_bytes = flac::encode(&native_i32, rate, 1, 16);
    report("FLAC encode", t.elapsed().as_secs_f64(), Some(flac_bytes.len()));
    let t = Instant::now();
    match flac::decode(&flac_bytes) {
        Ok(_) => report("FLAC decode", t.elapsed().as_secs_f64(), None),
        Err(e) => println!("  FLAC decode        FAILED: {e}"),
    }

    let pcm_i16: Vec<i16> = src.samples.iter().map(|&v| (v >> 16) as i16).collect();
    let t = Instant::now();
    let aac_bytes = aac::encode_pcm(&pcm_i16, rate, 1, 128);
    report("AAC-LC encode 128k", t.elapsed().as_secs_f64(), Some(aac_bytes.len()));
    let t = Instant::now();
    match aac::decode_pcm(&aac_bytes) {
        Ok(_) => report("AAC-LC decode", t.elapsed().as_secs_f64(), None),
        Err(e) => println!("  AAC-LC decode      FAILED: {e}"),
    }

    let t = Instant::now();
    let slac_bytes = codec::encode(&src.samples, rate, 1, Fmt::I16.code(), Fmt::I16.shift());
    report("SLAC encode", t.elapsed().as_secs_f64(), Some(slac_bytes.len()));
    let t = Instant::now();
    match codec::decode(&slac_bytes) {
        Ok(_) => report("SLAC decode", t.elapsed().as_secs_f64(), None),
        Err(e) => println!("  SLAC decode        FAILED: {e}"),
    }

    println!("\n(MP3 + Opus decode need a real encoded file — this crate ships no MP3/Opus encoder.)");
    0
}

// ─── CLI ───────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--selftest" {
        std::process::exit(selftest());
    }
    if args.len() >= 2 && args[1] == "--codec-selftest" {
        std::process::exit(codec::selftest());
    }
    if args.len() >= 2 && args[1] == "--bench" {
        let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);
        std::process::exit(bench(secs));
    }
    if args.len() == 4 && args[1] == "--from-mp3" {
        match mp3::decode_pcm(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok((s, rate, ch)) => {
                let samples: Vec<i32> = s.iter().map(|&v| (v as i32) << 16).collect();
                let w = Wav { sample_rate: rate, channels: ch, fmt: Fmt::I16, samples };
                write_wav(&args[3], &w).ok();
                println!("decoded {} samples @ {} Hz, {} ch → {}", s.len(), rate, ch, args[3]);
            }
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 2 && args[1] == "--aac-tdac-test" {
        std::process::exit(aac::tdac_test());
    }
    if args.len() == 2 && args[1] == "--aac-mdct-probe" {
        std::process::exit(aac::mdct_probe());
    }
    if args.len() == 2 && args[1] == "--aac-fft-test" {
        std::process::exit(aac::fft_check());
    }
    if args.len() == 2 && args[1] == "--aac-huff-test" {
        std::process::exit(aac::huffman_selftest());
    }
    if args.len() == 2 && args[1] == "--aac-synth-test" {
        std::process::exit(aac::synth_pr_test());
    }
    if args.len() == 2 && args[1] == "--aac-enc-test" {
        std::process::exit(aac::encode_roundtrip_test());
    }
    if args.len() == 2 && args[1] == "--opus-range-test" {
        std::process::exit(opus::range_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-laplace-test" {
        std::process::exit(opus::laplace_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-energy-test" {
        std::process::exit(opus::coarse_energy_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-pvq-test" {
        std::process::exit(opus::pvq_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-fine-test" {
        std::process::exit(opus::fine_energy_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-alloc-test" {
        std::process::exit(opus::alloc_cache_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-rotation-test" {
        std::process::exit(opus::exp_rotation_selftest());
    }
    if args.len() == 2 && args[1] == "--opus-theta-test" {
        std::process::exit(opus::theta_selftest());
    }
    if args.len() == 3 && args[1] == "--opus-celt-alloc" {
        match opus::celt_alloc_demo(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--opus-toc" {
        match opus::toc_stats(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--opus-info" {
        let bytes = std::fs::read(&args[2]).unwrap_or_default();
        match opus::parse(&bytes) {
            Ok(i) => {
                let dur = i.samples as f64 / 48000.0;
                println!(
                    "{}: Opus, {} ch, input_rate {} Hz, pre_skip {}, {} audio packets, {} samples@48k, {:.3}s",
                    args[2], i.channels, i.input_rate, i.pre_skip, i.audio_packets, i.samples, dur
                );
            }
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if (args.len() == 4 || args.len() == 5) && args[1] == "--to-aac" {
        // WAV → ADTS AAC-LC. Optional 5th arg = bitrate kbps (default 128).
        let w = match read_wav(&args[2]) {
            Ok(w) => w,
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        };
        let kbps: u32 = if args.len() == 5 { args[4].parse().unwrap_or(128) } else { 128 };
        let pcm: Vec<i16> = w.samples.iter().map(|&v| (v >> 16) as i16).collect();
        let bytes = aac::encode_pcm(&pcm, w.sample_rate, w.channels, kbps);
        let raw = w.samples.len() * 2;
        let ratio = if bytes.is_empty() { 0.0 } else { raw as f64 / bytes.len() as f64 };
        match std::fs::write(&args[3], &bytes) {
            Ok(()) => println!("AAC {} → {} : {} Hz, {} ch, {} kbps, {} B → {} B ({ratio:.2}x)",
                args[2], args[3], w.sample_rate, w.channels, kbps, raw, bytes.len()),
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--from-aac" {
        match aac::decode_pcm(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok((s, rate, ch)) => {
                let samples: Vec<i32> = s.iter().map(|&v| (v as i32) << 16).collect();
                let w = Wav { sample_rate: rate, channels: ch, fmt: Fmt::I16, samples };
                write_wav(&args[3], &w).ok();
                println!("decoded {} samples @ {} Hz, {} ch → {}", s.len(), rate, ch, args[3]);
            }
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--aac-stats" {
        match aac::feature_stats(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--aac-debug" {
        match aac::debug_frame(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--aac-align" {
        match aac::align_check(&std::fs::read(&args[2]).unwrap_or_default()) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }
    if args.len() == 3 && args[1] == "--aac-info" {
        let bytes = match std::fs::read(&args[2]) {
            Ok(b) => b,
            Err(e) => { eprintln!("read {}: {e}", args[2]); std::process::exit(1); }
        };
        match aac::parse(&bytes) {
            Ok(i) => {
                let dur = i.samples as f64 / i.samplerate as f64;
                println!("{}: AAC profile {} (1=LC), {} Hz, {} ch, {} frames, {} samples/ch, {:.3}s",
                    args[2], i.profile, i.samplerate, i.channels, i.frames, i.samples, dur);
            }
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 3 && args[1] == "--mp3-requant" {
        match mp3::requant_check(&std::fs::read(&args[2]).unwrap_or_default()) { Ok(s)=>println!("{s}"), Err(e)=>eprintln!("error: {e}") }
        return;
    }
    if args.len() == 3 && args[1] == "--mp3-spectral" {
        match mp3::spectral_check(&std::fs::read(&args[2]).unwrap_or_default()) { Ok(s)=>println!("{s}"), Err(e)=>eprintln!("error: {e}") }
        return;
    }
    if args.len() == 2 && args[1] == "--mp3-fb-test" {
        std::process::exit(mp3::filterbank_test());
    }
    if args.len() == 2 && args[1] == "--mp3-imdct-test" {
        std::process::exit(mp3::imdct_tdac_test());
    }
    if args.len() == 2 && args[1] == "--mp3-huff-test" {
        std::process::exit(mp3::huffman_selftest());
    }
    if args.len() == 3 && args[1] == "--mp3-info" {
        let bytes = match std::fs::read(&args[2]) {
            Ok(b) => b,
            Err(e) => { eprintln!("read {}: {e}", args[2]); std::process::exit(1); }
        };
        match mp3::parse(&bytes) {
            Ok(i) => {
                let dur = i.samples as f64 / i.samplerate as f64;
                println!("{}: MPEG{} Layer III, {} Hz, {} ch, {} frames, {} samples, {:.2}s, {}{} kbps",
                    args[2],
                    match i.version { mp3::Version::Mpeg1 => "1", mp3::Version::Mpeg2 => "2", mp3::Version::Mpeg25 => "2.5" },
                    i.samplerate, i.channels, i.frames, i.samples, dur,
                    if i.vbr { "VBR ~" } else { "" }, i.bitrate_kbps_first);
                match mp3::sideinfo_stats(&bytes) {
                    Ok(s) => println!("  {s}"),
                    Err(e) => eprintln!("  side-info: {e}"),
                }
                match mp3::reservoir_check(&bytes) {
                    Ok(s) => println!("  {s}"),
                    Err(e) => eprintln!("  reservoir: {e}"),
                }
            }
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--from-flac" {
        // real .flac → WAV
        let bytes = match std::fs::read(&args[2]) {
            Ok(b) => b,
            Err(e) => { eprintln!("read {}: {e}", args[2]); std::process::exit(1); }
        };
        match flac::decode(&bytes) {
            Ok((native, rate, channels, bps)) => {
                let shift = 32 - bps;
                let samples: Vec<i32> = native.iter().map(|&v| v << shift).collect();
                let fmt = match bps { 8 => Fmt::U8, 24 => Fmt::I24, 32 => Fmt::I32, _ => Fmt::I16 };
                let w = Wav { sample_rate: rate, channels, fmt, samples };
                match write_wav(&args[3], &w) {
                    Ok(()) => println!("FLAC {} → {} : {} Hz, {} ch, {}-bit", args[2], args[3], rate, channels, bps),
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            }
            Err(e) => { eprintln!("decode error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--to-flac" {
        // WAV → real .flac (decodable by any FLAC decoder)
        let w = match read_wav(&args[2]) {
            Ok(w) => w,
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        };
        let sh = w.fmt.shift();
        let native: Vec<i32> = w.samples.iter().map(|&v| v >> sh).collect();
        let bps = 32 - sh;
        let bytes = flac::encode(&native, w.sample_rate, w.channels, bps);
        let raw = w.samples.len() * bytes_per(w.fmt);
        let ratio = if bytes.is_empty() { 0.0 } else { raw as f64 / bytes.len() as f64 };
        match std::fs::write(&args[3], &bytes) {
            Ok(()) => println!("FLAC {} → {} : {} Hz, {} ch, {}-bit, {} B → {} B ({ratio:.2}x)",
                args[2], args[3], w.sample_rate, w.channels, bps, raw, bytes.len()),
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--encode" {
        // WAV → SLAC (lossless)
        let w = match read_wav(&args[2]) {
            Ok(w) => w,
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        };
        let bytes = codec::encode(&w.samples, w.sample_rate, w.channels, w.fmt.code(), w.fmt.shift());
        let raw = w.samples.len() * bytes_per(w.fmt);
        let ratio = if bytes.is_empty() { 0.0 } else { raw as f64 / bytes.len() as f64 };
        match std::fs::write(&args[3], &bytes) {
            Ok(()) => println!("encoded {} → {} : {} ch, {}, {} B → {} B ({ratio:.2}x)",
                args[2], args[3], w.channels, w.fmt.label(), raw, bytes.len()),
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 4 && args[1] == "--decode" {
        // SLAC → WAV
        let bytes = match std::fs::read(&args[2]) {
            Ok(b) => b,
            Err(e) => { eprintln!("read {}: {e}", args[2]); std::process::exit(1); }
        };
        match codec::decode(&bytes) {
            Ok((samples, rate, channels, fmt_code)) => {
                let fmt = Fmt::from_code(fmt_code).unwrap_or(Fmt::I16);
                let w = Wav { sample_rate: rate, channels, fmt, samples };
                match write_wav(&args[3], &w) {
                    Ok(()) => println!("decoded {} → {} : {} Hz, {} ch, {}", args[2], args[3], rate, channels, fmt.label()),
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            }
            Err(e) => { eprintln!("decode error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() == 6 && args[1] == "--gen" {
        let rate: u32 = args[3].parse().unwrap_or(0);
        let freq: f64 = args[4].parse().unwrap_or(0.0);
        let secs: f64 = args[5].parse().unwrap_or(0.0);
        let w = synth_sine(rate, freq, secs, 0.4);
        match write_wav(&args[2], &w) {
            Ok(()) => println!("gen {} : {rate} Hz, {freq} Hz tone, {secs}s", args[2]),
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
        }
        return;
    }
    if args.len() < 3 {
        eprintln!("usage: transcode <in.wav> <out.wav> [--rate HZ] [--bits 8|16|24|32|32f] [--channels 1|2]");
        eprintln!("       transcode --gen <out.wav> <rate> <freq> <secs>");
        eprintln!("       transcode --selftest");
        std::process::exit(2);
    }
    let (inp, outp) = (args[1].clone(), args[2].clone());
    let w = match read_wav(&inp) {
        Ok(w) => w,
        Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
    };
    let (mut rate, mut bits, mut chans) = (w.sample_rate, w.fmt, w.channels);
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--rate" if i + 1 < args.len() => { rate = args[i + 1].parse().unwrap_or(rate); i += 2; }
            "--bits" if i + 1 < args.len() => {
                match Fmt::parse(&args[i + 1]) { Some(f) => bits = f, None => { eprintln!("bad --bits {}", args[i + 1]); std::process::exit(2); } }
                i += 2;
            }
            "--channels" if i + 1 < args.len() => { chans = args[i + 1].parse().unwrap_or(chans); i += 2; }
            other => { eprintln!("unknown arg: {other}"); std::process::exit(2); }
        }
    }
    println!("in : {} Hz, {} ch, {}", w.sample_rate, w.channels, w.fmt.label());
    let out = transcode(&w, rate, chans, bits);
    println!("out: {} Hz, {} ch, {}", out.sample_rate, out.channels, out.fmt.label());
    match write_wav(&outp, &out) {
        Ok(()) => println!("wrote {outp}"),
        Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
    }
}
