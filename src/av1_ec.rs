// ═══════════════════════════════════════════════════════════════════════════
// av1_ec.rs — AV1 entropy coder: the EXACT daala/od_ec symbol arithmetic
// ═══════════════════════════════════════════════════════════════════════════
// AV1 codes every syntax element as a symbol drawn from an adapting CDF, through
// a 15-bit multi-symbol arithmetic coder. For a bitstream dav1d/ffmpeg can
// decode, the coder must be byte-identical to the AV1 reference — so this is
// transcribed verbatim, not derived:
//   * Writer  — the libaom/rav1e `od_ec` ENCODER (carry-deferred `precarry`
//     scheme, rng=0x8000, cnt=-9, EC_PROB_SHIFT=6, EC_MIN_PROB=4). Lifted from
//     rav1e `src/ec.rs` (the reference AV1 encoder).
//   * Reader  — dav1d's `msac` DECODER (rng=0x8000, cnt=-15, 64-bit `dif`
//     window, byte-inverted refill). Lifted from dav1d `src/msac.c` — the exact
//     decoder ffmpeg runs via libdav1d.
//   * update_cdf — the AV1 symbol adaptation `rate = 4 + (count>>4) + (n>2)`,
//     applied identically on both sides (dav1d form; algebraically equal to
//     rav1e's `3 + min(nsymbs>>1,2) + (count>>4)`).
//
// The "x = B/U" thesis in its purest codec form: the arithmetic coder IS an
// affine recurrence on the (low, rng) state — `S_t = A·S_{t-1} + B` — a
// fixed-state GET/update per symbol, never a search. The CDF lookup is the
// literal hashmap GET; renormalisation is the carry. (sanguine book,
// equations §8 affine scan; SpinorLoop.)
//
// VERIFICATION: the tests encode N symbols against adapting CDFs with the
// rav1e `od_ec` Writer and decode them with the dav1d `msac` Reader — i.e. a
// cross-check of our encoder against the *actual AV1 decoder arithmetic*. Every
// symbol and the final adapted CDF state must match. This is the conformance
// backbone every later AV1 rung (OBU/frame/tile/partition/coeff) builds on.
// ═══════════════════════════════════════════════════════════════════════════

const CDF_PROB_TOP: u16 = 1 << 15; // 32768
const EC_PROB_SHIFT: u32 = 6;
const EC_MIN_PROB: u32 = 4;

// ── AV1 symbol adaptation (dav1d msac update_cdf, exact) ─────────────────────
// `cdf` layout (AOM_ICDF): for `nsymbs` symbols, `cdf[0..nsymbs]` are the
// inverse-CDF values (monotonically decreasing, `cdf[nsymbs-1] == 0` terminator)
// and `cdf[nsymbs]` is the adaptation counter. Encoder and decoder MUST run this
// identically after each coded symbol.
fn update_cdf(cdf: &mut [u16], val: usize, nsymbs: usize) {
    let count = cdf[nsymbs] as u32;
    let rate = 4 + (count >> 4) + (nsymbs > 2) as u32;
    for (i, slot) in cdf.iter_mut().enumerate().take(nsymbs) {
        let v = *slot;
        if i < val {
            *slot = v + ((CDF_PROB_TOP - v) >> rate);
        } else {
            *slot = v - (v >> rate);
        }
    }
    cdf[nsymbs] = (count + (count < 32) as u32) as u16;
}

/// Build an initial uniform CDF for `nsymbs` symbols in AOM_ICDF form:
/// `cdf[0..nsymbs]` decreasing to a 0 terminator, `cdf[nsymbs]` = counter 0.
pub fn uniform_cdf(nsymbs: usize) -> Vec<u16> {
    let mut cdf = vec![0u16; nsymbs + 1];
    for (i, slot) in cdf.iter_mut().enumerate().take(nsymbs) {
        // cumulative F(i) = (i+1)/nsymbs of 32768; icdf = 32768 - F(i)
        let f = ((i + 1) as u32 * CDF_PROB_TOP as u32) / nsymbs as u32;
        *slot = (CDF_PROB_TOP as u32 - f) as u16;
    }
    cdf[nsymbs - 1] = 0; // exact terminator
    cdf // cdf[nsymbs] = 0 counter
}

// ═══════════════════════════════════════════════════════════════════════════
// Writer — the od_ec ENCODER (rav1e src/ec.rs, verbatim arithmetic)
// ═══════════════════════════════════════════════════════════════════════════

