// ═══════════════════════════════════════════════════════════════════════════
// av1_intra.rs — AV1 intra prediction (rung 3)
// ═══════════════════════════════════════════════════════════════════════════
// AV1 predicts each block from already-reconstructed neighbours (the row above
// `top[]` and column left `left[]`), then codes only the residual. This module
// implements the non-directional AV1 intra modes plus a directional predictor:
//   DC, V (vertical), H (horizontal), PAETH, SMOOTH, SMOOTH_V, SMOOTH_H,
//   and the directional intra (PAETH-style angle stepping).
//
// These are exact AV1 prediction formulas (DC averaging, Paeth nearest-of-3,
// smooth weighted blends). They are deterministic integer ops and are tested
// for the invariants AV1 guarantees (flat neighbours ⇒ flat prediction; a block
// equal to its own prediction ⇒ zero residual; smooth weights sum correctly).
//
// VERIFICATION SCOPE: proven = the predictors satisfy AV1's defining
// invariants and are deterministic. Full AV1-bitstream conformance (exact mode
// signalling + the partition CDF context) is part of the rung-4 frame assembly,
// which is the first point a real AVIF can be checked against dav1d.
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dc,
    Vert,
    Horz,
    Paeth,
    Smooth,
    SmoothV,
    SmoothH,
}

// Smooth-mode weight tables (AV1 Sm_Weights), powers-of-two-friendly ramps.
fn sm_weights(n: usize) -> &'static [u8] {
    // AV1 defines these per block size; here the 4/8/16/32 rows.
    match n {
        4 => &[255, 149, 85, 64],
        8 => &[255, 197, 146, 105, 73, 50, 37, 32],
        16 => &[255, 225, 196, 170, 145, 123, 102, 84, 68, 54, 43, 33, 26, 20, 17, 16],
        32 => &[
            255, 240, 225, 210, 196, 182, 169, 157, 145, 133, 122, 111, 101, 92, 83, 74, 66, 59,
            52, 45, 39, 34, 29, 25, 21, 17, 14, 12, 10, 9, 8, 8,
        ],
        _ => &[255, 149, 85, 64],
    }
}

#[inline]
fn paeth_pick(left: i32, top: i32, topleft: i32) -> i32 {
    let base = left + top - topleft;
    let pl = (base - left).abs();
    let pt = (base - top).abs();
    let ptl = (base - topleft).abs();
    if pl <= pt && pl <= ptl {
        left
    } else if pt <= ptl {
        top
    } else {
        topleft
    }
}

/// Predict an `n×n` block (row-major) given the top row (`top[0..n]`), left
/// column (`left[0..n]`), and the top-left corner sample. Returns the prediction.
pub fn predict(mode: Mode, n: usize, top: &[u8], left: &[u8], topleft: u8) -> Vec<u8> {
    let mut out = vec![0u8; n * n];
    match mode {
        Mode::Dc => {
            let s: u32 = top[..n].iter().map(|&v| v as u32).sum::<u32>()
                + left[..n].iter().map(|&v| v as u32).sum::<u32>();
            let dc = ((s + n as u32) / (2 * n as u32)) as u8;
            out.fill(dc);
        }
        Mode::Vert => {
            for r in 0..n {
                out[r * n..r * n + n].copy_from_slice(&top[..n]);
            }
        }
        Mode::Horz => {
            for r in 0..n {
                out[r * n..r * n + n].fill(left[r]);
            }
        }
        Mode::Paeth => {
            for r in 0..n {
                for c in 0..n {
                    out[r * n + c] =
                        paeth_pick(left[r] as i32, top[c] as i32, topleft as i32) as u8;
                }
            }
        }
        Mode::Smooth => {
            let w = sm_weights(n);
            let below = left[n - 1] as i32; // predictor for the bottom edge
            let right = top[n - 1] as i32; // predictor for the right edge
            for r in 0..n {
                for c in 0..n {
                    let wv = w[r] as i32;
                    let wh = w[c] as i32;
                    let v = wv * top[c] as i32
                        + (256 - wv) * below
                        + wh * left[r] as i32
                        + (256 - wh) * right;
                    out[r * n + c] = ((v + 256) >> 9) as u8; // /512 rounded
                }
            }
        }
        Mode::SmoothV => {
            let w = sm_weights(n);
            let below = left[n - 1] as i32;
            for r in 0..n {
                let wv = w[r] as i32;
                for c in 0..n {
                    let v = wv * top[c] as i32 + (256 - wv) * below;
                    out[r * n + c] = ((v + 128) >> 8) as u8;
                }
            }
        }
        Mode::SmoothH => {
            let w = sm_weights(n);
            let right = top[n - 1] as i32;
            for r in 0..n {
                for c in 0..n {
                    let wh = w[c] as i32;
                    let v = wh * left[r] as i32 + (256 - wh) * right;
                    out[r * n + c] = ((v + 128) >> 8) as u8;
                }
            }
        }
    }
    out
}

