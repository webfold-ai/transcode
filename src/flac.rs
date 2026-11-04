// ═══════════════════════════════════════════════════════════════════════════
// flac.rs — spec-compliant FLAC encoder (writes real .flac that ffmpeg decodes)
// ═══════════════════════════════════════════════════════════════════════════
// Fixed polynomial predictors (orders 0–4) + Rice-coded residuals, in the real
// FLAC bitstream: STREAMINFO, frame headers (sync, UTF-8 frame number, CRC-8),
// FIXED subframes, frame CRC-16. Native samples (canonical >> shift).
// ═══════════════════════════════════════════════════════════════════════════

const BLOCK: usize = 4096;

fn crc8(d: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in d {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}
fn crc16(d: &[u8]) -> u16 {
    let mut c = 0u16;
    for &b in d {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
        }
    }
    c
}

/// MSB-first bit writer over a byte buffer (so CRCs can be taken over `buf`).
struct W {
    buf: Vec<u8>,
    acc: u32,
    n: u32,
}
impl W {
    fn new() -> Self {
        W {
            buf: Vec::new(),
            acc: 0,
            n: 0,
        }
    }
    fn bits(&mut self, v: u64, nb: u32) {
        for i in (0..nb).rev() {
            self.acc = (self.acc << 1) | (((v >> i) & 1) as u32);
            self.n += 1;
            if self.n == 8 {
                self.buf.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn align(&mut self) {
        if self.n > 0 {
            self.bits(0, 8 - self.n);
        }
    }
}

/// FLAC UTF-8-style coding of an unsigned value (frame number), up to 36 bits.
fn write_utf8(w: &mut W, val: u64) {
    if val < 0x80 {
        w.bits(val, 8);
        return;
    }
    let nb = if val < 0x800 {
        2
    } else if val < 0x10000 {
        3
    } else if val < 0x200000 {
        4
    } else if val < 0x4000000 {
        5
    } else if val < 0x80000000 {
        6
    } else {
        7u32
    };
    let lead = ((0xFFu32 << (8 - nb)) & 0xFF) as u64 | (val >> (6 * (nb - 1)));
    w.bits(lead, 8);
    for i in (0..nb - 1).rev() {
        w.bits(0x80 | ((val >> (6 * i)) & 0x3F), 8);
    }
}

/// Rice-encode a signed residual: zigzag fold, quotient as q zeros + a 1, then
/// the low k bits (FLAC's rice; note the unary is zeros-then-one). -/
fn rice(w: &mut W, val: i64, k: u32) {
    let u = ((val << 1) ^ (val >> 63)) as u64;
    let q = (u >> k) as u32;
    for _ in 0..q {
        w.bits(0, 1);
    }
    w.bits(1, 1);
    if k > 0 {
        w.bits(u & ((1 << k) - 1), k);
    }
}

fn choose_k(res: &[i64]) -> u32 {
    if res.is_empty() {
        return 0;
    }
    let mean = res
        .iter()
        .map(|&r| ((r << 1) ^ (r >> 63)) as u128)
        .sum::<u128>()
        / res.len() as u128;
    let k = if mean == 0 {
        0
    } else {
        63 - (mean as u64).leading_zeros()
    };
    k.min(30)
}

/// Fixed-predictor residual for the given order (0..=4). -/
fn residual_fixed(x: &[i64], order: usize) -> Vec<i64> {
    (order..x.len())
        .map(|n| match order {
            0 => x[n],
            1 => x[n] - x[n - 1],
            2 => x[n] - 2 * x[n - 1] + x[n - 2],
            3 => x[n] - 3 * x[n - 1] + 3 * x[n - 2] - x[n - 3],
            _ => x[n] - 4 * x[n - 1] + 6 * x[n - 2] - 4 * x[n - 3] + x[n - 4],
        })
        .collect()
}

const LPC_ORDER: usize = 12;
const LPC_PREC: u32 = 15;

/// Write a residual block (5-bit Rice params, single partition). -/
fn write_residual(w: &mut W, res: &[i64]) {
    w.bits(1, 2); // coding method 1 (5-bit params)
    w.bits(0, 4); // partition order 0
    let k = choose_k(res);
    w.bits(k as u64, 5);
    for &r in res {
        rice(w, r, k);
    }
}

fn autocorr(x: &[i64], lag: usize) -> Vec<f64> {
    let n = x.len();
    let half = (n as f64 - 1.0) / 2.0;
    let w: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let t = if half > 0.0 {
                (i as f64 - half) / half
            } else {
                0.0
            };
            v as f64 * (1.0 - t * t)
        })
        .collect();
    (0..=lag)
        .map(|k| (k..n).map(|i| w[i] * w[i - k]).sum())
        .collect()
}

fn levinson(r: &[f64], order: usize) -> Vec<f64> {
    let mut a = vec![0.0f64; order];
    let mut e = r[0];
    if e <= 0.0 {
        return a;
    }
    for i in 0..order {
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / e;
        let mut na = a.clone();
        na[i] = k;
        for j in 0..i {
            na[j] = a[j] - k * a[i - 1 - j];
        }
        a = na;
        e *= 1.0 - k * k;
        if e <= 0.0 {
            break;
        }
    }
    a
}

fn quantize_flac(lpc: &[f64], prec: u32) -> (Vec<i32>, i32) {
    let maxc = lpc.iter().fold(0.0f64, |m, &c| m.max(c.abs()));
    if maxc <= 0.0 {
        return (vec![0; lpc.len()], 0);
    }
    let mut shift = (prec as i32 - 1) - (maxc.log2().floor() as i32) - 1;
    shift = shift.clamp(0, 15);
    let lim = (1i64 << (prec - 1)) - 1;
    let mut q = Vec::with_capacity(lpc.len());
    let mut err = 0.0f64;
    for &c in lpc {
        let v = c * (1i64 << shift) as f64 + err;
        let qi = (v.round() as i64).clamp(-lim - 1, lim);
        err = v - qi as f64;
        q.push(qi as i32);
    }
    (q, shift)
}

/// LPC subframe: Levinson coefficients (the δS=0 predictor), real FLAC layout. -/
fn write_lpc(w: &mut W, x: &[i64], bps: u32, order: usize) {
    let (q, shift) = quantize_flac(&levinson(&autocorr(x, order), order), LPC_PREC);
    w.bits(0, 1);
    w.bits(0b100000 | (order - 1) as u64, 6); // LPC subframe, order
    w.bits(0, 1); // no wasted bits
    let mask = if bps >= 64 {
        u64::MAX
    } else {
        (1u64 << bps) - 1
    };
    for &s in &x[..order] {
        w.bits((s as u64) & mask, bps);
    }
    w.bits((LPC_PREC - 1) as u64, 4);
    w.bits(shift as u64, 5);
    let cmask = (1u64 << LPC_PREC) - 1;
    for &qc in &q {
        w.bits((qc as u64) & cmask, LPC_PREC);
    }
    let res: Vec<i64> = (order..x.len())
        .map(|n| {
            let mut p = 0i64;
            for j in 0..order {
                p += q[j] as i64 * x[n - 1 - j];
            }
            x[n] - (p >> shift)
        })
        .collect();
    write_residual(w, &res);
}

/// FIXED subframe (orders 0–4) — fallback for tiny blocks. -/
fn write_fixed(w: &mut W, x: &[i64], bps: u32) {
    let mut best = 0usize;
    let mut best_sum = u128::MAX;
    for o in 0..=4.min(x.len().saturating_sub(1)) {
        let s: u128 = residual_fixed(x, o)
            .iter()
            .map(|&r| r.unsigned_abs() as u128)
            .sum();
        if s < best_sum {
            best_sum = s;
            best = o;
        }
    }
    let order = best;
    w.bits(0, 1);
    w.bits(0b001000 | order as u64, 6);
    w.bits(0, 1);
    let mask = if bps >= 64 {
        u64::MAX
    } else {
        (1u64 << bps) - 1
    };
    for &s in &x[..order] {
        w.bits((s as u64) & mask, bps);
    }
    write_residual(w, &residual_fixed(x, order));
}

/// Pick subframe type: CONSTANT for flat blocks, LPC for normal, FIXED for tiny. -/
fn write_subframe(w: &mut W, x: &[i64], bps: u32) {
    let mask = if bps >= 64 {
        u64::MAX
    } else {
        (1u64 << bps) - 1
    };
    if x.iter().all(|&v| v == x[0]) {
        w.bits(0, 1);
        w.bits(0, 6); // CONSTANT
        w.bits(0, 1);
        w.bits((x[0] as u64) & mask, bps);
        return;
    }
    if x.len() > 2 * LPC_ORDER {
        write_lpc(w, x, bps, LPC_ORDER);
    } else {
        write_fixed(w, x, bps);
    }
}

fn block_size_code(bs: usize) -> (u64, bool) {
    match bs {
        4096 => (0b1100, false),
        _ => (0b0111, true), // 16-bit (blocksize-1) appended
    }
}
fn sample_size_code(bps: u32) -> u64 {
    match bps {
        8 => 0b001,
        12 => 0b010,
        16 => 0b100,
        20 => 0b101,
        24 => 0b110,
        _ => 0b000,
    }
}

/// Encode native samples (per-sample value already at `bps`) → real FLAC bytes. -/
pub fn encode(samples: &[i32], rate: u32, channels: u16, bps: u32) -> Vec<u8> {
    let ch = channels.max(1) as usize;
    let frames = samples.len() / ch;
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");

    // STREAMINFO (last metadata block, type 0, length 34)
    let mut s = W::new();
    s.bits(1, 1); // last-metadata-block
    s.bits(0, 7); // STREAMINFO
    s.bits(34, 24); // length
    s.bits(BLOCK as u64, 16); // min blocksize
    s.bits(BLOCK as u64, 16); // max blocksize
    s.bits(0, 24); // min framesize (unknown)
    s.bits(0, 24); // max framesize (unknown)
    s.bits(rate as u64, 20);
    s.bits((ch - 1) as u64, 3);
    s.bits((bps - 1) as u64, 5);
    s.bits(frames as u64, 36); // total samples
    for _ in 0..16 {
        s.bits(0, 8); // MD5 = 0 (not computed)
    }
    out.extend_from_slice(&s.buf);

    // de-interleave
    let mut chans: Vec<Vec<i64>> = vec![Vec::with_capacity(frames); ch];
    for f in 0..frames {
        for c in 0..ch {
            chans[c].push(samples[f * ch + c] as i64);
        }
    }

    let mut fnum = 0u64;
    let mut start = 0usize;
    while start < frames {
        let bs = (frames - start).min(BLOCK);
        let mut w = W::new();
        // frame header
        w.bits(0b11111111111110, 14); // sync
        w.bits(0, 1); // reserved
        w.bits(0, 1); // blocking strategy: fixed
        let (bsc, bs_ext) = block_size_code(bs);
        w.bits(bsc, 4);
        w.bits(0, 4); // sample rate: from STREAMINFO
        w.bits((ch - 1) as u64, 4); // channel assignment: independent
        w.bits(sample_size_code(bps), 3);
        w.bits(0, 1); // reserved
        write_utf8(&mut w, fnum);
        if bs_ext {
            w.bits((bs - 1) as u64, 16);
        }
        // CRC-8 over the (byte-aligned) header so far
        let c8 = crc8(&w.buf);
        w.bits(c8 as u64, 8);
        // subframes
        for c in 0..ch {
            write_subframe(&mut w, &chans[c][start..start + bs], bps);
        }
        w.align();
        let c16 = crc16(&w.buf);
        w.bits(c16 as u64, 16);
        out.extend_from_slice(&w.buf);

        start += bs;
        fnum += 1;
    }
    out
}

// ─── decoder: read real .flac (any encoder's output) → native samples ─────────

struct R<'a> {
    d: &'a [u8],
    pos: usize,
}
impl<'a> R<'a> {
    fn new(d: &'a [u8]) -> Self {
        R { d, pos: 0 }
    }
    fn bit(&mut self) -> u64 {
        let (byte, off) = (self.pos / 8, 7 - (self.pos % 8));
        self.pos += 1;
        if byte < self.d.len() {
            ((self.d[byte] >> off) & 1) as u64
        } else {
            0
        }
    }
    fn bits(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }
    fn sbits(&mut self, n: u32) -> i64 {
        if n == 0 {
            return 0;
        }
        let v = self.bits(n);
        let sh = 64 - n;
        ((v << sh) as i64) >> sh
    }
    fn unary(&mut self) -> u32 {
        let mut q = 0u32;
        while self.bit() == 0 {
            q += 1;
        }
        q
    }
    fn align(&mut self) {
        if self.pos % 8 != 0 {
            self.pos += 8 - (self.pos % 8);
        }
    }
}

