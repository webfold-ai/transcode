// ═══════════════════════════════════════════════════════════════════════════
// av1_coef.rs — AV1 2D-class coefficient SYNTAX, written ONCE (arm ③).
//
// Per THREE_HASHMAPS.md / applicative_is_the_only_freedom (vocalize arch.rs
// Backend, SANGUINE-PATH-REVIEW): the coefficient bitstream is one declaration,
// `code_block_2d`, generic over a [`Coder`]. `Enc` writes, `Dec` reads — the two
// applicatives are the only freedom. Contexts depend only on already-coded
// values (the `levels` stencil, the eob), which both sides hold, so the encoder
// is the decoder run backward — they cannot diverge (R(θ)⁻¹ = R(−θ); `/` its own
// inverse). The CDFs are a persistent ① Coord context ([`CoefCdf`]) adapted in
// place. Scoped to luma TX_64X64 (the 32×32 coeff region, 2D/DCT class).
//
// PROOF CHAIN: round-trip (Enc∘Dec = id, tests here) ∧ Reader ≡ dav1d (the
// av1_msac_trace test) ⟹ the encoder is dav1d-conformant for free.
// ═══════════════════════════════════════════════════════════════════════════

use crate::av1_coef_tables::*;
use crate::av1_ec::{Coder, Dec, Enc, Reader, Writer};

fn icdf(args: &[u16]) -> Vec<u16> {
    let mut v: Vec<u16> = args.iter().map(|&a| 32768 - a).collect();
    v.push(0);
    v.push(0);
    v
}
fn bcdf(arg: u16) -> [u16; 2] {
    [32768 - arg, 0]
}

const EOB_BIN_1024_L: [u16; 10] =
    [6698, 8334, 11961, 15762, 20186, 23862, 27434, 29326, 31082, 32050];
// EOB_HI_BIT_64 (= eob_hi_bit[4][0]) comes from av1_coef_tables (the correct
// TX_64X64 context; an earlier hardcode used [0][0] and desynced — caught by
// the dav1d symbol trace).

/// Persistent coefficient CDF context (① Coord), adapted in place across a tile.
pub struct CoefCdf {
    pub txb_skip: [u16; 2],
    pub eob_bin: Vec<u16>,
    pub eob_hi_bit: [[u16; 2]; 9],
    pub eob_base_tok: [Vec<u16>; 4],
    pub base_tok: Vec<Vec<u16>>,
    pub br_tok: Vec<Vec<u16>>,
    pub dc_sign: [u16; 2],
}
impl Default for CoefCdf {
    fn default() -> Self {
        Self::new()
    }
}
impl CoefCdf {
    pub fn new() -> Self {
        CoefCdf {
            txb_skip: bcdf(31539),
            eob_bin: icdf(&EOB_BIN_1024_L),
            eob_hi_bit: core::array::from_fn(|i| bcdf(EOB_HI_BIT_64[i])),
            eob_base_tok: core::array::from_fn(|i| icdf(&EOB_BASE_TOK_64[i])),
            base_tok: BASE_TOK_64.iter().map(|a| icdf(a)).collect(),
            br_tok: BR_TOK_64.iter().map(|a| icdf(a)).collect(),
            dc_sign: bcdf(16000),
        }
    }
}

const STRIDE: usize = 32;
const SHIFT: usize = 5;
const MASK: usize = 31;

fn lo_ctx_2d(levels: &[u8], rc: usize, x: usize, y: usize) -> (usize, u32) {
    let l = |o: usize| levels[rc + o] as u32;
    let mut mag = l(1) + l(STRIDE);
    mag += l(STRIDE + 1);
    let hi_mag = mag;
    mag += l(2) + l(2 * STRIDE);
    let offset = LO_CTX_OFFSETS_2D[y.min(4)][x.min(4)] as usize;
    (offset + if mag > 512 { 4 } else { ((mag + 64) >> 7) as usize }, hi_mag)
}

fn level_byte(tok: u32, is_hi: bool) -> u8 {
    if is_hi { (tok + 0xc0) as u8 } else { (tok.wrapping_mul(0x41)) as u8 }
}

/// hi_tok (br_tok): base-3 with 3=continue. `hint` = full |level| on encode.
/// Returns the token ∈ 3..=15. Symmetric (Enc emits hint's tokens, Dec decodes).
fn code_hi_tok<C: Coder>(c: &mut C, br: &mut [u16], hint: u32) -> u32 {
    let h = hint.min(15);
    let t1 = c.symbol(br, 4, (h - 3).min(3) as usize) as u32;
    let mut tok = 3 + t1;
    if t1 == 3 {
        let t2 = c.symbol(br, 4, (h - 6).min(3) as usize) as u32;
        tok = 6 + t2;
        if t2 == 3 {
            let t3 = c.symbol(br, 4, (h - 9).min(3) as usize) as u32;
            tok = 9 + t3;
            if t3 == 3 {
                let t4 = c.symbol(br, 4, (h - 12).min(3) as usize) as u32;
                tok = 12 + t4;
            }
        }
    }
    tok
}