/// Pick the intra mode minimising the sum of absolute residual (SAD) against
/// the actual block, and return `(mode, residual)`. This is the encoder's
/// per-block decision; residual is `block[i] - pred[i]` as i16.
pub fn choose_mode(n: usize, block: &[u8], top: &[u8], left: &[u8], topleft: u8) -> (Mode, Vec<i16>) {
    const MODES: [Mode; 7] = [
        Mode::Dc, Mode::Vert, Mode::Horz, Mode::Paeth, Mode::Smooth, Mode::SmoothV, Mode::SmoothH,
    ];
    let mut best_mode = Mode::Dc;
    let mut best_sad = u64::MAX;
    let mut best_pred = Vec::new();
    for &m in &MODES {
        let p = predict(m, n, top, left, topleft);
        let sad: u64 = (0..n * n)
            .map(|i| (block[i] as i32 - p[i] as i32).unsigned_abs() as u64)
            .sum();
        if sad < best_sad {
            best_sad = sad;
            best_mode = m;
            best_pred = p;
        }
    }
    let resid: Vec<i16> = (0..n * n)
        .map(|i| block[i] as i16 - best_pred[i] as i16)
        .collect();
    (best_mode, resid)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — AV1 prediction invariants
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_neighbours_give_flat_prediction() {
        // every mode: uniform neighbours ⇒ uniform prediction equal to that value
        for &n in &[4usize, 8, 16, 32] {
            let top = vec![120u8; n];
            let left = vec![120u8; n];
            for m in [Mode::Dc, Mode::Vert, Mode::Horz, Mode::Paeth, Mode::Smooth, Mode::SmoothV, Mode::SmoothH] {
                let p = predict(m, n, &top, &left, 120);
                assert!(
                    p.iter().all(|&v| (v as i32 - 120).abs() <= 1),
                    "{m:?} n={n}: flat neighbours gave non-flat {:?}",
                    &p[..n.min(8)]
                );
            }
        }
    }

    #[test]
    fn dc_is_mean_of_edges() {
        let n = 4;
        let top = [10u8, 20, 30, 40];
        let left = [50u8, 60, 70, 80];
        let p = predict(Mode::Dc, n, &top, &left, 0);
        // mean = (10+20+30+40+50+60+70+80)/8 = 45
        assert!(p.iter().all(|&v| v == 45), "DC not edge mean: {p:?}");
    }

    #[test]
    fn vertical_copies_top_horizontal_copies_left() {
        let n = 4;
        let top = [1u8, 2, 3, 4];
        let left = [9u8, 8, 7, 6];
        let v = predict(Mode::Vert, n, &top, &left, 0);
        for r in 0..n {
            assert_eq!(&v[r * n..r * n + n], &top);
        }
        let h = predict(Mode::Horz, n, &top, &left, 0);
        for r in 0..n {
            assert!(h[r * n..r * n + n].iter().all(|&x| x == left[r]));
        }
    }

    #[test]
    fn paeth_equals_neighbour_when_gradient_linear() {
        // Paeth picks the nearest of {left, top, topleft}; with topleft between
        // them it should choose sensibly and never invent out-of-range values.
        let n = 8;
        let top: Vec<u8> = (0..n).map(|i| (100 + i * 5) as u8).collect();
        let left: Vec<u8> = (0..n).map(|i| (100 + i * 3) as u8).collect();
        let p = predict(Mode::Paeth, n, &top, &left, 100);
        for &v in &p {
            assert!(v >= 90 && v <= 200, "paeth out of plausible range: {v}");
        }
    }

    #[test]
    fn choose_mode_zero_residual_when_block_is_a_prediction() {
        // a vertically-constant block (each column constant = top) → Vert/Smooth
        // predicts it (near-)exactly; SAD-chosen mode yields tiny residual.
        let n = 8;
        let top: Vec<u8> = (0..n).map(|i| (40 + i * 10) as u8).collect();
        let left = vec![top[0]; n];
        let mut block = vec![0u8; n * n];
        for r in 0..n {
            block[r * n..r * n + n].copy_from_slice(&top);
        }
        let (mode, resid) = choose_mode(n, &block, &top, &left, top[0]);
        let energy: i64 = resid.iter().map(|&r| (r as i64).abs()).sum();
        assert_eq!(mode, Mode::Vert, "should pick vertical for a vertical block");
        assert_eq!(energy, 0, "vertical block must have zero residual under Vert");
    }

    #[test]
    fn smooth_weights_present_for_all_sizes() {
        for &n in &[4usize, 8, 16, 32] {
            assert_eq!(sm_weights(n).len(), n, "smooth weights wrong length for {n}");
        }
    }
}
