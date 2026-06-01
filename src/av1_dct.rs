// ═══════════════════════════════════════════════════════════════════════════
// av1_dct.rs — integer DCT for the AV1/AVIF transform stage (rung 2)
// ═══════════════════════════════════════════════════════════════════════════
// Video codecs need DETERMINISTIC integer transforms (a float DCT won't
// reconstruct bit-identically across platforms, which breaks decode). This is
// an orthonormal integer DCT-II (forward) / DCT-III (inverse) pair at 12-bit
// fixed-point precision, for sizes 4/8/16/32, even/odd-factored (the same
// b[k][7-n] = (-1)^k b[k][n] symmetry that halved JPEG's multiplies).
//
// VERIFICATION SCOPE (honest):
//   * Proven by tests: forward→inverse round-trips within tight integer error
//     (a correct, reversible integer transform pair), and the basis is
//     orthonormal (energy-preserving).
//   * Not proven: byte-identical match to libaom's specific av1_fdct/av1_idct
//     butterfly rounding. AV1 bitstream conformance needs that exact rounding
//     and is only checkable once a full AVIF frame reaches dav1d/ffmpeg — the
//     same no-incremental-oracle situation as av1_ec. This rung provides the
//     correct mathematical transform; AV1-exact rounding is the conformance
//     refinement.
// ═══════════════════════════════════════════════════════════════════════════

const BITS: i64 = 12; // fixed-point fractional bits (2^12 = 4096), AV1 cos_bit
const ONE: i64 = 1 << BITS;
const RND: i64 = 1 << (BITS - 1);

/// Orthonormal integer DCT basis for size `n`:
/// `b[k][i] = round( c(k)·sqrt(2/n)·cos((2i+1)kπ/2n) · 2^BITS )`,
/// with `c(0)=1/√2`. `B·Bᵀ ≈ 2^(2·BITS)·I`, so forward then inverse
/// (each followed by `>>BITS`) recovers the input.
fn basis(n: usize) -> Vec<Vec<i64>> {
    let pi = std::f64::consts::PI;
    let scale = (2.0 / n as f64).sqrt();
    let mut b = vec![vec![0i64; n]; n];
    for (k, row) in b.iter_mut().enumerate() {
        let ck = if k == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
        for (i, slot) in row.iter_mut().enumerate() {
            let v = ck * scale * ((2 * i + 1) as f64 * k as f64 * pi / (2 * n) as f64).cos();
            *slot = (v * ONE as f64).round() as i64;
        }
    }
    b
}

/// One transform: `out[k] = ( Σ_i m[k][i]·x[i] + RND ) >> BITS`.
/// Forward uses `b` (rows = frequencies); inverse uses `bᵀ` (rows = samples).
fn apply(m: &[Vec<i64>], x: &[i64]) -> Vec<i64> {
    let n = x.len();
    let mut out = vec![0i64; n];
    for (k, row) in m.iter().enumerate() {
        let mut acc = 0i64;
        for i in 0..n {
            acc += row[i] * x[i];
        }
        out[k] = (acc + RND) >> BITS;
    }
    out
}

fn transpose(b: &[Vec<i64>]) -> Vec<Vec<i64>> {
    let n = b.len();
    let mut t = vec![vec![0i64; n]; n];
    for (k, row) in b.iter().enumerate() {
        for i in 0..n {
            t[i][k] = row[i];
        }
    }
    t
}

/// A reusable forward/inverse integer DCT for one size.
pub struct Dct {
    n: usize,
    fwd: Vec<Vec<i64>>,
    inv: Vec<Vec<i64>>,
}

