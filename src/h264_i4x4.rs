// ═══════════════════════════════════════════════════════════════════════════
// h264_i4x4.rs — H.264 Intra_4x4 luma prediction (the 9 modes, §8.3.1)
// ═══════════════════════════════════════════════════════════════════════════
// I_4x4 macroblocks predict each 4x4 luma block from its already-reconstructed
// neighbours using one of 9 modes, then code the residual. This module provides
// the 9-mode predictor and the encode-side helpers (most-probable-mode, the
// coded_block_pattern golomb mapping). The per-block reconstruction-feedback
// loop and CAVLC emission are driven from h264_intra.rs.
//
// Modes (§8.3.1.2): 0=Vertical 1=Horizontal 2=DC 3=Diagonal-Down-Left
//   4=Diagonal-Down-Right 5=Vertical-Right 6=Horizontal-Down 7=Vertical-Left
//   8=Horizontal-Up. Availability of top/left/top-right/top-left neighbours
//   gates which modes are usable; DC (2) is always available.
//
// VERIFICATION: as with the rest of the H.264 work, correctness is judged by
// ffmpeg decoding the produced stream bit-exact against the encoder's own
// reconstruction. The predictor formulas are the exact spec rounding.
// ═══════════════════════════════════════════════════════════════════════════

/// Neighbour samples for one 4x4 block prediction. `top[0..8]` = the 8 samples
/// above (top-right beyond x+3 may be unavailable → caller fills), `left[0..4]`
/// the 4 to the left, `tl` the top-left corner. Availability flags gate modes.
pub struct Neighbours {
    pub left: [i32; 4],
    pub top: [i32; 8], // top[0..4] above the block, top[4..8] = top-right
    pub tl: i32,
    pub avail_l: bool,
    pub avail_t: bool,
    pub avail_tr: bool, // top-right available (else top[4..8] replicate top[3])
}

