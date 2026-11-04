// ═══════════════════════════════════════════════════════════════════════════
// codec.rs — minimal lossless audio codec, assembling the proven decode atoms
// ═══════════════════════════════════════════════════════════════════════════
//
// The decode path is the chain proven in proof/:
//   • frame sync          (FrameHeaderSelection.sync_match_iff → the 0xAA check)
//   • blocksize           (FrameHeaderSelection.blockSize / decodeBoundary)
//   • Rice residuals      (RiceDecodeSelection → CodecBoundaries.decodeBoundary)
//   • order-1 LPC synth   (LpcSynthesisCollapse.lpc_closed_form, a = 1: x[n]=x[n-1]+e[n])
//
// Bit/byte I/O is the boundary interop (per HOW_TO_USE_IOTA: I/O at the boundary,
// the residual/predict compute is the platonic part). Lossless: decode∘encode = id.
// ═══════════════════════════════════════════════════════════════════════════

const BLOCK: usize = 4096;
const SYNC: u64 = 0xAA;

// ─── bit I/O (MSB-first) ──────────────────────────────────────────────────────

struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u8,
}
impl BitWriter {
    fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | ((b & 1) as u8);
        self.nbits += 1;
        if self.nbits == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }
    fn bits(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            self.bit(((v >> i) & 1) as u32);
        }
    }
    fn unary(&mut self, q: u64) {
        for _ in 0..q {
            self.bit(1);
        }
        self.bit(0);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.cur <<= 8 - self.nbits;
            self.buf.push(self.cur);
        }
        self.buf
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }
    fn bit(&mut self) -> u32 {
        let byte = self.pos / 8;
        let off = 7 - (self.pos % 8);
        self.pos += 1;
        if byte < self.buf.len() {
            ((self.buf[byte] >> off) & 1) as u32
        } else {
            0
        }
    }
    fn bits(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit() as u64;
        }
        v
    }
    fn unary(&mut self) -> u64 {
        let mut q = 0u64;
        while self.bit() == 1 {
            q += 1;
        }
        q
    }
}

// ─── zigzag (signed → unsigned) + Rice code ───────────────────────────────────

fn zigzag(x: i64) -> u64 {
    ((x << 1) ^ (x >> 63)) as u64
}
fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn rice_encode(bw: &mut BitWriter, u: u64, k: u32) {
    bw.unary(u >> k);
    if k > 0 {
        bw.bits(u & ((1 << k) - 1), k);
    }
}
fn rice_decode(br: &mut BitReader, k: u32) -> u64 {
    let q = br.unary();
    let r = if k > 0 { br.bits(k) } else { 0 };
    (q << k) | r
}

/// Pick the Rice parameter `k` minimising the unary blow-up: roughly log2(mean). -/
fn choose_k(u: &[u64]) -> u32 {
    if u.is_empty() {
        return 0;
    }
    let mean = (u.iter().map(|&x| x as u128).sum::<u128>() / u.len() as u128) as u64;
    let k = if mean == 0 {
        0
    } else {
        63 - mean.leading_zeros()
    };
    k.min(30)
}

// ─── channel de/interleave ────────────────────────────────────────────────────

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

// ─── order-p LPC: optimal predictor = δS=0 stationary point (Levinson-Durbin) ──

const LPC_ORDER: usize = 12; // fixed order (closed-form Levinson); a measured
                             // order-search beats a closed-form estimate here,
                             // so we take the fast fixed-order operating point.
const COEFF_PREC: u32 = 15; // bits per quantised coefficient

/// Partitioned-Rice encode: split residuals into ~256-sample partitions, each
/// with its own `k` chosen closed-form from the partition mean (a per-partition
/// selection, no search). Stores partition order (3 bits) then per-partition k.
fn encode_residuals(bw: &mut BitWriter, resid: &[u64]) {
    let n = resid.len();
    let mut po = 0u32;
    while (n >> (po + 1)) >= 128 && po < 6 {
        po += 1;
    }
    let parts = 1usize << po;
    let psize = n / parts;
    bw.bits(po as u64, 3);
    for p in 0..parts {
        let start = p * psize;
        let end = if p == parts - 1 { n } else { start + psize };
        let slice = &resid[start..end];
        let k = choose_k(slice);
        bw.bits(k as u64, 5);
        for &u in slice {
            rice_encode(bw, u, k);
        }
    }
}