fn read_utf8(r: &mut R) {
    let b0 = r.bits(8);
    if b0 < 0x80 {
        return;
    }
    let mut nc = 0u32;
    let mut m = 0x40u64;
    while b0 & m != 0 {
        nc += 1;
        m >>= 1;
    }
    for _ in 0..nc {
        r.bits(8);
    }
}

fn decode_residual(r: &mut R, blocksize: usize, order: usize) -> Vec<i64> {
    let method = r.bits(2);
    let pbits = if method == 0 { 4 } else { 5 };
    let po = r.bits(4) as u32;
    let parts = 1usize << po;
    let mut res = Vec::with_capacity(blocksize - order);
    for p in 0..parts {
        let cnt = if p == 0 {
            (blocksize >> po) - order
        } else {
            blocksize >> po
        };
        let k = r.bits(pbits) as u32;
        let escape = (pbits == 4 && k == 0xF) || (pbits == 5 && k == 0x1F);
        if escape {
            let bw = r.bits(5) as u32;
            for _ in 0..cnt {
                res.push(r.sbits(bw));
            }
        } else {
            for _ in 0..cnt {
                let q = r.unary();
                let rem = if k > 0 { r.bits(k) } else { 0 };
                let u = ((q as u64) << k) | rem;
                res.push(((u >> 1) as i64) ^ -((u & 1) as i64));
            }
        }
    }
    res
}