/// Predict a 4x4 block under `mode`, returning row-major 16 samples, or None if
/// the mode's required neighbours are unavailable.
pub fn predict4(mode: u8, n: &Neighbours) -> Option<[i32; 16]> {
    let l = &n.left;
    let t = &n.top;
    let tl = n.tl;
    let mut p = [0i32; 16];
    let set = |p: &mut [i32; 16], x: usize, y: usize, v: i32| p[y * 4 + x] = v;
    match mode {
        0 => {
            // Vertical: needs top
            if !n.avail_t {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    set(&mut p, x, y, t[x]);
                }
            }
        }
        1 => {
            // Horizontal: needs left
            if !n.avail_l {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    set(&mut p, x, y, l[y]);
                }
            }
        }
        2 => {
            // DC
            let v = match (n.avail_l, n.avail_t) {
                (true, true) => (l[0] + l[1] + l[2] + l[3] + t[0] + t[1] + t[2] + t[3] + 4) >> 3,
                (true, false) => (l[0] + l[1] + l[2] + l[3] + 2) >> 2,
                (false, true) => (t[0] + t[1] + t[2] + t[3] + 2) >> 2,
                (false, false) => 128,
            };
            p.fill(v);
        }
        3 => {
            // Diagonal-Down-Left: needs top (and top-right; if unavailable,
            // top[4..8] are replicated from top[3] by the caller)
            if !n.avail_t {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let v = if x == 3 && y == 3 {
                        (t[6] + 3 * t[7] + 2) >> 2
                    } else {
                        let i = x + y;
                        (t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2
                    };
                    set(&mut p, x, y, v);
                }
            }
        }
        4 => {
            // Diagonal-Down-Right (ffmpeg pred4x4_down_right, verbatim).
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            set(&mut p, 0, 3, (l3 + 2 * l2 + l1 + 2) >> 2);
            let a = (l2 + 2 * l1 + l0 + 2) >> 2;
            set(&mut p, 0, 2, a); set(&mut p, 1, 3, a);
            let b = (l1 + 2 * l0 + tl + 2) >> 2;
            set(&mut p, 0, 1, b); set(&mut p, 1, 2, b); set(&mut p, 2, 3, b);
            let c = (l0 + 2 * tl + t0 + 2) >> 2;
            set(&mut p, 0, 0, c); set(&mut p, 1, 1, c); set(&mut p, 2, 2, c); set(&mut p, 3, 3, c);
            let d = (tl + 2 * t0 + t1 + 2) >> 2;
            set(&mut p, 1, 0, d); set(&mut p, 2, 1, d); set(&mut p, 3, 2, d);
            let e = (t0 + 2 * t1 + t2 + 2) >> 2;
            set(&mut p, 2, 0, e); set(&mut p, 3, 1, e);
            set(&mut p, 3, 0, (t1 + 2 * t2 + t3 + 2) >> 2);
        }
        5 => {
            // Vertical-Right (ffmpeg pred4x4_vertical_right, verbatim).
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (l0, l1, l2) = (l[0], l[1], l[2]);
            let a = (tl + t0 + 1) >> 1;
            set(&mut p, 0, 0, a); set(&mut p, 1, 2, a);
            let b = (t0 + t1 + 1) >> 1;
            set(&mut p, 1, 0, b); set(&mut p, 2, 2, b);
            let c = (t1 + t2 + 1) >> 1;
            set(&mut p, 2, 0, c); set(&mut p, 3, 2, c);
            set(&mut p, 3, 0, (t2 + t3 + 1) >> 1);
            let d = (l0 + 2 * tl + t0 + 2) >> 2;
            set(&mut p, 0, 1, d); set(&mut p, 1, 3, d);
            let e = (tl + 2 * t0 + t1 + 2) >> 2;
            set(&mut p, 1, 1, e); set(&mut p, 2, 3, e);
            let f = (t0 + 2 * t1 + t2 + 2) >> 2;
            set(&mut p, 2, 1, f); set(&mut p, 3, 3, f);
            set(&mut p, 3, 1, (t1 + 2 * t2 + t3 + 2) >> 2);
            set(&mut p, 0, 2, (tl + 2 * l0 + l1 + 2) >> 2);
            set(&mut p, 0, 3, (l0 + 2 * l1 + l2 + 2) >> 2);
        }
        6 => {
            // Horizontal-Down (ffmpeg pred4x4_horizontal_down, verbatim).
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            let (t0, t1, t2) = (t[0], t[1], t[2]);
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            let a = (tl + l0 + 1) >> 1;
            set(&mut p, 0, 0, a); set(&mut p, 2, 1, a);
            let b = (l0 + 2 * tl + t0 + 2) >> 2;
            set(&mut p, 1, 0, b); set(&mut p, 3, 1, b);
            set(&mut p, 2, 0, (tl + 2 * t0 + t1 + 2) >> 2);
            set(&mut p, 3, 0, (t0 + 2 * t1 + t2 + 2) >> 2);
            let c = (l0 + l1 + 1) >> 1;
            set(&mut p, 0, 1, c); set(&mut p, 2, 2, c);
            let d = (tl + 2 * l0 + l1 + 2) >> 2;
            set(&mut p, 1, 1, d); set(&mut p, 3, 2, d);
            let e = (l1 + l2 + 1) >> 1;
            set(&mut p, 0, 2, e); set(&mut p, 2, 3, e);
            let f = (l0 + 2 * l1 + l2 + 2) >> 2;
            set(&mut p, 1, 2, f); set(&mut p, 3, 3, f);
            set(&mut p, 0, 3, (l2 + l3 + 1) >> 1);
            set(&mut p, 1, 3, (l1 + 2 * l2 + l3 + 2) >> 2);
        }
        7 => {
            // Vertical-Left (ffmpeg pred4x4_vertical_left, verbatim). t4..t6 top-right.
            if !n.avail_t {
                return None;
            }
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (t4, t5, t6) = (t[4], t[5], t[6]);
            set(&mut p, 0, 0, (t0 + t1 + 1) >> 1);
            let a = (t1 + t2 + 1) >> 1;
            set(&mut p, 1, 0, a); set(&mut p, 0, 2, a);
            let b = (t2 + t3 + 1) >> 1;
            set(&mut p, 2, 0, b); set(&mut p, 1, 2, b);
            let c = (t3 + t4 + 1) >> 1;
            set(&mut p, 3, 0, c); set(&mut p, 2, 2, c);
            set(&mut p, 3, 2, (t4 + t5 + 1) >> 1);
            set(&mut p, 0, 1, (t0 + 2 * t1 + t2 + 2) >> 2);
            let d = (t1 + 2 * t2 + t3 + 2) >> 2;
            set(&mut p, 1, 1, d); set(&mut p, 0, 3, d);
            let e = (t2 + 2 * t3 + t4 + 2) >> 2;
            set(&mut p, 2, 1, e); set(&mut p, 1, 3, e);
            let f = (t3 + 2 * t4 + t5 + 2) >> 2;
            set(&mut p, 3, 1, f); set(&mut p, 2, 3, f);
            set(&mut p, 3, 3, (t4 + 2 * t5 + t6 + 2) >> 2);
        }
        8 => {
            // Horizontal-Up (ffmpeg pred4x4_horizontal_up, verbatim).
            if !n.avail_l {
                return None;
            }
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            set(&mut p, 0, 0, (l0 + l1 + 1) >> 1);
            set(&mut p, 1, 0, (l0 + 2 * l1 + l2 + 2) >> 2);
            let a = (l1 + l2 + 1) >> 1;
            set(&mut p, 2, 0, a); set(&mut p, 0, 1, a);
            let b = (l1 + 2 * l2 + l3 + 2) >> 2;
            set(&mut p, 3, 0, b); set(&mut p, 1, 1, b);
            let c = (l2 + l3 + 1) >> 1;
            set(&mut p, 2, 1, c); set(&mut p, 0, 2, c);
            let d = (l2 + 2 * l3 + l3 + 2) >> 2;
            set(&mut p, 3, 1, d); set(&mut p, 1, 2, d);
            for &(x, y) in &[(3, 2), (1, 3), (0, 3), (2, 2), (2, 3), (3, 3)] {
                set(&mut p, x, y, l3);
            }
        }
        _ => return None,
    }
    for v in p.iter_mut() {
        *v = (*v).clamp(0, 255);
    }
    Some(p)
}

