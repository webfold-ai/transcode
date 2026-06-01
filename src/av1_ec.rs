// ═══════════════════════════════════════════════════════════════════════════
// av1_ec.rs — AV1 entropy-coder foundation: CDF model + adaptive multi-symbol
//             arithmetic coding (rung 1 of an AV1/AVIF encoder)
// ═══════════════════════════════════════════════════════════════════════════
// AV1 codes every syntax element as a symbol drawn from an adapting CDF. This
// module implements:
//   * the AV1 CDF representation — `AOM_ICDF` (inverse CDF, 15-bit, the array
//     is monotonically DECREASING with an implicit 0 terminator + an
//     adaptation counter), and
//   * the AV1 symbol adaptation `update_cdf` (libaom-exact: rate from the
//     counter + symbol count, move toward the coded symbol),
//   * on a correct, reversible carryless range-coder core (Subbotin-style).
//
// VERIFICATION SCOPE (honest):
//   * What the tests prove: this is a CORRECT, REVERSIBLE adaptive multi-symbol
//     arithmetic coder — encode N symbols against adapting CDFs, decode, and
//     every symbol + the final CDF state matches (round-trip + adaptation).
//   * What they do NOT prove: byte-identical output to libaom's daala range
//     coder. AV1 bitstream conformance (so dav1d/ffmpeg can decode it) requires
//     swapping this range core for libaom's exact carry-resolved byte layout
//     and is only verifiable once a full frame/OBU exists. That is the next
//     rung; this rung is the symbol model + adaptation, which is the bulk of
//     the conceptual machinery and is independently testable.
// ═══════════════════════════════════════════════════════════════════════════

const CDF_PROB_TOP: u32 = 1 << 15; // 32768

// ── AV1 symbol adaptation (libaom update_cdf, exact) ─────────────────────────
// `cdf` layout: cdf[0..nsymbs] are AOM_ICDF values (decreasing; cdf[nsymbs-1]
// is the implicit 0), and cdf[nsymbs] is the adaptation counter.
fn update_cdf(cdf: &mut [u16], val: usize, nsymbs: usize) {
    const NSYMBS2SPEED: [u32; 17] = [
        0, 0, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    ];
    let count = cdf[nsymbs];
    let rate = 3 + (count > 15) as u32 + (count > 31) as u32 + NSYMBS2SPEED[nsymbs];
    let mut tmp: i32 = 0; // becomes the target each step
    for (i, slot) in cdf.iter_mut().enumerate().take(nsymbs - 1) {
        // libaom: tmp starts at 32768; once i reaches val it drops to 0
        tmp = if i == val { 0 } else if i < val { CDF_PROB_TOP as i32 } else { 0 };
        let cur = *slot as i32;
        if tmp < cur {
            *slot = (cur - ((cur - tmp) >> rate)) as u16;
        } else {
            *slot = (cur + ((tmp - cur) >> rate)) as u16;
        }
    }
    if count < 32 {
        cdf[nsymbs] = count + 1;
    }
}

/// Build an initial uniform CDF for `nsymbs` symbols in AOM_ICDF form
/// (`cdf[0..nsymbs]` decreasing to 0, `cdf[nsymbs]` = adaptation counter 0).
pub fn uniform_cdf(nsymbs: usize) -> Vec<u16> {
    let mut cdf = vec![0u16; nsymbs + 1];
    for (i, slot) in cdf.iter_mut().enumerate().take(nsymbs) {
        // cumulative F(i) = (i+1)/nsymbs of 32768; icdf = 32768 - F(i)
        let f = ((i + 1) as u32 * CDF_PROB_TOP) / nsymbs as u32;
        *slot = (CDF_PROB_TOP - f) as u16;
    }
    cdf[nsymbs - 1] = 0; // exact terminator
    cdf // cdf[nsymbs] = 0 counter
}

// ── carryless range coder (Subbotin), 32-bit, correct & reversible ───────────
const RC_TOP: u32 = 1 << 24;
const RC_BOT: u32 = 1 << 16;

pub struct Writer {
    low: u32,
    range: u32,
    buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Writer { low: 0, range: 0xFFFF_FFFF, buf: Vec::new() }
    }
    fn encode_freq(&mut self, cum: u32, freq: u32, tot: u32) {
        let r = self.range / tot;
        self.low = self.low.wrapping_add(r * cum);
        self.range = r * freq;
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) < RC_TOP {
                // top byte settled
            } else if self.range < RC_BOT {
                self.range = self.low.wrapping_neg() & (RC_BOT - 1); // underflow squeeze
            } else {
                break;
            }
            self.buf.push((self.low >> 24) as u8);
            self.low <<= 8;
            self.range <<= 8;
        }
    }
    /// Encode one symbol `s` against the adaptive `cdf` (AOM_ICDF, `nsymbs`).
    pub fn symbol(&mut self, s: usize, cdf: &mut [u16], nsymbs: usize) {
        let icdf_prev = if s == 0 { CDF_PROB_TOP } else { cdf[s - 1] as u32 };
        let icdf_cur = cdf[s] as u32;
        let cum = CDF_PROB_TOP - icdf_prev;
        let freq = icdf_prev - icdf_cur;
        self.encode_freq(cum, freq, CDF_PROB_TOP);
        update_cdf(cdf, s, nsymbs);
    }
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.buf.push((self.low >> 24) as u8);
            self.low <<= 8;
        }
        self.buf
    }
}