fn decode_subframe(r: &mut R, blocksize: usize, bps: u32) -> Vec<i64> {
    let _pad = r.bit();
    let typ = r.bits(6);
    let wasted = if r.bit() == 1 { r.unary() + 1 } else { 0 };
    let b = bps - wasted;
    let mut s: Vec<i64> = if typ == 0 {
        vec![r.sbits(b); blocksize]
    } else if typ == 1 {
        (0..blocksize).map(|_| r.sbits(b)).collect()
    } else if typ & 0b111000 == 0b001000 {
        let order = (typ & 0b111) as usize;
        let mut s: Vec<i64> = (0..order).map(|_| r.sbits(b)).collect();
        let res = decode_residual(r, blocksize, order);
        for i in 0..res.len() {
            let n = order + i;
            let p = match order {
                0 => 0,
                1 => s[n - 1],
                2 => 2 * s[n - 1] - s[n - 2],
                3 => 3 * s[n - 1] - 3 * s[n - 2] + s[n - 3],
                _ => 4 * s[n - 1] - 6 * s[n - 2] + 4 * s[n - 3] - s[n - 4],
            };
            s.push(res[i] + p);
        }
        s
    } else if typ & 0b100000 == 0b100000 {
        let order = ((typ & 0b011111) + 1) as usize;
        let mut s: Vec<i64> = (0..order).map(|_| r.sbits(b)).collect();
        let prec = (r.bits(4) as u32) + 1;
        let shift = r.bits(5) as i64;
        let coeffs: Vec<i64> = (0..order).map(|_| r.sbits(prec)).collect();
        let res = decode_residual(r, blocksize, order);
        for i in 0..res.len() {
            let n = order + i;
            let mut p = 0i64;
            for j in 0..order {
                p += coeffs[j] * s[n - 1 - j];
            }
            s.push(res[i] + (p >> shift));
        }
        s
    } else {
        vec![0i64; blocksize]
    };
    if wasted > 0 {
        for v in s.iter_mut() {
            *v <<= wasted;
        }
    }
    s
}

