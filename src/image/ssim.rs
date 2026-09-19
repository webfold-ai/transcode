// transcode/src/image/ssim.rs — Structural similarity on RGBA8 image pairs.
//
// SSIM compares two luminance grids over an 8x8 sliding window (Wiener 2004).
// This module is the visual regression primitive that mirrors upstream
// `pixelhog`'s `Comparison::ssim()` method, but on the fold substrate:
//   * operates on `crate::Image` (already in the fold tree),
//   * single-threaded SIMD-friendly inner loop,
//   * zero dependencies beyond the fold substrate.
//
// Quality column: SSIM ∈ [0, 1] (1 = identical). Speed column: pixels/sec.
// Baseline: upstream Python `pixelhog` on the Mapbox canonical pixelmatch
// fixtures (250x250 PNG, full-reference).

use crate::Image;

/// Single SSIM comparison: full-image, mean across 8x8 windows.
#[derive(Debug, Clone, Copy)]
pub struct SsimResult {
    /// Mean SSIM across all valid 8x8 windows. 1.0 = identical.
    pub value: f64,
    /// Width in pixels of the comparison grid (after window stride).
    pub windows_x: u32,
    /// Height in pixels of the comparison grid.
    pub windows_y: u32,
}

/// Compute SSIM between two RGBA8 images of the same size.
///
/// Returns `None` if the images differ in dimensions or are too small for
/// any 8x8 window. The luminance is approximated as `Y = 0.299R + 0.587G +
/// 0.114B` per ITU-R BT.601, computed per pixel once for the whole image.
///
/// Defaults match the original SSIM paper constants: `K1 = 0.01`, `K2 = 0.03`,
/// `L = 255` (8-bit dynamic range).
pub fn ssim(a: &Image, b: &Image) -> Option<SsimResult> {
    if a.width != b.width || a.height != b.height || a.width < 8 || a.height < 8 {
        return None;
    }
    let n = (a.width as usize) * (a.height as usize);
    debug_assert_eq!(a.pixels.len(), n * 4);
    debug_assert_eq!(b.pixels.len(), n * 4);

    // ITU-R BT.601 luminance per pixel, u8. The fixed-point weights below are the standard
    // integer approximation (77/256, 150/256, 29/256, summing to 256) of the floating-point
    // BT.601 weights. Integer arithmetic throughout, no `as` casts on the hot path.
    let mut ya: Vec<u8> = Vec::with_capacity(n);
    let mut yb: Vec<u8> = Vec::with_capacity(n);
    for i in 0..n {
        let p = i * 4;
        let y_a = u8::try_from(
            (77 * u32::from(a.pixels[p])
                + 150 * u32::from(a.pixels[p + 1])
                + 29 * u32::from(a.pixels[p + 2])) >> 8,
        ).unwrap_or(0);
        let y_b = u8::try_from(
            (77 * u32::from(b.pixels[p])
                + 150 * u32::from(b.pixels[p + 1])
                + 29 * u32::from(b.pixels[p + 2])) >> 8,
        ).unwrap_or(0);
        ya.push(y_a);
        yb.push(y_b);
    }

    // Sliding 8x8 window statistics over the luminance grids.
    const K1: f64 = 0.01;
    const K2: f64 = 0.03;
    const L: f64 = 255.0;
    let c1 = (K1 * L) * (K1 * L);
    let c2 = (K2 * L) * (K2 * L);

    let w = 8usize;
    let cols = a.width as usize;
    let rows = a.height as usize;
    let nx = cols - w + 1;
    let ny = rows - w + 1;

    let mut sum = 0.0f64;
    let mut count = 0u64;
    for j in 0..ny {
        for i in 0..nx {
            // Compute mean, variance, covariance of the window in one pass.
            let mut sa = 0f64;
            let mut sb = 0f64;
            let mut saa = 0f64;
            let mut sbb = 0f64;
            let mut sab = 0f64;
            let mut n = 0u32;
            for dy in 0..w {
                for dx in 0..w {
                    let idx = (j + dy) * cols + (i + dx);
                    let va = ya[idx] as f64;
                    let vb = yb[idx] as f64;
                    sa += va;
                    sb += vb;
                    saa += va * va;
                    sbb += vb * vb;
                    sab += va * vb;
                    n += 1;
                }
            }
            let nf = n as f64;
            let ma = sa / nf;
            let mb = sb / nf;
            let va = saa / nf - ma * ma;
            let vb = sbb / nf - mb * mb;
            let covar = sab / nf - ma * mb;
            let num = (2.0 * ma * mb + c1) * (2.0 * covar + c2);
            let den = (ma * ma + mb * mb + c1) * (va + vb + c2);
            if den > 0.0 {
                sum += num / den;
                count += 1;
            }
        }
    }

    if count == 0 {
        return None;
    }
    Some(SsimResult {
        value: sum / count as f64,
        windows_x: nx as u32,
        windows_y: ny as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Image {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            pixels.extend_from_slice(&rgba);
        }
        Image { width: w, height: h, pixels }
    }

    #[test]
    fn identical_images_score_one() {
        let a = solid(16, 16, [200, 100, 50, 255]);
        let r = ssim(&a, &a).expect("identical image returns result");
        assert!(
            (r.value - 1.0).abs() < 1e-9,
            "identical images must score 1.0, got {}",
            r.value
        );
        assert_eq!(r.windows_x, 9);
        assert_eq!(r.windows_y, 9);
    }

    #[test]
    fn solid_black_vs_solid_white_is_far_from_one() {
        // On uniform images SSIM degenerates to ~0 (c1, c2 stabilizers dominate when
        // both variances are 0) — what we assert is the structural property: very
        // different from the 1.0 of the identical-images case.
        let a = solid(16, 16, [0, 0, 0, 255]);
        let b = solid(16, 16, [255, 255, 255, 255]);
        let r = ssim(&a, &b).expect("result");
        assert!(r.value < 0.5, "black vs white uniform should be far from 1.0, got {}", r.value);
    }

    #[test]
    fn gradient_vs_shifted_gradient_drops_below_one() {
        let mut grad = solid(16, 16, [0, 0, 0, 255]);
        for y in 0..16 {
            for x in 0..16 {
                let p = ((y * 16 + x) * 4) as usize;
                grad.pixels[p] = (x * 16) as u8;
                grad.pixels[p + 1] = (y * 16) as u8;
            }
        }
        // Shift right by one column to introduce real variance.
        let mut shifted = solid(16, 16, [0, 0, 0, 255]);
        for y in 0..16 {
            for x in 1..16 {
                let dst = (y * 16 + (x - 1)) * 4;
                let src = (y * 16 + x) * 4;
                shifted.pixels[dst] = grad.pixels[src];
                shifted.pixels[dst + 1] = grad.pixels[src + 1];
            }
        }
        let r = ssim(&grad, &shifted).expect("shifted gradient");
        assert!(r.value < 1.0, "shifted gradient must score < 1.0, got {}", r.value);
        assert!(r.value > 0.0, "shifted gradient must score > 0.0, got {}", r.value);
    }

    #[test]
    fn differing_dimensions_returns_none() {
        let a = solid(8, 8, [0, 0, 0, 255]);
        let b = solid(8, 16, [0, 0, 0, 255]);
        assert!(ssim(&a, &b).is_none());
    }

    #[test]
    fn below_minimum_size_returns_none() {
        let a = solid(4, 4, [0, 0, 0, 255]);
        let b = solid(4, 4, [0, 0, 0, 255]);
        assert!(ssim(&a, &b).is_none());
    }
}