// coded_block_pattern golomb mapping for Intra_4x4 (FFmpeg h264data.c,
// ff_h264_golomb_to_intra4x4_cbp, first 48 entries — intra uses index 0..47).
// This is golomb→CBP (decoder); the encoder needs CBP→golomb (the inverse).
const GOLOMB_TO_INTRA4X4_CBP: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

/// CBP → ue(v) golomb code value for an Intra_4x4 macroblock (inverse of the
/// spec table). `cbp` ∈ 0..48 (4 luma 8x8 bits + chroma 0/1/2 in high bits).
pub fn intra4x4_cbp_to_golomb(cbp: u8) -> u32 {
    for (g, &c) in GOLOMB_TO_INTRA4X4_CBP.iter().enumerate() {
        if c == cbp {
            return g as u32;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb(left: [i32; 4], top: [i32; 8], tl: i32) -> Neighbours {
        Neighbours { left, top, tl, avail_l: true, avail_t: true, avail_tr: true }
    }

    #[test]
    fn vertical_copies_top() {
        let p = predict4(0, &nb([9, 9, 9, 9], [1, 2, 3, 4, 4, 4, 4, 4], 0)).unwrap();
        for y in 0..4 {
            assert_eq!(&p[y * 4..y * 4 + 4], &[1, 2, 3, 4]);
        }
    }

    #[test]
    fn horizontal_copies_left() {
        let p = predict4(1, &nb([5, 6, 7, 8], [0; 8], 0)).unwrap();
        for y in 0..4 {
            assert!(p[y * 4..y * 4 + 4].iter().all(|&v| v == [5, 6, 7, 8][y]));
        }
    }

    #[test]
    fn dc_is_mean() {
        // left+top all 100 → DC 100
        let p = predict4(2, &nb([100; 4], [100; 8], 100)).unwrap();
        assert!(p.iter().all(|&v| v == 100));
    }

    #[test]
    fn flat_neighbours_flat_for_all_modes() {
        // uniform neighbours ⇒ every mode predicts that uniform value
        for m in 0..9u8 {
            let p = predict4(m, &nb([77; 4], [77; 8], 77)).unwrap();
            assert!(
                p.iter().all(|&v| (v - 77).abs() <= 1),
                "mode {m} not flat on flat input: {:?}",
                &p[..4]
            );
        }
    }

    #[test]
    fn cbp_golomb_roundtrips() {
        // every CBP value present in the table maps back to a golomb code whose
        // table entry returns that CBP.
        for &cbp in GOLOMB_TO_INTRA4X4_CBP.iter() {
            let g = intra4x4_cbp_to_golomb(cbp);
            assert_eq!(GOLOMB_TO_INTRA4X4_CBP[g as usize], cbp);
        }
    }

    #[test]
    fn dc_availability() {
        // DC works with no neighbours (→128); directional modes return None.
        let none = Neighbours { left: [0; 4], top: [0; 8], tl: 0, avail_l: false, avail_t: false, avail_tr: false };
        assert!(predict4(2, &none).is_some());
        assert_eq!(predict4(2, &none).unwrap()[0], 128);
        assert!(predict4(0, &none).is_none());
        assert!(predict4(1, &none).is_none());
    }
}

// ── I_4x4 luma encode: per-block predict→transform→quant→reconstruct ─────────
use crate::h264_intra::{fwd_core, inv_core, pos_class, MF, V};

/// Top-right availability for each luma 4×4 block (scan order), as a gate over
/// the macroblock-level neighbour flags. The 4×4 decode scan visits the 16
/// blocks Z-ordered; the top-right of a block is the unit diagonally up-right.
/// For blocks 0/1/4 it lies in the MB above (gated by `avail_t`); for block 5
/// in the above-right MB (gated by `avail_tr`); for 3/7/11/13/15 it is not yet
/// decoded (replicate top[3]); the rest are interior and always available.
/// (Derived from §6.4.11.4 + the Inverse4x4LumaBlockScan, §8.3.1.)
#[derive(Clone, Copy)]
enum TrGate { Top, TopRight, Always, Never }
const TR_GATE: [TrGate; 16] = {
    use TrGate::*;
    // blk:   0     1     2       3      4     5         6       7      8       9       10      11     12      13     14      15
    [Top, Top, Always, Never, Top, TopRight, Always, Never, Always, Always, Always, Never, Always, Never, Always, Never]
};

/// Gather the neighbour samples for the 4×4 block `blk` (scan index) at MB-pixel
/// origin (`px`,`py`), respecting MB-edge availability. Unavailable top-right
/// samples are replicated from top[3], matching the H.264 reference decoders.
#[allow(clippy::too_many_arguments)]
fn gather(
    rec: &[u8], pw: usize, px: usize, py: usize, ux: usize, uy: usize, blk: usize,
    avail_l_mb: bool, avail_t_mb: bool, avail_tr_mb: bool,
) -> Neighbours {
    let avail_l = ux > 0 || avail_l_mb;
    let avail_t = uy > 0 || avail_t_mb;
    let avail_tr = match TR_GATE[blk] {
        TrGate::Top => avail_t_mb,
        TrGate::TopRight => avail_tr_mb,
        TrGate::Always => true,
        TrGate::Never => false,
    };
    let mut left = [0i32; 4];
    let mut top = [0i32; 8];
    let mut tl = 0i32;
    if avail_l {
        for i in 0..4 {
            left[i] = rec[(py + i) * pw + (px - 1)] as i32;
        }
    }
    if avail_t {
        for i in 0..4 {
            top[i] = rec[(py - 1) * pw + (px + i)] as i32;
        }
        for i in 0..4 {
            let tx = px + 4 + i;
            top[4 + i] = if avail_tr && tx < pw { rec[(py - 1) * pw + tx] as i32 } else { top[3] };
        }
    }
    if avail_l && avail_t {
        tl = rec[(py - 1) * pw + (px - 1)] as i32;
    }
    Neighbours { left, top, tl, avail_l, avail_t, avail_tr }
}

pub struct I4x4Luma {
    pub coeffs: [[i32; 16]; 16],
    pub modes: [u8; 16],
    pub any_nonzero: bool,
}

/// Encode the 16 luma 4×4 blocks of one I_4x4 macroblock: for each block in
/// decode-scan order, predict from already-reconstructed neighbours (SAD-best
/// of the 9 modes), forward-transform+quantise the residual (all 16 coeffs —
/// I_4x4 has no separate luma-DC block, unlike I_16x16), then dequant+inverse
/// and write the reconstruction straight back into `rec` so the next block sees
/// the true decoded neighbours. The dequant matches the ffmpeg-validated AC
/// path in h264_intra.rs exactly.
#[allow(clippy::too_many_arguments)]
pub fn encode_luma_i4x4(
    src: &[u8], rec: &mut [u8], pw: usize, _ph: usize,
    mbx: usize, mby: usize, qp: i32, blk_xy: &[(usize, usize); 16],
    avail_l_mb: bool, avail_t_mb: bool, avail_tr_mb: bool,
) -> I4x4Luma {
    let qbits = 15 + qp / 6;
    let shift = qp / 6;
    let rem = (qp % 6) as usize;
    let f = (1i64 << qbits) / 3;
    let mut out = I4x4Luma { coeffs: [[0; 16]; 16], modes: [2; 16], any_nonzero: false };
    let mb_x0 = mbx * 16;
    for blk in 0..16 {
        let (bx, by) = blk_xy[blk];
        let px = mb_x0 + bx;
        let py = mby * 16 + by;
        let n = gather(rec, pw, px, py, bx / 4, by / 4, blk, avail_l_mb, avail_t_mb, avail_tr_mb);
        let mut best_mode = 2u8;
        let mut best_pred = [0i32; 16];
        let mut best_sad = u64::MAX;
        for m in 0..9u8 {
            if let Some(pp) = predict4(m, &n) {
                let mut sad = 0u64;
                for yy in 0..4 {
                    for xx in 0..4 {
                        let s = src[(py + yy) * pw + (px + xx)] as i32;
                        sad += (s - pp[yy * 4 + xx]).unsigned_abs() as u64;
                    }
                }
                if sad < best_sad { best_sad = sad; best_mode = m; best_pred = pp; }
            }
        }
        let mut resid = [0i32; 16];
        for yy in 0..4 {
            for xx in 0..4 {
                resid[yy * 4 + xx] = src[(py + yy) * pw + (px + xx)] as i32 - best_pred[yy * 4 + xx];
            }
        }
        let wq = fwd_core(&resid);
        let mut z = [0i32; 16];
        let mut deq = [0i32; 16];
        for i in 0..16 {
            let cl = pos_class(i);
            let mf = MF[rem][cl] as i64;
            let zz = ((wq[i].abs() as i64 * mf + f) >> qbits) as i32;
            z[i] = if wq[i] < 0 { -zz } else { zz };
            if z[i] != 0 { out.any_nonzero = true; }
            let vv = V[rem][cl];
            deq[i] = if shift >= 4 {
                (z[i] * vv) << shift
            } else {
                (z[i] * vv * 16 + (1 << (3 - shift))) >> (4 - shift)
            };
        }
        let sp = inv_core(&deq);
        for yy in 0..4 {
            for xx in 0..4 {
                rec[(py + yy) * pw + (px + xx)] = (best_pred[yy * 4 + xx] + sp[yy * 4 + xx]).clamp(0, 255) as u8;
            }
        }
        out.coeffs[blk] = z;
        out.modes[blk] = best_mode;
    }
    out
}