fn decode_residuals(br: &mut BitReader, n: usize) -> Vec<u64> {
    let po = br.bits(3) as u32;
    let parts = 1usize << po;
    let psize = n / parts;
    let mut out = Vec::with_capacity(n);
    for p in 0..parts {
        let cnt = if p == parts - 1 {
            n - psize * (parts - 1)
        } else {
            psize
        };
        let k = br.bits(5) as u32;
        for _ in 0..cnt {
            out.push(rice_decode(br, k));
        }
    }
    out
}

/// Welch-windowed autocorrelation r[0..=lag] — the action's curvature. -/
fn autocorr(x: &[i32], lag: usize) -> Vec<f64> {
    let n = x.len();
    let half = (n as f64 - 1.0) / 2.0;
    let w: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &xi)| {
            let t = if half > 0.0 {
                (i as f64 - half) / half
            } else {
                0.0
            };
            xi as f64 * (1.0 - t * t)
        })
        .collect();
    (0..=lag)
        .map(|k| (k..n).map(|i| w[i] * w[i - k]).sum())
        .collect()
}

/// Levinson-Durbin recursion: the closed-form solution of the normal equations
/// — the predictor where the variation of the residual-energy action is zero
/// (δS = 0). No candidate search; the optimum is *computed*. Coeffs `a[i]`
/// predict `x̂[n] = Σ a[i]·x[n-1-i]`. -/
fn levinson_full(r: &[f64], max_order: usize) -> (Vec<Vec<f64>>, Vec<f64>) {
    let mut coeffs: Vec<Vec<f64>> = vec![Vec::new()]; // order 0
    let mut errs = vec![r[0]];
    let mut a: Vec<f64> = Vec::new();
    let mut e = r[0];
    for i in 0..max_order {
        if e <= 0.0 {
            while coeffs.len() <= max_order {
                coeffs.push(a.clone());
                errs.push(e);
            }
            break;
        }
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / e;
        let mut na = a.clone();
        na.push(0.0);
        na[i] = k;
        for j in 0..i {
            na[j] = a[j] - k * a[i - 1 - j];
        }
        a = na;
        e *= 1.0 - k * k;
        coeffs.push(a.clone());
        errs.push(e.max(0.0));
    }
    (coeffs, errs)
}

/// Quantise float coeffs to integers (with error feedback), choosing the shift. -/
fn quantize(lpc: &[f64], prec: u32) -> (Vec<i32>, i32) {
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

fn encode_block(bw: &mut BitWriter, x: &[i32]) {
    let n = x.len();
    let max_o = LPC_ORDER.min(n.saturating_sub(1) / 2);
    if max_o < 1 {
        // tiny block → order-1 delta fallback (lpc_closed_form, a = 1)
        bw.bits(0, 6);
        let mut prev = 0i64;
        let u: Vec<u64> = x
            .iter()
            .map(|&xi| {
                let e = xi as i64 - prev;
                prev = xi as i64;
                zigzag(e)
            })
            .collect();
        let k = choose_k(&u);
        bw.bits(k as u64, 5);
        for uu in u {
            rice_encode(bw, uu, k);
        }
        return;
    }
    let order = max_o; // = LPC_ORDER (12) for normal blocks; closed-form Levinson
    let r = autocorr(x, order);
    let (coeffs, _) = levinson_full(&r, order);
    let (q, shift) = quantize(&coeffs[order], COEFF_PREC);
    bw.bits(order as u64, 6);
    bw.bits(shift as u64, 4);
    for &qc in &q {
        bw.bits(zigzag(qc as i64), COEFF_PREC); // signed coeff, zigzagged
    }
    // residual e[n] = x[n] - (Σ q[i]·x[n-1-i] >> shift); warm-up (n<order) verbatim
    let resid: Vec<u64> = (0..n)
        .map(|nn| {
            let e = if nn < order {
                x[nn] as i64
            } else {
                let mut p = 0i64;
                for i in 0..order {
                    p += q[i] as i64 * x[nn - 1 - i] as i64;
                }
                x[nn] as i64 - (p >> shift)
            };
            zigzag(e)
        })
        .collect();
    encode_residuals(bw, &resid);
}

fn decode_block(br: &mut BitReader, bs: usize) -> Vec<i32> {
    let order = br.bits(6) as usize;
    if order == 0 {
        let k = br.bits(5) as u32;
        let mut prev = 0i64;
        let mut out = Vec::with_capacity(bs);
        for _ in 0..bs {
            let e = unzigzag(rice_decode(br, k));
            let x = prev + e;
            out.push(x as i32);
            prev = x;
        }
        return out;
    }
    let shift = br.bits(4) as i64;
    let q: Vec<i64> = (0..order).map(|_| unzigzag(br.bits(COEFF_PREC))).collect();
    let resid = decode_residuals(br, bs);
    let mut out = Vec::with_capacity(bs);
    for nn in 0..bs {
        let e = unzigzag(resid[nn]);
        if nn < order {
            out.push(e as i32);
        } else {
            let mut p = 0i64;
            for i in 0..order {
                p += q[i] * out[nn - 1 - i] as i64;
            }
            out.push(((p >> shift) + e) as i32); // lpc_closed_form, order p
        }
    }
    out
}

// ─── container (canonical i32 samples ↔ SLAC bytes) ───────────────────────────

/// Encode canonical-i32 samples to SLAC bytes. `fmt_code` records the source
/// WAV format so decode reproduces it exactly; `shift` un-scales the canonical
/// full-scale value to the native sample so residuals are properly sized.
pub fn encode(samples: &[i32], rate: u32, channels: u16, fmt_code: u8, shift: u32) -> Vec<u8> {
    let ch = channels as usize;
    let frames = if ch == 0 { 0 } else { samples.len() / ch };
    let mut out = Vec::new();
    out.extend_from_slice(b"SLA1");
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&(frames as u32).to_le_bytes());
    out.push(fmt_code);
    out.push(shift as u8);

    let native: Vec<i32> = samples.iter().map(|&v| v >> shift).collect();
    let chans = deinterleave(&native, ch.max(1));
    let mut bw = BitWriter::new();
    let mut f = 0;
    while f < frames {
        let bs = (frames - f).min(BLOCK);
        bw.bits(SYNC, 8); // frame sync (the sync_match boundary)
        bw.bits(bs as u64, 16); // blocksize
        for c in 0..ch {
            encode_block(&mut bw, &chans[c][f..f + bs]);
        }
        f += bs;
    }
    out.extend_from_slice(&bw.finish());
    out
}