/// Exp-Golomb (inverse of read_golomb). `hint` = x on encode; returns x.
fn code_golomb<C: Coder>(c: &mut C, hint: u32) -> u32 {
    let val_hint = hint + 1;
    let len_enc = (31 - val_hint.leading_zeros()) as usize;
    // unary length: zeros until a 1 (the run terminator)
    let mut len = 0usize;
    loop {
        let stop = if len < len_enc { 0 } else { 1 };
        if c.bit(stop) == 1 {
            break;
        }
        len += 1;
        if len >= 32 {
            break;
        }
    }
    // the len value bits below the leading 1 (MSB-first)
    let mut val = 1u32;
    for k in (0..len).rev() {
        val = (val << 1) | c.bit((val_hint >> k) & 1);
    }
    val - 1
}

fn eob_pt_of(eob: usize) -> usize {
    if eob <= 1 {
        eob
    } else {
        (32 - (eob as u32).leading_zeros()) as usize
    }
}

/// Code one luma TX_64X64 block's quantised coefficients (raster, stride 32; the
/// top-left 32×32 may be nonzero), signed quantised levels. Written ONCE; `Enc`
/// reads `coeffs` and emits, `Dec` (caller zeroes `coeffs` first) writes them.
/// Returns false (all_zero) if the block is empty. Mirrors decode_coefs 2D.
fn code_block<C: Coder>(c: &mut C, cdf: &mut CoefCdf, coeffs: &mut [i32]) -> bool {
    let empty = coeffs[..1024].iter().all(|&x| x == 0);
    let all_zero = c.bool_adapt(&mut cdf.txb_skip, empty);
    if all_zero {
        return false; // Dec: caller pre-zeroed; Enc: was empty
    }

    // eob (last nonzero scan index) — encode hint only
    let mut eob_enc = 0usize;
    for (i, &rc) in SCAN_32X32.iter().enumerate() {
        if coeffs[rc as usize] != 0 {
            eob_enc = i;
        }
    }
    let eob_pt = c.symbol(&mut cdf.eob_bin, 11, eob_pt_of(eob_enc));
    let eob = if eob_pt <= 1 {
        eob_pt
    } else {
        let eob_bin = eob_pt - 2;
        let hi = c.bool_adapt(&mut cdf.eob_hi_bit[eob_bin], (eob_enc >> eob_bin) & 1 == 1) as usize;
        let mut e = (hi | 2) << eob_bin;
        for b in (0..eob_bin).rev() {
            e |= (c.bit(((eob_enc >> b) & 1) as u32) as usize) << b;
        }
        e
    };

    let mut levels = vec![0u8; STRIDE * (STRIDE + 2)];
    let mut tok_at = vec![0u32; 1024]; // capped token (≤15) per rc, for the sign pass

    if eob == 0 {
        // dc-only: DC via eob_base_tok[0]
        let h = coeffs[0].unsigned_abs();
        let et = c.symbol(&mut cdf.eob_base_tok[0], 3, h.min(3).saturating_sub(1) as usize);
        let mut tok = et as u32 + 1;
        if et == 2 {
            tok = code_hi_tok(c, &mut cdf.br_tok[0], h);
        }
        let sign = c.bool_adapt(&mut cdf.dc_sign, coeffs[0] < 0);
        let full = if tok == 15 { 15 + code_golomb(c, h.saturating_sub(15)) } else { tok };
        coeffs[0] = if sign { -(full as i32) } else { full as i32 };
        return true;
    }

    // eob coeff (scan[eob]) via eob_base_tok[eob_ctx]
    let rc_e = SCAN_32X32[eob] as usize;
    let (xe, ye) = (rc_e >> SHIFT, rc_e & MASK);
    let eob_ctx = 1 + (eob > (2 << 6)) as usize + (eob > (4 << 6)) as usize;
    let he = coeffs[rc_e].unsigned_abs();
    let et = c.symbol(&mut cdf.eob_base_tok[eob_ctx], 3, he.min(3).saturating_sub(1) as usize);
    let tok_e = if et == 2 {
        let hctx = if (xe | ye) > 1 { 14 } else { 7 };
        let t = code_hi_tok(c, &mut cdf.br_tok[hctx], he);
        levels[rc_e] = level_byte(t, true);
        t
    } else {
        let t = et as u32 + 1;
        levels[rc_e] = level_byte(t, false);
        t
    };
    tok_at[rc_e] = tok_e;

    // middle AC, i = eob-1 .. 1
    for i in (1..eob).rev() {
        let rc = SCAN_32X32[i] as usize;
        let (x, y) = (rc >> SHIFT, rc & MASK);
        let (ctx, _) = lo_ctx_2d(&levels, rc, x, y);
        let h = coeffs[rc].unsigned_abs();
        let base = c.symbol(&mut cdf.base_tok[ctx], 4, h.min(3) as usize) as u32;
        let tok = if base == 3 {
            let (_, hi_mag) = lo_ctx_2d(&levels, rc, x, y);
            let mag = hi_mag & 63;
            let yy = (y | x) as u32;
            let hctx = (if yy > 1 { 14 } else { 7 }) + if mag > 12 { 6 } else { ((mag + 1) >> 1) as usize };
            let t = code_hi_tok(c, &mut cdf.br_tok[hctx], h);
            levels[rc] = level_byte(t, true);
            t
        } else {
            levels[rc] = level_byte(base, false);
            base
        };
        tok_at[rc] = tok;
    }

    // DC (scan[0]) — 2D class fixes ctx 0
    let h0 = coeffs[0].unsigned_abs();
    let base = c.symbol(&mut cdf.base_tok[0], 4, h0.min(3) as usize) as u32;
    let tok0 = if base == 3 {
        let mag = (levels[STRIDE] as u32 + levels[1] as u32 + levels[STRIDE + 1] as u32) & 63;
        let hctx = if mag > 12 { 6 } else { ((mag + 1) >> 1) as usize };
        code_hi_tok(c, &mut cdf.br_tok[hctx], h0)
    } else {
        base
    };
    tok_at[0] = tok0;

    // ── signs + golomb: dc_sign (+dc golomb), then AC signs (increasing scan) ──
    let dsign = c.bool_adapt(&mut cdf.dc_sign, coeffs[0] < 0);
    let dc_full = if tok0 == 15 { 15 + code_golomb(c, h0.saturating_sub(15)) } else { tok0 };
    coeffs[0] = if dsign { -(dc_full as i32) } else { dc_full as i32 };
    for i in 1..=eob {
        let rc = SCAN_32X32[i] as usize;
        let t = tok_at[rc];
        if t != 0 {
            let sign = c.bit((coeffs[rc] < 0) as u32) == 1;
            let full = if t == 15 { 15 + code_golomb(c, coeffs[rc].unsigned_abs().saturating_sub(15)) } else { t };
            coeffs[rc] = if sign { -(full as i32) } else { full as i32 };
        }
    }
    true
}