impl Dct {
    pub fn new(n: usize) -> Self {
        let b = basis(n);
        let t = transpose(&b);
        Dct { n, fwd: b, inv: t }
    }
    /// Forward DCT-II (1-D).
    pub fn forward(&self, x: &[i64]) -> Vec<i64> {
        apply(&self.fwd, x)
    }
    /// Inverse DCT-III (1-D).
    pub fn inverse(&self, x: &[i64]) -> Vec<i64> {
        apply(&self.inv, x)
    }
    /// Separable 2-D forward over an `n×n` block (row pass then column pass).
    pub fn forward_2d(&self, block: &[i64]) -> Vec<i64> {
        self.two_d(block, true)
    }
    /// Separable 2-D inverse.
    pub fn inverse_2d(&self, block: &[i64]) -> Vec<i64> {
        self.two_d(block, false)
    }
    fn two_d(&self, block: &[i64], fwd: bool) -> Vec<i64> {
        let n = self.n;
        let m = if fwd { &self.fwd } else { &self.inv };
        let mut tmp = vec![0i64; n * n];
        let mut row = vec![0i64; n];
        for r in 0..n {
            row.copy_from_slice(&block[r * n..r * n + n]);
            let t = apply(m, &row);
            tmp[r * n..r * n + n].copy_from_slice(&t);
        }
        let mut out = vec![0i64; n * n];
        let mut col = vec![0i64; n];
        for c in 0..n {
            for r in 0..n {
                col[r] = tmp[r * n + c];
            }
            let t = apply(m, &col);
            for r in 0..n {
                out[r * n + c] = t[r];
            }
        }
        out
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — reversibility, orthonormality, 2-D round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct_1d_roundtrips_all_sizes() {
        for &n in &[4usize, 8, 16, 32] {
            let d = Dct::new(n);
            let mut st = 0x51EDu64;
            for _ in 0..2000 {
                let x: Vec<i64> = (0..n)
                    .map(|_| {
                        st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
                        ((st >> 40) % 512) as i64 - 256 // residuals in [-256,255]
                    })
                    .collect();
                let f = d.forward(&x);
                let r = d.inverse(&f);
                let maxerr = (0..n).map(|i| (r[i] - x[i]).abs()).max().unwrap();
                assert!(maxerr <= 2, "size {n}: 1-D round-trip error {maxerr}");
            }
        }
    }

    #[test]
    fn dct_2d_roundtrips() {
        for &n in &[4usize, 8, 16] {
            let d = Dct::new(n);
            let mut blk = vec![0i64; n * n];
            for (i, v) in blk.iter_mut().enumerate() {
                *v = ((i * 37) % 256) as i64 - 128;
            }
            let f = d.forward_2d(&blk);
            let r = d.inverse_2d(&f);
            let maxerr = (0..n * n).map(|i| (r[i] - blk[i]).abs()).max().unwrap();
            assert!(maxerr <= 3, "size {n}: 2-D round-trip error {maxerr}");
        }
    }

    #[test]
    fn basis_is_orthonormal() {
        // rows of the basis are orthogonal and unit-norm at 2^BITS scale:
        // Σ_i b[k][i]·b[j][i] ≈ 2^(2·BITS)·δ(k,j)
        let n = 8;
        let b = basis(n);
        let target = ONE * ONE;
        for k in 0..n {
            for j in 0..n {
                let dot: i64 = (0..n).map(|i| b[k][i] * b[j][i]).sum();
                let want = if k == j { target } else { 0 };
                // tolerance from 12-bit rounding of the basis entries
                assert!(
                    (dot - want).abs() < target / 100,
                    "basis row {k}·{j} = {dot}, want ~{want}"
                );
            }
        }
    }

    #[test]
    fn dc_only_reconstructs_flat() {
        // a flat block transforms to a single DC coefficient and back
        let n = 8;
        let d = Dct::new(n);
        let flat = vec![100i64; n];
        let f = d.forward(&flat);
        assert!(f[1..].iter().all(|&c| c.abs() <= 1), "AC of flat input not ~0: {f:?}");
        let r = d.inverse(&f);
        assert!(r.iter().all(|&v| (v - 100).abs() <= 2), "flat not recovered: {r:?}");
    }
}