/// Decode SLAC bytes → (canonical i32 samples, rate, channels, fmt_code).
pub fn decode(data: &[u8]) -> Result<(Vec<i32>, u32, u16, u8), String> {
    if data.len() < 16 || &data[0..4] != b"SLA1" {
        return Err("not a SLAC stream".into());
    }
    let rate = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let channels = u16::from_le_bytes([data[8], data[9]]);
    let frames = u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;
    let fmt_code = data[14];
    let shift = data[15] as u32;
    let ch = channels.max(1) as usize;

    let mut br = BitReader::new(&data[16..]);
    let mut chans: Vec<Vec<i32>> = vec![Vec::with_capacity(frames); ch];
    let mut got = 0;
    while got < frames {
        if br.bits(8) != SYNC {
            return Err(format!("sync lost at frame {got}")); // sync_match failed
        }
        let bs = br.bits(16) as usize;
        if bs == 0 || got + bs > frames {
            return Err("bad blocksize".into());
        }
        for c in 0..ch {
            chans[c].extend_from_slice(&decode_block(&mut br, bs));
        }
        got += bs;
    }
    let native = interleave(&chans);
    let samples: Vec<i32> = native.iter().map(|&v| v << shift).collect();
    Ok((samples, rate, channels, fmt_code))
}

// ─── self-test: lossless round-trip ───────────────────────────────────────────

pub fn selftest() -> i32 {
    let mut fails = 0;
    for &(ch, n) in &[(1usize, 9000usize), (2, 9000)] {
        // a smooth multi-tone signal at native 16-bit scale, interleaved
        let mut samples = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in 0..ch {
                let t = i as f64 / 16000.0;
                let v =
                    8000.0 * (2.0 * std::f64::consts::PI * (300.0 + 120.0 * c as f64) * t).sin();
                samples.push((v as i32) << 16); // canonical full-scale (16-bit source)
            }
        }
        let enc = encode(&samples, 16000, ch as u16, 1 /*I16*/, 16);
        match decode(&enc) {
            Ok((dec, rate, dch, fmt)) => {
                let lossless = dec == samples && rate == 16000 && dch == ch as u16 && fmt == 1;
                let raw = samples.len() * 4;
                let ratio = raw as f64 / enc.len() as f64;
                if !lossless {
                    fails += 1;
                }
                println!(
                    "  {ch}ch x {n}: raw={raw}B enc={}B  ratio={ratio:.2}x  lossless={}  [{}]",
                    enc.len(),
                    lossless,
                    if lossless { "ok" } else { "FAIL" }
                );
            }
            Err(e) => {
                fails += 1;
                println!("  {ch}ch x {n}: decode error: {e}  [FAIL]");
            }
        }
    }
    if fails == 0 {
        println!("codec selftest: all passed (decode∘encode = identity)");
        0
    } else {
        println!("codec selftest: {fails} FAILED");
        1
    }
}