/// Decode a real FLAC stream → (native samples interleaved, rate, channels, bps).
pub fn decode(data: &[u8]) -> Result<(Vec<i32>, u32, u16, u32), String> {
    if data.len() < 4 || &data[0..4] != b"fLaC" {
        return Err("not a FLAC stream".into());
    }
    let mut r = R::new(data);
    r.pos = 32; // past "fLaC"
    let (mut rate, mut channels, mut bps, mut total) = (0u32, 0u16, 0u32, 0u64);
    loop {
        let last = r.bits(1);
        let btype = r.bits(7);
        let len = r.bits(24) as usize;
        let blk_start = r.pos;
        if btype == 0 {
            r.bits(16);
            r.bits(16);
            r.bits(24);
            r.bits(24); // block/frame sizes
            rate = r.bits(20) as u32;
            channels = (r.bits(3) + 1) as u16;
            bps = (r.bits(5) + 1) as u32;
            total = r.bits(36);
        }
        r.pos = blk_start + len * 8; // skip to end of metadata block
        if last == 1 {
            break;
        }
    }
    if rate == 0 || channels == 0 {
        return Err("missing STREAMINFO".into());
    }
    let ch = channels as usize;
    let mut chans: Vec<Vec<i64>> = vec![Vec::new(); ch];
    let mut got = 0u64;
    while got < total {
        // frame header
        if r.bits(14) != 0b11111111111110 {
            return Err(format!("lost sync at sample {got}"));
        }
        r.bits(1); // reserved
        r.bits(1); // blocking strategy
        let bsc = r.bits(4);
        let src = r.bits(4);
        let cassign = r.bits(4);
        let _ssc = r.bits(3);
        r.bits(1); // reserved
        read_utf8(&mut r);
        let bs = match bsc {
            1 => 192usize,
            2..=5 => 576usize << (bsc - 2),
            6 => (r.bits(8) + 1) as usize,
            7 => (r.bits(16) + 1) as usize,
            8..=15 => 256usize << (bsc - 8),
            _ => return Err("bad blocksize code".into()),
        };
        match src {
            12 => {
                r.bits(8);
            }
            13 | 14 => {
                r.bits(16);
            }
            _ => {}
        }
        r.bits(8); // CRC-8 (header is byte-aligned here)
                   // subframes (decorrelation modes give the "side" channel +1 bit)
        let nsub = if cassign < 8 {
            (cassign + 1) as usize
        } else {
            2
        };
        let mut sub: Vec<Vec<i64>> = Vec::with_capacity(nsub);
        for c in 0..nsub {
            let side_bonus = match cassign {
                0b1000 => {
                    if c == 1 {
                        1
                    } else {
                        0
                    }
                } // left/side
                0b1001 => {
                    if c == 0 {
                        1
                    } else {
                        0
                    }
                } // side/right
                0b1010 => {
                    if c == 1 {
                        1
                    } else {
                        0
                    }
                } // mid/side
                _ => 0,
            };
            sub.push(decode_subframe(&mut r, bs, bps + side_bonus));
        }
        r.align();
        r.bits(16); // CRC-16
                    // channel decorrelation → independent L/R
        let frame: Vec<Vec<i64>> = match cassign {
            0b1000 => {
                let (l, s) = (&sub[0], &sub[1]);
                vec![l.clone(), (0..bs).map(|i| l[i] - s[i]).collect()]
            }
            0b1001 => {
                let (s, rr) = (&sub[0], &sub[1]);
                vec![(0..bs).map(|i| rr[i] + s[i]).collect(), rr.clone()]
            }
            0b1010 => {
                let (m, s) = (&sub[0], &sub[1]);
                let l: Vec<i64> = (0..bs)
                    .map(|i| {
                        let m2 = (m[i] << 1) | (s[i] & 1);
                        (m2 + s[i]) >> 1
                    })
                    .collect();
                let rr: Vec<i64> = (0..bs)
                    .map(|i| {
                        let m2 = (m[i] << 1) | (s[i] & 1);
                        (m2 - s[i]) >> 1
                    })
                    .collect();
                vec![l, rr]
            }
            _ => sub,
        };
        for c in 0..ch {
            chans[c].extend_from_slice(&frame[c]);
        }
        got += bs as u64;
    }
    // interleave
    let frames = chans[0].len();
    let mut out = Vec::with_capacity(frames * ch);
    for f in 0..frames {
        for c in 0..ch {
            out.push(chans[c][f] as i32);
        }
    }
    Ok((out, rate, channels, bps))
}