pub struct Reader<'a> {
    low: u32,
    range: u32,
    code: u32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut r = Reader { low: 0, range: 0xFFFF_FFFF, code: 0, data, pos: 0 };
        for _ in 0..4 {
            r.code = (r.code << 8) | r.next();
        }
        r
    }
    fn next(&mut self) -> u32 {
        let b = if self.pos < self.data.len() { self.data[self.pos] } else { 0 };
        self.pos += 1;
        b as u32
    }
    fn get_freq(&mut self, tot: u32) -> u32 {
        let r = self.range / tot;
        let v = self.code.wrapping_sub(self.low) / r;
        v.min(tot - 1)
    }
    fn decode_freq(&mut self, cum: u32, freq: u32, tot: u32) {
        let r = self.range / tot;
        self.low = self.low.wrapping_add(r * cum);
        self.range = r * freq;
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) < RC_TOP {
            } else if self.range < RC_BOT {
                self.range = self.low.wrapping_neg() & (RC_BOT - 1);
            } else {
                break;
            }
            self.code = (self.code << 8) | self.next();
            self.low <<= 8;
            self.range <<= 8;
        }
    }
    /// Decode one symbol against the adaptive `cdf` (must mirror the encoder).
    pub fn symbol(&mut self, cdf: &mut [u16], nsymbs: usize) -> usize {
        let v = self.get_freq(CDF_PROB_TOP);
        // find s: cum(s) <= v < cum(s)+freq(s), cum(s) = 32768 - icdf[s-1]
        let mut s = 0usize;
        while s < nsymbs - 1 {
            let icdf_cur = cdf[s] as u32;
            let cum_next = CDF_PROB_TOP - icdf_cur; // = cum(s)+freq(s)
            if v < cum_next {
                break;
            }
            s += 1;
        }
        let icdf_prev = if s == 0 { CDF_PROB_TOP } else { cdf[s - 1] as u32 };
        let icdf_cur = cdf[s] as u32;
        let cum = CDF_PROB_TOP - icdf_prev;
        let freq = icdf_prev - icdf_cur;
        self.decode_freq(cum, freq, CDF_PROB_TOP);
        update_cdf(cdf, s, nsymbs);
        s
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — round-trip + adaptation correctness
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_adaptive_multisymbol() {
        // deterministic pseudo-random symbol stream over mixed alphabet sizes
        let mut st = 0xC0FFEEu32;
        let mut rng = || {
            st = st.wrapping_mul(1664525).wrapping_add(1013904223);
            st
        };
        let n = 100_000;
        // pre-generate (nsymbs, symbol) with a skewed distribution per stream
        let mut plan = Vec::with_capacity(n);
        for _ in 0..n {
            let nsymbs = 2 + (rng() >> 28) as usize % 14; // 2..=15
            // skew toward low symbols (realistic)
            let r = rng() >> 8;
            let s = if r % 100 < 70 {
                (rng() as usize) % nsymbs.min(3)
            } else {
                (rng() as usize) % nsymbs
            };
            plan.push((nsymbs, s));
        }

        // encode (CDFs adapt as we go)
        let mut w = Writer::new();
        let mut enc_cdfs: std::collections::HashMap<usize, Vec<u16>> = Default::default();
        for &(nsymbs, s) in &plan {
            let cdf = enc_cdfs.entry(nsymbs).or_insert_with(|| uniform_cdf(nsymbs));
            w.symbol(s, cdf, nsymbs);
        }
        let bytes = w.finish();

        // decode with independently-initialised CDFs that must track identically
        let mut r = Reader::new(&bytes);
        let mut dec_cdfs: std::collections::HashMap<usize, Vec<u16>> = Default::default();
        for (i, &(nsymbs, s)) in plan.iter().enumerate() {
            let cdf = dec_cdfs.entry(nsymbs).or_insert_with(|| uniform_cdf(nsymbs));
            let got = r.symbol(cdf, nsymbs);
            assert_eq!(got, s, "symbol {i} mismatch (nsymbs={nsymbs})");
        }
        // final adaptation state must be identical encoder vs decoder
        assert_eq!(enc_cdfs, dec_cdfs, "CDF adaptation diverged");

        let bits = bytes.len() as f64 * 8.0 / n as f64;
        eprintln!("AV1 entropy coder: {n} symbols → {} bytes ({bits:.2} bits/sym)", bytes.len());
    }

    #[test]
    fn binary_roundtrip() {
        // 2-symbol (bool) stream, heavily skewed — should compress well below 1 bit
        let mut w = Writer::new();
        let mut cdf = uniform_cdf(2);
        let mut st = 1u32;
        let mut bits = Vec::new();
        for _ in 0..20_000 {
            st = st.wrapping_mul(1103515245).wrapping_add(12345);
            let b = if (st >> 24) % 10 < 9 { 0usize } else { 1 }; // 90% zeros
            bits.push(b);
            w.symbol(b, &mut cdf, 2);
        }
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        let mut cdf2 = uniform_cdf(2);
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(r.symbol(&mut cdf2, 2), b, "bool {i}");
        }
        let bps = bytes.len() as f64 * 8.0 / bits.len() as f64;
        eprintln!("AV1 bool coder: 90/10 skew → {bps:.3} bits/sym (entropy ≈ 0.47)");
        assert!(bps < 0.7, "skewed bool stream should compress below 0.7 bits/sym, got {bps}");
    }
}
