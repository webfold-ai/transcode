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
            // Diagonal-Down-Right: needs top, left, top-left
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let v = match (x as i32) - (y as i32) {
                        d if d > 0 => {
                            let i = (x - y - 1) as usize;
                            let a = if i == 0 { tl } else { t[i - 1] };
                            (a + 2 * t[i] + t[i + 1] + 2) >> 2
                        }
                        0 => (t[0] + 2 * tl + l[0] + 2) >> 2,
                        _ => {
                            let i = (y - x - 1) as usize;
                            let a = if i == 0 { tl } else { l[i - 1] };
                            (a + 2 * l[i] + l[i + 1] + 2) >> 2
                        }
                    };
                    set(&mut p, x, y, v);
                }
            }
        }
        5 => {
            // Vertical-Right: needs top, left, top-left
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let z = 2 * (x as i32) - (y as i32);
                    let v = if z >= 0 {
                        if z % 2 == 0 {
                            let i = x - y / 2;
                            let a = if i == 0 { tl } else { t[i - 1] };
                            (a + t[i] + 1) >> 1
                        } else {
                            let i = x - y / 2;
                            let a = if i < 2 { if i == 1 { tl } else { t[0] } } else { t[i - 2] };
                            let b = if i == 0 { tl } else { t[i - 1] };
                            // careful indices per spec
                            let aa = if i == 1 { tl } else { t[i - 2] };
                            (aa + 2 * b + t[i] + 2) >> 2
                        }
                    } else if z == -1 {
                        (l[0] + 2 * tl + t[0] + 2) >> 2
                    } else {
                        let i = y - 2 * x;
                        let a = if i >= 2 { l[i - 2] } else { tl };
                        (a + 2 * l[i - 1] + l[i] + 2) >> 2
                    };
                    set(&mut p, x, y, v);
                }
            }
        }
        6 => {
            // Horizontal-Down: needs top, left, top-left
            if !(n.avail_l && n.avail_t) {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let z = 2 * (y as i32) - (x as i32);
                    let v = if z >= 0 {
                        if z % 2 == 0 {
                            let i = y - x / 2;
                            let a = if i == 0 { tl } else { l[i - 1] };
                            (a + l[i] + 1) >> 1
                        } else {
                            let i = y - x / 2;
                            let aa = if i == 1 { tl } else { l[i - 2] };
                            let b = if i == 0 { tl } else { l[i - 1] };
                            (aa + 2 * b + l[i] + 2) >> 2
                        }
                    } else if z == -1 {
                        (t[0] + 2 * tl + l[0] + 2) >> 2
                    } else {
                        let i = x - 2 * y;
                        let a = if i >= 2 { t[i - 2] } else { tl };
                        (a + 2 * t[i - 1] + t[i] + 2) >> 2
                    };
                    set(&mut p, x, y, v);
                }
            }
        }
        7 => {
            // Vertical-Left: needs top (and top-right)
            if !n.avail_t {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let i = x + y / 2;
                    let v = if y % 2 == 0 {
                        (t[i] + t[i + 1] + 1) >> 1
                    } else {
                        (t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2
                    };
                    set(&mut p, x, y, v);
                }
            }
        }
        8 => {
            // Horizontal-Up: needs left
            if !n.avail_l {
                return None;
            }
            for y in 0..4 {
                for x in 0..4 {
                    let z = x + 2 * y;
                    let v = if z < 5 {
                        if z % 2 == 0 {
                            (l[y + x / 2] + l[y + x / 2 + 1] + 1) >> 1
                        } else {
                            let i = y + x / 2;
                            (l[i] + 2 * l[i + 1] + l[i + 2] + 2) >> 2
                        }
                    } else if z == 5 {
                        (l[2] + 3 * l[3] + 2) >> 2
                    } else {
                        l[3]
                    };
                    set(&mut p, x, y, v);
                }
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