pub struct Writer {
    low: u32,
    rng: u16,
    cnt: i16,
    precarry: Vec<u16>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Writer { low: 0, rng: 0x8000, cnt: -9, precarry: Vec::new() }
    }

    /// Range split from the icdf boundaries `fl` (high) and `fh` (low) with
    /// `nms` symbols remaining. Returns (low_offset, new_range).
    fn lr_compute(&self, fl: u16, fh: u16, nms: u16) -> (u32, u16) {
        let r = self.rng as u32;
        let mut u = (((r >> 8) * (fl as u32 >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT))
            + EC_MIN_PROB * nms as u32;
        if fl >= CDF_PROB_TOP {
            u = r;
        }
        let v = (((r >> 8) * (fh as u32 >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT))
            + EC_MIN_PROB * (nms - 1) as u32;
        (r - u, (u - v) as u16)
    }

    /// Renormalise + carry-defer (od_ec_encode core): fold the new interval into
    /// `low`, emit settled bytes into `precarry`, normalise `rng` left.
    fn store(&mut self, fl: u16, fh: u16, nms: u16) {
        let (l, r) = self.lr_compute(fl, fh, nms);
        let mut low = l + self.low;
        let mut c = self.cnt;
        let d = r.leading_zeros() as i16; // r:u16 → 0..=15
        let mut s = c + d;
        if s >= 0 {
            c += 16;
            let mut m: u32 = (1u32 << c) - 1;
            if s >= 8 {
                self.precarry.push((low >> c) as u16);
                low &= m;
                c -= 8;
                m >>= 8;
            }
            self.precarry.push((low >> c) as u16);
            s = c + d - 24;
            low &= m;
        }
        self.low = low << d;
        self.rng = r << d;
        self.cnt = s;
    }

    /// Encode one symbol `s` against the adaptive `cdf` (AOM_ICDF, `nsymbs`).
    pub fn symbol(&mut self, s: usize, cdf: &mut [u16], nsymbs: usize) {
        let fl = if s > 0 { cdf[s - 1] } else { CDF_PROB_TOP };
        let fh = cdf[s];
        // dav1d's n_symbols = symbol_count − 1; the boundary for symbol s uses
        // EC_MIN_PROB·(n_symbols − s), i.e. nms−1 = nsymbs−1−s ⇒ nms = nsymbs − s.
        let nms = (nsymbs - s) as u16;
        self.store(fl, fh, nms);
        update_cdf(cdf, s, nsymbs);
    }

    /// Encode a non-adaptive boolean with probability `f`/32768 of being 0.
    pub fn bool(&mut self, val: bool, f: u16) {
        let s = val as usize;
        let fl = if s > 0 { f } else { CDF_PROB_TOP };
        let fh = if s > 0 { 0 } else { f };
        let nms = (2 - s) as u16;
        self.store(fl, fh, nms);
    }

    /// Encode an equiprobable bit (f = 16384).
    pub fn bit(&mut self, b: u32) {
        self.bool(b & 1 == 1, 16384);
    }

    /// Adaptive boolean against a 2-entry CDF `[icdf, count]` — dav1d
    /// `decode_bool_adapt`. Distinct from `symbol(_,_,2)`: the bool range split
    /// uses a single `EC_MIN_PROB`, and the adaptation rate is `4 + (count>>4)`.
    /// Used for AV1's skip, all_zero (txb_skip), and most binary flags.
    pub fn bool_adapt(&mut self, val: bool, cdf: &mut [u16; 2]) {
        self.bool(val, cdf[0]);
        let count = cdf[1];
        let rate = 4 + (count >> 4) as u32;
        if val {
            cdf[0] += (CDF_PROB_TOP - cdf[0]) >> rate;
        } else {
            cdf[0] -= cdf[0] >> rate;
        }
        cdf[1] = count + (count < 32) as u16;
    }

    /// Encode `bits` literal bits of `s`, MSB first (each equiprobable).
    pub fn literal(&mut self, bits: u8, s: u32) {
        for i in (0..bits).rev() {
            self.bit((s >> i) & 1);
        }
    }

    /// Flush (od_ec_enc_done): resolve deferred carries into the final bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let l = self.low;
        let mut c = self.cnt;
        let mut s = 10i16 + c;
        let m: u32 = 0x3FFF;
        let mut e = ((l + m) & !m) | (m + 1);
        if s > 0 {
            let mut n: u32 = (1u32 << (c + 16)) - 1;
            loop {
                self.precarry.push((e >> (c + 16)) as u16);
                e &= n;
                s -= 8;
                c -= 8;
                n >>= 8;
                if s <= 0 {
                    break;
                }
            }
        }
        // propagate carries through the precarry buffer (high → low)
        let mut carry: u32 = 0;
        let mut offs = self.precarry.len();
        let mut out = vec![0u8; offs];
        while offs > 0 {
            offs -= 1;
            carry += self.precarry[offs] as u32;
            out[offs] = carry as u8;
            carry >>= 8;
        }
        out
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Reader — dav1d's msac DECODER (src/msac.c, verbatim arithmetic)
// ═══════════════════════════════════════════════════════════════════════════

const EC_WIN_SIZE: u32 = 64; // ec_win = u64

pub struct Reader<'a> {
    dif: u64,
    rng: u32,
    cnt: i32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut r = Reader { dif: 0, rng: 0x8000, cnt: -15, data, pos: 0 };
        r.refill();
        r
    }

    fn refill(&mut self) {
        let mut c: i32 = EC_WIN_SIZE as i32 - self.cnt - 24;
        let mut dif = self.dif;
        loop {
            if self.pos >= self.data.len() {
                // dif |= ~(~(ec_win)0xff << c)
                dif |= !((!0xffu64) << c);
                break;
            }
            dif |= ((self.data[self.pos] ^ 0xff) as u64) << c;
            self.pos += 1;
            c -= 8;
            if c < 0 {
                break;
            }
        }
        self.dif = dif;
        self.cnt = EC_WIN_SIZE as i32 - c - 24;
    }

    fn norm(&mut self, dif: u64, rng: u32) {
        let d = 15 ^ (31 ^ rng.leading_zeros() as i32);
        let cnt = self.cnt;
        self.dif = dif << d;
        self.rng = rng << d;
        self.cnt = cnt - d;
        if (cnt as u32) < (d as u32) {
            self.refill();
        }
    }

    /// Decode one symbol against the adaptive `cdf` (mirrors `Writer::symbol`).
    pub fn symbol(&mut self, cdf: &mut [u16], nsymbs: usize) -> usize {
        let c = (self.dif >> (EC_WIN_SIZE - 16)) as u32;
        let r = self.rng >> 8;
        let mut v: u32 = self.rng;
        let mut u: u32;
        let mut val: usize = usize::MAX; // wraps to 0 on first increment
        loop {
            val = val.wrapping_add(1);
            u = v;
            v = r * (cdf[val] as u32 >> EC_PROB_SHIFT);
            v >>= 7 - EC_PROB_SHIFT;
            v += EC_MIN_PROB * (nsymbs - 1 - val) as u32; // dav1d n_symbols = count−1
            if c >= v {
                break;
            }
        }
        let dif = self.dif - ((v as u64) << (EC_WIN_SIZE - 16));
        self.norm(dif, u - v);
        update_cdf(cdf, val, nsymbs);
        val
    }

    /// Decode a non-adaptive boolean with probability `f`/32768 of being 0.
    pub fn bool(&mut self, f: u16) -> bool {
        let r = self.rng;
        let mut dif = self.dif;
        let mut v: u32 =
            (((r >> 8) * (f as u32 >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)) + EC_MIN_PROB;
        let vw = (v as u64) << (EC_WIN_SIZE - 16);
        let ret = (dif >= vw) as u32;
        dif -= ret as u64 * vw;
        v += ret * (r - 2 * v);
        self.norm(dif, v);
        ret == 0
    }

    /// Adaptive boolean (mirrors [`Writer::bool_adapt`]).
    pub fn bool_adapt(&mut self, cdf: &mut [u16; 2]) -> bool {
        let val = self.bool(cdf[0]);
        let count = cdf[1];
        let rate = 4 + (count >> 4) as u32;
        if val {
            cdf[0] += (CDF_PROB_TOP - cdf[0]) >> rate;
        } else {
            cdf[0] -= cdf[0] >> rate;
        }
        cdf[1] = count + (count < 32) as u16;
        val
    }

    /// Current normalized range (∈ [0x8000, 0xFFFF]) — matches dav1d's `s->rng`,
    /// for diffing per-symbol decode traces against dav1d.
    pub fn rng(&self) -> u32 {
        self.rng
    }

    pub fn bit(&mut self) -> u32 {
        // equiprobable: dav1d decode_bool_equi
        let r = self.rng;
        let mut dif = self.dif;
        let mut v: u32 = ((r >> 8) << 7) + EC_MIN_PROB;
        let vw = (v as u64) << (EC_WIN_SIZE - 16);
        let ret = (dif >= vw) as u32;
        dif -= ret as u64 * vw;
        v += ret * (r - 2 * v);
        self.norm(dif, v);
        (ret == 0) as u32
    }

    pub fn literal(&mut self, bits: u8) -> u32 {
        let mut v = 0u32;
        for _ in 0..bits {
            v = (v << 1) | self.bit();
        }
        v
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — encoder (od_ec) ⇄ decoder (dav1d msac) cross-check + adaptation
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_adaptive_multisymbol() {
        // deterministic pseudo-random symbol stream over mixed alphabet sizes,
        // encoded with the od_ec Writer and decoded with the dav1d msac Reader.
        let mut st = 0xC0FFEEu32;
        let mut rng = || {
            st = st.wrapping_mul(1664525).wrapping_add(1013904223);
            st
        };
        let n = 100_000;
        let mut plan = Vec::with_capacity(n);
        for _ in 0..n {
            let nsymbs = 2 + (rng() >> 28) as usize % 14; // 2..=15
            let r = rng() >> 8;
            let s = if r % 100 < 70 {
                (rng() as usize) % nsymbs.min(3)
            } else {
                (rng() as usize) % nsymbs
            };
            plan.push((nsymbs, s));
        }

        let mut w = Writer::new();
        let mut enc_cdfs: std::collections::HashMap<usize, Vec<u16>> = Default::default();
        for &(nsymbs, s) in &plan {
            let cdf = enc_cdfs.entry(nsymbs).or_insert_with(|| uniform_cdf(nsymbs));
            w.symbol(s, cdf, nsymbs);
        }
        let bytes = w.finish();

        let mut r = Reader::new(&bytes);
        let mut dec_cdfs: std::collections::HashMap<usize, Vec<u16>> = Default::default();
        for (i, &(nsymbs, s)) in plan.iter().enumerate() {
            let cdf = dec_cdfs.entry(nsymbs).or_insert_with(|| uniform_cdf(nsymbs));
            let got = r.symbol(cdf, nsymbs);
            assert_eq!(got, s, "symbol {i} mismatch (nsymbs={nsymbs})");
        }
        assert_eq!(enc_cdfs, dec_cdfs, "CDF adaptation diverged enc vs dec");

        let bits = bytes.len() as f64 * 8.0 / n as f64;
        eprintln!("AV1 od_ec⇄msac: {n} symbols → {} bytes ({bits:.2} bits/sym)", bytes.len());
    }

    #[test]
    fn binary_roundtrip() {
        // non-adaptive bool stream, heavily skewed (90% zeros). The icdf prob
        // f gives P(symbol 0) ∝ (32768 − f); for P(0)≈0.9 use f = 3277.
        const F: u16 = 3277;
        let mut w = Writer::new();
        let mut st = 1u32;
        let mut bits = Vec::new();
        for _ in 0..20_000 {
            st = st.wrapping_mul(1103515245).wrapping_add(12345);
            let b = (st >> 24) % 10 >= 9; // 10% ones (symbol 1), 90% zeros (symbol 0)
            bits.push(b);
            w.bool(b, F);
        }
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(r.bool(F), b, "bool {i}");
        }
        let bps = bytes.len() as f64 * 8.0 / bits.len() as f64;
        eprintln!("AV1 bool coder: 90/10 skew → {bps:.3} bits/sym (entropy ≈ 0.47)");
        assert!(bps < 0.7, "skewed bool stream should compress below 0.7 bits/sym, got {bps}");
    }

    #[test]
    fn literal_roundtrip() {
        // mixed literals (equiprobable bits) round-trip through both coders
        let mut w = Writer::new();
        let vals: Vec<(u8, u32)> = (0..5000u32)
            .map(|i| {
                let bits = (1 + (i % 12)) as u8;
                (bits, (i.wrapping_mul(2654435761)) & ((1u32 << bits) - 1))
            })
            .collect();
        for &(bits, v) in &vals {
            w.literal(bits, v);
        }
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        for (i, &(bits, v)) in vals.iter().enumerate() {
            assert_eq!(r.literal(bits), v, "literal {i} ({bits} bits)");
        }
    }
}