/// Encode a luma TX_64X64 block (the Enc applicative).
pub fn encode_block_2d(w: &mut Writer, cdf: &mut CoefCdf, coeffs: &[i32]) -> bool {
    let mut tmp = coeffs.to_vec();
    code_block(&mut Enc(w), cdf, &mut tmp)
}

/// Decode a luma TX_64X64 block (the Dec applicative). `coeffs` is written
/// (1024 entries, raster); caller passes a zeroed slice.
pub fn decode_block_2d(r: &mut Reader, cdf: &mut CoefCdf, coeffs: &mut [i32]) -> bool {
    for v in coeffs[..1024].iter_mut() {
        *v = 0;
    }
    code_block(&mut Dec(r), cdf, coeffs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1_ec::{Reader, Writer};

    /// Enc ∘ Dec = identity over arbitrary quantised coefficient blocks — the
    /// proof that encode and decode are the one grammar. (Pairs with
    /// av1_msac_trace's Reader≡dav1d to give encoder conformance.)
    #[test]
    fn coeff_block_roundtrips() {
        let cases: &[&[(usize, i32)]] = &[
            &[],                                    // empty (all_zero)
            &[(0, 1)],                              // DC only, +1
            &[(0, -7)],                             // DC only, -7 (hi_tok)
            &[(0, 2), (32, 1), (1, 3)],             // DC + 2 AC (scan 0,1,2)
            &[(0, 40)],                             // DC golomb (level>15)
            &[(0, -3), (32, -20), (1, 2), (2, 9)],  // mixed signs + golomb AC
            &[(0, 1), (1, 1), (2, 1), (33, 1), (64, 1), (96, 1)], // a run of AC
        ];
        for (ci, case) in cases.iter().enumerate() {
            let mut coeffs = vec![0i32; 1024];
            for &(rc, v) in *case {
                coeffs[rc] = v;
            }
            let mut enc_cdf = CoefCdf::new();
            let mut w = Writer::new();
            encode_block_2d(&mut w, &mut enc_cdf, &coeffs);
            let bytes = w.finish();

            let mut dec_cdf = CoefCdf::new();
            let mut r = Reader::new(&bytes);
            let mut out = vec![0i32; 1024];
            decode_block_2d(&mut r, &mut dec_cdf, &mut out);
            assert_eq!(out, coeffs, "case {ci} round-trip mismatch");
            assert_eq!(enc_cdf.base_tok, dec_cdf.base_tok, "case {ci} CDF adaptation diverged");
        }
    }
}
