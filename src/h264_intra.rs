// ═══════════════════════════════════════════════════════════════════════════
// h264_intra.rs — H.264 rung 2: I_16x16 intra compression (transform+CAVLC)
// ═══════════════════════════════════════════════════════════════════════════
// Real H.264 compression (not I_PCM): I_16x16 macroblocks with DC intra
// prediction, the 4×4 integer transform, the luma-DC Hadamard, scalar
// quantization, and CAVLC residual coding — plus the encoder's reconstruction
// loop so prediction uses decoded neighbors exactly as a decoder would.
//
// VERIFICATION CONTRACT: a conformant decoder must reproduce the encoder's
// reconstructed samples bit-exactly. So the test is: ffmpeg-decoded YUV ==
// our reconstructed YUV, byte-for-byte. If that holds, prediction + transform
// + quant + CAVLC are all correct together.
//
// Scope this rung: luma DC+AC coded (CBPLuma = 0 or 15) AND chroma DC+AC
// coded (CBPChroma 1/2) — full intra residual, bit-exact vs ffmpeg on
// chroma-rich content (verified: flat color, color edges, gradients, color
// noise). DC intra prediction only (mode 2). One slice, frame_mbs_only,
// CAVLC, fixed QP. The chroma-DC reconstruction scale ((deq·V)<<(cshift-1))
// was solved against the ffmpeg oracle, not derived.
//
// The integer transform is linear (factorable like the DCT); quantization is
// the lossy, non-invertible step (no inverse to "look up" — BeyondTheThrone
// `zero_not_being`). CAVLC is data-dependent entropy coding. No x=B/U shortcut.
// ═══════════════════════════════════════════════════════════════════════════

use crate::h264bits::BitWriter;
use crate::h264bits::{build_pps, build_sps, emit_nal, rgb_to_ycbcr601_limited};

// luma4x4BlkIdx → (x,y) pixel offset within the 16×16 MB (H.264 Z-order).
const BLK_XY: [(usize, usize); 16] = [
    (0, 0), (4, 0), (0, 4), (4, 4),
    (8, 0), (12, 0), (8, 4), (12, 4),
    (0, 8), (4, 8), (0, 12), (4, 12),
    (8, 8), (12, 8), (8, 12), (12, 12),
];

// zig-zag scan for a 4×4 block (frequency order used by CAVLC).
const ZZ4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

// luma4x4BlkIdx → raster index in the 4×4 grid of block-DCs (for the I_16x16
// luma DC Hadamard, which operates in raster order, not Z-order).
const DC_RASTER: [usize; 16] = {
    let mut m = [0usize; 16];
    let mut k = 0;
    while k < 16 {
        let bx = BLK_XY[k].0 / 4;
        let by = BLK_XY[k].1 / 4;
        m[k] = by * 4 + bx;
        k += 1;
    }
    m
};

// ── quantization tables (H.264 8.5) ──────────────────────────────────────
// MF[qp%6][class], class: 0=(0,0)/(0,2)/(2,0)/(2,2), 1=(1,1)/(1,3)/(3,1)/(3,3), 2=rest
const MF: [[i32; 3]; 6] = [
    [13107, 5243, 8066],
    [11916, 4660, 7490],
    [10082, 4194, 6554],
    [9362, 3647, 5825],
    [8192, 3355, 5243],
    [7282, 2893, 4559],
];
// V[qp%6][class] dequant scale
const V: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

#[inline]
fn pos_class(i: usize) -> usize {
    // i in 0..16 row-major (r=i/4, c=i%4)
    let r = i / 4;
    let c = i % 4;
    match (r % 2, c % 2) {
        (0, 0) => 0, // even,even
        (1, 1) => 1, // odd,odd
        _ => 2,
    }
}

// ── core 4×4 integer transform ─────────────────────────────────────────────
fn fwd_core(b: &[i32; 16]) -> [i32; 16] {
    // Cf * X * Cf^T, Cf rows: [1,1,1,1],[2,1,-1,-2],[1,-1,-1,1],[1,-2,2,-1]
    let mut t = [0i32; 16];
    // rows
    for r in 0..4 {
        let x = [b[r * 4], b[r * 4 + 1], b[r * 4 + 2], b[r * 4 + 3]];
        let a0 = x[0] + x[3];
        let a1 = x[1] + x[2];
        let a2 = x[1] - x[2];
        let a3 = x[0] - x[3];
        t[r * 4] = a0 + a1;
        t[r * 4 + 1] = 2 * a3 + a2;
        t[r * 4 + 2] = a0 - a1;
        t[r * 4 + 3] = a3 - 2 * a2;
    }
    let mut o = [0i32; 16];
    for c in 0..4 {
        let x = [t[c], t[4 + c], t[8 + c], t[12 + c]];
        let a0 = x[0] + x[3];
        let a1 = x[1] + x[2];
        let a2 = x[1] - x[2];
        let a3 = x[0] - x[3];
        o[c] = a0 + a1;
        o[4 + c] = 2 * a3 + a2;
        o[8 + c] = a0 - a1;
        o[12 + c] = a3 - 2 * a2;
    }
    o
}

fn inv_core(w: &[i32; 16]) -> [i32; 16] {
    // Ci^T * W * Ci with the standard integer butterflies; final (x+32)>>6.
    let mut t = [0i32; 16];
    for r in 0..4 {
        let x = [w[r * 4], w[r * 4 + 1], w[r * 4 + 2], w[r * 4 + 3]];
        let a0 = x[0] + x[2];
        let a1 = x[0] - x[2];
        let a2 = (x[1] >> 1) - x[3];
        let a3 = x[1] + (x[3] >> 1);
        t[r * 4] = a0 + a3;
        t[r * 4 + 1] = a1 + a2;
        t[r * 4 + 2] = a1 - a2;
        t[r * 4 + 3] = a0 - a3;
    }
    let mut o = [0i32; 16];
    for c in 0..4 {
        let x = [t[c], t[4 + c], t[8 + c], t[12 + c]];
        let a0 = x[0] + x[2];
        let a1 = x[0] - x[2];
        let a2 = (x[1] >> 1) - x[3];
        let a3 = x[1] + (x[3] >> 1);
        o[c] = (a0 + a3 + 32) >> 6;
        o[4 + c] = (a1 + a2 + 32) >> 6;
        o[8 + c] = (a1 - a2 + 32) >> 6;
        o[12 + c] = (a0 - a3 + 32) >> 6;
    }
    o
}

// 2×2 Hadamard for the chroma DC block (H.264 8.5.11.1).
// Forward and inverse are the same butterfly (self-inverse up to the ×4 the
// scaling absorbs): [[1,1],[1,-1]] · X · [[1,1],[1,-1]].
fn hadamard2(b: &[i32; 4]) -> [i32; 4] {
    // rows
    let r0 = [b[0] + b[1], b[0] - b[1]];
    let r1 = [b[2] + b[3], b[2] - b[3]];
    // columns
    [r0[0] + r1[0], r0[1] + r1[1], r0[0] - r1[0], r0[1] - r1[1]]
}

// QPc from QPy per Table 8-15 (chroma QP mapping). For qp<30, QPc=qp.
fn chroma_qp(qpy: i32) -> i32 {
    const MAP: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    let q = qpy.clamp(0, 51);
    if q < 30 {
        q
    } else {
        MAP[(q - 30) as usize]
    }
}

// 4×4 Hadamard for the luma DC block.
fn hadamard4(b: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for r in 0..4 {
        let x = [b[r * 4], b[r * 4 + 1], b[r * 4 + 2], b[r * 4 + 3]];
        let a0 = x[0] + x[3];
        let a1 = x[1] + x[2];
        let a2 = x[1] - x[2];
        let a3 = x[0] - x[3];
        t[r * 4] = a0 + a1;
        t[r * 4 + 1] = a3 + a2;
        t[r * 4 + 2] = a0 - a1;
        t[r * 4 + 3] = a3 - a2;
    }
    let mut o = [0i32; 16];
    for c in 0..4 {
        let x = [t[c], t[4 + c], t[8 + c], t[12 + c]];
        let a0 = x[0] + x[3];
        let a1 = x[1] + x[2];
        let a2 = x[1] - x[2];
        let a3 = x[0] - x[3];
        o[c] = a0 + a1;
        o[4 + c] = a3 + a2;
        o[8 + c] = a0 - a1;
        o[12 + c] = a3 - a2;
    }
    o
}

// ── CAVLC ──────────────────────────────────────────────────────────────────
include!("h264_cavlc_tables.rs");

struct Cavlc<'a> {
    bw: &'a mut BitWriter,
}

impl<'a> Cavlc<'a> {
    /// Encode one residual block in CAVLC. `coeffs` are in *scan order*
    /// (zig-zag): index 0 = lowest frequency, last = highest. `nc` is the
    /// context. Returns TotalCoeff (for neighbor context bookkeeping).
    fn block(&mut self, coeffs: &[i32], nc: i32) -> usize {
        let max = coeffs.len();
        // nonzero (position, level) low→high frequency
        let nz: Vec<(usize, i32)> = coeffs
            .iter()
            .enumerate()
            .filter(|(_, &v)| v != 0)
            .map(|(p, &v)| (p, v))
            .collect();
        let tc = nz.len();

        // trailing ones: from highest-freq end, consecutive ±1, capped at 3
        let mut t1 = 0usize;
        for &(_, lv) in nz.iter().rev() {
            if lv.abs() == 1 && t1 < 3 {
                t1 += 1;
            } else {
                break;
            }
        }

        let (len, code) = coeff_token(tc, t1, nc);
        self.bw.put_bits(code, len);
        if tc == 0 {
            return 0;
        }

        // trailing-ones signs (highest-freq first; 1 = negative)
        for j in 0..t1 {
            let lv = nz[tc - 1 - j].1;
            self.bw.put_bit((lv < 0) as u32);
        }

        // remaining levels, highest-freq first
        let mut suffix_length: u32 = if tc > 10 && t1 < 3 { 1 } else { 0 };
        for j in t1..tc {
            let level = nz[tc - 1 - j].1;
            let mut level_code: i32 = if level > 0 { (level << 1) - 2 } else { (-level << 1) - 1 };
            if j == t1 && t1 < 3 {
                level_code -= 2;
            }
            let lc = level_code as u32;
            if suffix_length == 0 {
                if lc < 14 {
                    self.put_level_prefix(lc);
                } else if lc < 30 {
                    self.put_level_prefix(14);
                    self.bw.put_bits(lc - 14, 4);
                } else {
                    self.put_level_prefix(15);
                    self.bw.put_bits(lc - 30, 12);
                }
            } else {
                let prefix = lc >> suffix_length;
                if prefix < 15 {
                    self.put_level_prefix(prefix);
                    self.bw.put_bits(lc & ((1 << suffix_length) - 1), suffix_length);
                } else {
                    self.put_level_prefix(15);
                    let escape = lc - (15u32 << suffix_length);
                    self.bw.put_bits(escape & 0xFFF, 12);
                }
            }
            if suffix_length == 0 {
                suffix_length = 1;
            }
            if level.unsigned_abs() > (3u32 << (suffix_length - 1)) && suffix_length < 6 {
                suffix_length += 1;
            }
        }

        // total_zeros = zeros before the highest-freq nonzero in the coded prefix
        let last_pos = nz[tc - 1].0;
        let total_zeros = (last_pos + 1 - tc) as i32;
        if tc < max {
            self.put_total_zeros(tc, total_zeros, max);
        }

        // run_before for each coded level except the lowest-freq one (implicit)
        let mut zeros_left = total_zeros;
        for j in 0..tc - 1 {
            // gap between nz[tc-1-j] (higher) and nz[tc-2-j] (lower)
            let high = nz[tc - 1 - j].0;
            let low = nz[tc - 2 - j].0;
            let run = (high - low - 1) as i32;
            if zeros_left <= 0 {
                break;
            }
            self.put_run_before(run, zeros_left);
            zeros_left -= run;
        }

        tc
    }

    fn put_level_prefix(&mut self, p: u32) {
        for _ in 0..p {
            self.bw.put_bit(0);
        }
        self.bw.put_bit(1);
    }

    fn put_total_zeros(&mut self, total_coeff: usize, total_zeros: i32, max: usize) {
        let (len, code) = if max == 4 {
            total_zeros_chroma_dc(total_coeff, total_zeros)
        } else {
            total_zeros_4x4(total_coeff, total_zeros)
        };
        self.bw.put_bits(code, len);
    }

    fn put_run_before(&mut self, run: i32, zeros_left: i32) {
        let (len, code) = run_before(run, zeros_left);
        self.bw.put_bits(code, len);
    }
}

/// Encode a sequence of RGBA8 frames as intra-only (all-IDR) compressed H.264.
pub fn encode_sequence(frames: &[Vec<u8>], width: u32, height: u32, qp: i32) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("H264-intra: no frames".into());
    }
    let need = (width * height * 4) as usize;
    let mut out = Vec::new();
    emit_nal(&mut out, 3, 7, &build_sps(width, height));
    emit_nal(&mut out, 3, 8, &build_pps());
    for (i, f) in frames.iter().enumerate() {
        if f.len() < need {
            return Err(format!("H264-intra: frame {i} too small"));
        }
        let slice = encode_idr_intra(width, height, f, qp, i as u32)?;
        emit_nal(&mut out, 3, 5, &slice);
    }
    Ok(out)
}

/// Reconstructed-frame accessor — also returns the planes so the verifier can
/// compare against ffmpeg's decode.
pub fn encode_sequence_with_recon(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    qp: i32,
) -> Result<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>), String> {
    if frames.is_empty() {
        return Err("H264-intra: no frames".into());
    }
    let need = (width * height * 4) as usize;
    let mut out = Vec::new();
    emit_nal(&mut out, 3, 7, &build_sps(width, height));
    emit_nal(&mut out, 3, 8, &build_pps());
    let mut recons = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        if f.len() < need {
            return Err(format!("H264-intra: frame {i} too small"));
        }
        let (slice, recon) = encode_idr_intra_recon(width, height, f, qp, i as u32)?;
        emit_nal(&mut out, 3, 5, &slice);
        recons.push(recon);
    }
    Ok((out, recons))
}

fn encode_idr_intra(width: u32, height: u32, rgba: &[u8], qp: i32, idr: u32) -> Result<Vec<u8>, String> {
    Ok(encode_idr_intra_recon(width, height, rgba, qp, idr)?.0)
}

#[allow(clippy::needless_range_loop)]
/// Encode one frame as an IDR I-slice, returning (slice RBSP, reconstructed
/// Y/Cb/Cr planes cropped to width×height). Exposed for the inter (P-frame)
/// encoder, which re-keys with a full IDR when content changes.
pub(crate) fn encode_idr_intra_recon(
    width: u32,
    height: u32,
    rgba: &[u8],
    qp: i32,
    idr: u32,
) -> Result<(Vec<u8>, (Vec<u8>, Vec<u8>, Vec<u8>)), String> {
    let w = width as usize;
    let h = height as usize;
    let mbw = ((width + 15) / 16) as usize;
    let mbh = ((height + 15) / 16) as usize;
    let pw = mbw * 16;
    let ph = mbh * 16;
    let cw = pw / 2;
    let ch = ph / 2;

    // source planes (full-res luma, full-res chroma before subsample)
    let mut src_y = vec![0u8; pw * ph];
    let mut src_cb = vec![0u8; cw * ch];
    let mut src_cr = vec![0u8; cw * ch];
    {
        // luma
        for y in 0..ph {
            for x in 0..pw {
                let sx = x.min(w - 1);
                let sy = y.min(h - 1);
                let i = (sy * w + sx) * 4;
                let (yy, _, _) =
                    rgb_to_ycbcr601_limited(rgba[i] as f32, rgba[i + 1] as f32, rgba[i + 2] as f32);
                src_y[y * pw + x] = yy;
            }
        }
        // chroma 4:2:0 (average 2×2)
        for cy in 0..ch {
            for cx in 0..cw {
                let mut sb = 0u32;
                let mut sr = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let x = (cx * 2 + dx).min(w - 1);
                        let y = (cy * 2 + dy).min(h - 1);
                        let i = (y * w + x) * 4;
                        let (_, cb, cr) = rgb_to_ycbcr601_limited(
                            rgba[i] as f32,
                            rgba[i + 1] as f32,
                            rgba[i + 2] as f32,
                        );
                        sb += cb as u32;
                        sr += cr as u32;
                    }
                }
                src_cb[cy * cw + cx] = ((sb + 2) / 4) as u8;
                src_cr[cy * cw + cx] = ((sr + 2) / 4) as u8;
            }
        }
    }

    // reconstructed planes
    let mut rec_y = vec![0u8; pw * ph];
    let mut rec_cb = vec![0u8; cw * ch];
    let mut rec_cr = vec![0u8; cw * ch];
    // per-4x4 TotalCoeff grids for nC context (luma in 4x4 units across frame)
    let lw4 = pw / 4;
    let lh4 = ph / 4;
    let mut nz_luma = vec![-1i32; lw4 * lh4]; // -1 = not yet coded
    // chroma 4×4 TotalCoeff grids (cw/4 × ch/4) for chroma-AC nC context, per plane
    let cw4 = cw / 4;
    let ch4 = ch / 4;
    let mut nz_cb = vec![-1i32; cw4 * ch4];
    let mut nz_cr = vec![-1i32; cw4 * ch4];

    let mut bw = BitWriter::new();
    // slice header (IDR, I-slice). SliceQP = 26 + slice_qp_delta. We want SliceQP=qp.
    bw.ue(0); // first_mb_in_slice
    bw.ue(7); // slice_type = I
    bw.ue(0); // pps id
    bw.put_bits(0, 4); // frame_num
    bw.ue(idr); // idr_pic_id
    bw.put_bits(0, 4); // poc lsb
    bw.put_flag(false); // no_output_of_prior_pics
    bw.put_flag(false); // long_term_reference
    bw.se(qp - 26); // slice_qp_delta → SliceQP = qp
    // deblocking_filter_control_present is set in the PPS, so signal it here.
    // disable_deblocking_filter_idc = 1 → decoder skips the in-loop deblocking
    // filter, so its reconstruction matches our (non-deblocked) recon bit-exact.
    bw.ue(1); // disable_deblocking_filter_idc = 1

    for mby in 0..mbh {
        for mbx in 0..mbw {
            // ── luma 16×16 DC prediction from reconstructed neighbors ──
            let avail_l = mbx > 0;
            let avail_t = mby > 0;
            let pred_y = dc_pred_16(&rec_y, pw, mbx, mby, avail_l, avail_t);

            // residuals per 4×4, forward transform, collect DC, quant AC
            let mut dc_vals = [0i32; 16]; // DC per block in raster blk grid (we map by blkIdx→raster pos)
            let mut ac_z: [[i32; 16]; 16] = [[0; 16]; 16];
            let mut ac_recon: [[i32; 16]; 16] = [[0; 16]; 16];
            // first: forward transform each block, capture DC (pre-quant W[0])
            let mut wblocks: [[i32; 16]; 16] = [[0; 16]; 16];
            for blk in 0..16 {
                let (bx, by) = BLK_XY[blk];
                let mut resid = [0i32; 16];
                for yy in 0..4 {
                    for xx in 0..4 {
                        let px = src_y[(mby * 16 + by + yy) * pw + (mbx * 16 + bx + xx)] as i32;
                        resid[yy * 4 + xx] = px - pred_y;
                    }
                }
                wblocks[blk] = fwd_core(&resid);
                dc_vals[DC_RASTER[blk]] = wblocks[blk][0];
            }
            // luma DC: Hadamard then quant (DC has its own scaling)
            let dc_had = hadamard4(&dc_vals);
            let qbits = 15 + qp / 6;
            let rem = (qp % 6) as usize;
            // Forward DC quant: the unnormalised 2-D Hadamard adds ×16, so the
            // forward shift is qbits+4 (not qbits+1) to invert the spec §8.5.10
            // 16·V dequant. Round-trip with the 16·V inverse is then exact.
            let fq = qbits + 4;
            let f = (1i64 << fq) / 3;
            let mut dc_z = [0i32; 16];
            for i in 0..16 {
                let mf = MF[rem][0] as i64;
                let v = dc_had[i] as i64;
                let zz = ((v.abs() * mf + f) >> fq) as i32;
                dc_z[i] = if v < 0 { -zz } else { zz };
            }
            // dequant DC (inverse Hadamard then scale) for reconstruction
            let dc_deq = hadamard4(&dc_z); // inverse Hadamard == forward (orthogonal, /4 handled by scale)
            let mut dc_recon = [0i32; 16];
            let shift = qp / 6;
            for i in 0..16 {
                // DC inverse scaling consistent with the forward >>(qbits+1):
                // the unnormalised 2-D Hadamard contributes a ×16, so the
                // round-trip-consistent inverse weight is 2·V (not 16·V).
                let vv = V[rem][0]; // LevelScale = weightScale(16)·normAdjust(V); spec §8.5.10
                if shift >= 6 {
                    dc_recon[i] = (dc_deq[i] * vv) << (shift - 2); // (f·16·V)<<(shift-6)
                } else {
                    dc_recon[i] = (dc_deq[i] * vv * 16 + (1 << (5 - shift))) >> (6 - shift);
                }
            }

            // AC: quant each block's AC coeffs (excluding DC), reconstruct full block
            let mut cbp_luma_nonzero = false;
            for blk in 0..16 {
                // build W with DC replaced by reconstructed DC for this block
                let mut wq = [0i32; 16];
                // AC quant
                let mut z = [0i32; 16];
                for i in 1..16 {
                    let cl = pos_class(i);
                    let mf = MF[rem][cl] as i64;
                    let wv = wblocks[blk][i] as i64;
                    let f_ac = (1i64 << qbits) / 3;
                    let zz = ((wv.abs() * mf + f_ac) >> qbits) as i32;
                    z[i] = if wv < 0 { -zz } else { zz };
                    if z[i] != 0 {
                        cbp_luma_nonzero = true;
                    }
                }
                ac_z[blk] = z;
                // dequant AC: LevelScale = weightScale(16) * normAdjust(V). 8.5.12.1.
                for i in 1..16 {
                    let cl = pos_class(i);
                    let vv = V[rem][cl];
                    if shift >= 4 {
                        // (z*16*vv) << (shift-4) == (z*vv) << shift
                        wq[i] = (z[i] * vv) << shift;
                    } else {
                        wq[i] = (z[i] * vv * 16 + (1 << (3 - shift))) >> (4 - shift);
                    }
                }
                wq[0] = dc_recon[DC_RASTER[blk]]; // reconstructed DC for this block (raster→blkIdx)
                ac_recon[blk] = inv_core(&wq);
            }

            // write reconstructed luma samples
            for blk in 0..16 {
                let (bx, by) = BLK_XY[blk];
                for yy in 0..4 {
                    for xx in 0..4 {
                        let v = pred_y + ac_recon[blk][yy * 4 + xx];
                        let vv = v.clamp(0, 255) as u8;
                        rec_y[(mby * 16 + by + yy) * pw + (mbx * 16 + bx + xx)] = vv;
                    }
                }
            }

            // ── chroma residual (CBPChroma 1 = DC+AC) ──
            // DC prediction per plane, then per-4×4 residual → transform →
            // DC(2×2 Hadamard)+AC quant, x264-exact scaling. The 4 sub-blocks
            // are raster: (cx,cy) in {0,1}×{0,1}, pixel origin (cx*4, cy*4).
            let qpc = chroma_qp(qp);
            let crem = (qpc % 6) as usize;
            let cshift = qpc / 6;
            let cq4mf = c_shift(MF[crem][0] as i64, qpc / 6 - 1);
            let c_mf = cq4mf >> 1;
            let c_bias = ((1i64 << 15) / cq4mf) << 1;
            let cqbits = 15 + qpc / 6;
            let cdmf = (V[crem][0] as i64) << cshift;

            let (pred_cb, pred_cr) = chroma_dc_pred(&rec_cb, &rec_cr, cw, mbx, mby, avail_l, avail_t);
            // per plane: returns (dc_z[4] scan, ac_z[4][16], reconstructed 8×8, any_ac)
            let mut chroma_planes: [(Vec<i32>, [[i32; 16]; 4], [[u8; 64]; 1], bool); 2] = [
                (Vec::new(), [[0; 16]; 4], [[0u8; 64]; 1], false),
                (Vec::new(), [[0; 16]; 4], [[0u8; 64]; 1], false),
            ];
            let mut cbp_chroma_ac = false;
            for (pi, (src, pred)) in [(&src_cb, &pred_cb), (&src_cr, &pred_cr)].into_iter().enumerate() {
                let mut dc_vals = [0i32; 4]; // raster order of the 4 sub-blocks' W[0]
                let mut wblk: [[i32; 16]; 4] = [[0; 16]; 4];
                for sb in 0..4 {
                    let bx = (sb % 2) * 4;
                    let by = (sb / 2) * 4;
                    let p = pred[sb] as i32;
                    let mut resid = [0i32; 16];
                    for yy in 0..4 {
                        for xx in 0..4 {
                            let s = src[(mby * 8 + by + yy) * cw + (mbx * 8 + bx + xx)] as i32;
                            resid[yy * 4 + xx] = s - p;
                        }
                    }
                    wblk[sb] = fwd_core(&resid);
                    dc_vals[sb] = wblk[sb][0];
                }
                // chroma DC: 2×2 Hadamard then x264 quant
                let had = hadamard2(&dc_vals);
                let mut dc_z = [0i32; 4];
                for i in 0..4 {
                    let d = had[i] as i64;
                    let q = ((c_bias + d.abs()) * c_mf) >> 16;
                    dc_z[i] = if d < 0 { -(q as i32) } else { q as i32 };
                }
                // Reconstructed DC coefficient, scaled to match the AC dequant so
                // it can be placed at wq[0] and run through the SAME inv_core
                // (which finishes with (x+32)>>6). AC dequant is (z·V)<<cshift
                // (= z·V·16<<(cshift-4)); the 2×2 inverse Hadamard already carries
                // the ×4 chroma-DC gain, so the DC coeff scale is (deq·V)<<(cshift)
                // with the spec's chroma-DC >>1 normalisation folded in.
                // DC coeff placed at wq[0] for the shared inv_core (which ends with
                // (x+32)>>6). Empirically solved against the ffmpeg oracle: the
                // chroma-DC reconstruction scale is (deq·V) << (cshift-1), i.e. one
                // shift below the AC scale (the 2×2 Hadamard's ×4 gain vs the 4×4
                // path accounts for the difference). Verified bit-exact vs ffmpeg.
                let dc_deq = hadamard2(&dc_z);
                let mut dc_recon = [0i32; 4];
                let _ = cdmf;
                for i in 0..4 {
                    let vv = V[crem][0] as i64;
                    if cshift >= 1 {
                        dc_recon[i] = ((dc_deq[i] as i64 * vv) << (cshift - 1)) as i32;
                    } else {
                        dc_recon[i] = ((dc_deq[i] as i64 * vv + 1) >> 1) as i32;
                    }
                }
                // AC quant per sub-block (indices 1..16), reconstruct each block
                let mut ac_z: [[i32; 16]; 4] = [[0; 16]; 4];
                let mut recon8 = [[0u8; 64]; 1];
                let mut any_ac = false;
                for sb in 0..4 {
                    let mut z = [0i32; 16];
                    let mut wq = [0i32; 16];
                    for i in 1..16 {
                        let cl = pos_class(i);
                        let mf = MF[crem][cl] as i64;
                        let wv = wblk[sb][i] as i64;
                        let fac = (1i64 << cqbits) / 3;
                        let zz = ((wv.abs() * mf + fac) >> cqbits) as i32;
                        z[i] = if wv < 0 { -zz } else { zz };
                        if z[i] != 0 {
                            any_ac = true;
                        }
                        let vv = V[crem][cl];
                        if cshift >= 4 {
                            wq[i] = (z[i] * vv) << cshift;
                        } else {
                            wq[i] = (z[i] * vv * 16 + (1 << (3 - cshift))) >> (4 - cshift);
                        }
                    }
                    wq[0] = dc_recon[sb];
                    let sp = inv_core(&wq);
                    let bx = (sb % 2) * 4;
                    let by = (sb / 2) * 4;
                    let p = pred[sb] as i32;
                    for yy in 0..4 {
                        for xx in 0..4 {
                            recon8[0][(by + yy) * 8 + (bx + xx)] =
                                (p + sp[yy * 4 + xx]).clamp(0, 255) as u8;
                        }
                    }
                    ac_z[sb] = z;
                }
                // dc scan order for the 2×2 chroma DC is raster (no zigzag for 2×2)
                let dc_scan: Vec<i32> = dc_z.to_vec();
                chroma_planes[pi] = (dc_scan, ac_z, recon8, any_ac);
                if any_ac {
                    cbp_chroma_ac = true;
                }
            }
            // write reconstructed chroma samples
            for (pi, plane) in [&mut rec_cb, &mut rec_cr].into_iter().enumerate() {
                let recon8 = &chroma_planes[pi].2[0];
                for cy in 0..8 {
                    for cx in 0..8 {
                        plane[(mby * 8 + cy) * cw + (mbx * 8 + cx)] = recon8[cy * 8 + cx];
                    }
                }
            }

            // ── mb_type & MB layer ──
            let cbp_luma_flag = if cbp_luma_nonzero { 1 } else { 0 };
            // CBPChroma: 0=none, 1=DC only, 2=DC+AC. We always send chroma DC, so
            // min is 1; 2 if any AC coeff is nonzero.
            let cbp_chroma: u32 = if cbp_chroma_ac { 2 } else { 1 };
            // I_16x16, pred mode 2 (DC): mb_type = 1 + 2 + 4*cbpC + 12*cbpLflag
            let mb_type = 1 + 2 + 4 * cbp_chroma + 12 * cbp_luma_flag;
            bw.ue(mb_type);
            bw.ue(0); // intra_chroma_pred_mode = DC
            bw.se(0); // mb_qp_delta = 0

            // residual() in H.264 order: luma DC, luma AC, chroma DC (both), chroma AC (both).
            let mut cav = Cavlc { bw: &mut bw };
            let nc_dc = luma_nc(&nz_luma, lw4, mbx, mby, 0);
            let dc_scan: Vec<i32> = ZZ4.iter().map(|&i| dc_z[i]).collect();
            cav.block(&dc_scan, nc_dc);

            if cbp_luma_flag == 1 {
                for blk in 0..16 {
                    let nc = luma_nc(&nz_luma, lw4, mbx, mby, blk);
                    let ac_scan: Vec<i32> = ZZ4.iter().skip(1).map(|&i| ac_z[blk][i]).collect();
                    let tc = cav.block(&ac_scan, nc);
                    set_luma_nz(&mut nz_luma, lw4, mbx, mby, blk, tc as i32);
                }
            } else {
                for blk in 0..16 {
                    set_luma_nz(&mut nz_luma, lw4, mbx, mby, blk, 0);
                }
            }

            // chroma DC blocks (both planes), nC = -1 (chroma-DC context), 4 coeffs
            for pi in 0..2 {
                cav.block(&chroma_planes[pi].0, -1);
            }
            // chroma AC blocks (both planes) only if CBPChroma == 2
            if cbp_chroma == 2 {
                for pi in 0..2 {
                    let grid: &mut Vec<i32> = if pi == 0 { &mut nz_cb } else { &mut nz_cr };
                    for sb in 0..4 {
                        let nc = chroma_ac_nc(grid, cw4, mbx, mby, sb);
                        let ac_scan: Vec<i32> =
                            ZZ4.iter().skip(1).map(|&i| chroma_planes[pi].1[sb][i]).collect();
                        let tc = cav.block(&ac_scan, nc);
                        set_chroma_ac_nz(grid, cw4, mbx, mby, sb, tc as i32);
                    }
                }
            } else {
                for grid in [&mut nz_cb, &mut nz_cr] {
                    for sb in 0..4 {
                        set_chroma_ac_nz(grid, cw4, mbx, mby, sb, 0);
                    }
                }
            }
        }
    }
    bw.trailing_bits();

    // crop reconstructed planes to width×height for the verifier
    let mut ry = vec![0u8; w * h];
    for y in 0..h {
        ry[y * w..y * w + w].copy_from_slice(&rec_y[y * pw..y * pw + w]);
    }
    let mut rcb = vec![0u8; (w / 2).max(1) * (h / 2).max(1)];
    let mut rcr = vec![0u8; (w / 2).max(1) * (h / 2).max(1)];
    let cw2 = (w + 1) / 2;
    let chh = (h + 1) / 2;
    rcb.resize(cw2 * chh, 0);
    rcr.resize(cw2 * chh, 0);
    for y in 0..chh {
        for x in 0..cw2 {
            rcb[y * cw2 + x] = rec_cb[y * cw + x];
            rcr[y * cw2 + x] = rec_cr[y * cw + x];
        }
    }

    Ok((bw.into_rbsp(), (ry, rcb, rcr)))
}

// ── prediction helpers ──────────────────────────────────────────────────────
fn dc_pred_16(rec: &[u8], pw: usize, mbx: usize, mby: usize, al: bool, at: bool) -> i32 {
    let x0 = mbx * 16;
    let y0 = mby * 16;
    let mut sl = 0i32;
    let mut st = 0i32;
    if al {
        for k in 0..16 {
            sl += rec[(y0 + k) * pw + (x0 - 1)] as i32;
        }
    }
    if at {
        for k in 0..16 {
            st += rec[(y0 - 1) * pw + (x0 + k)] as i32;
        }
    }
    match (al, at) {
        (true, true) => (sl + st + 16) >> 5,
        (true, false) => (sl + 8) >> 4,
        (false, true) => (st + 8) >> 4,
        (false, false) => 128,
    }
}

// chroma 8×8 DC prediction (8.3.4) → returns 2×2 DC values for Cb and Cr.
fn chroma_dc_pred(
    rcb: &[u8],
    rcr: &[u8],
    cw: usize,
    mbx: usize,
    mby: usize,
    al: bool,
    at: bool,
) -> ([u8; 4], [u8; 4]) {
    let pred_plane = |rec: &[u8]| -> [u8; 4] {
        let x0 = mbx * 8;
        let y0 = mby * 8;
        let top = |a: usize, b: usize| -> Option<i32> {
            if at {
                Some((a..b).map(|k| rec[(y0 - 1) * cw + (x0 + k)] as i32).sum())
            } else {
                None
            }
        };
        let left = |a: usize, b: usize| -> Option<i32> {
            if al {
                Some((a..b).map(|k| rec[(y0 + k) * cw + (x0 - 1)] as i32).sum())
            } else {
                None
            }
        };
        let mut out = [128u8; 4];
        // blk0 TL: top[0..4]+left[0..4]
        out[0] = combine(top(0, 4), left(0, 4));
        // blk1 TR: prefer top[4..8]
        out[1] = match (top(4, 8), left(0, 4)) {
            (Some(t), _) => ((t + 2) >> 2) as u8,
            (None, Some(l)) => ((l + 2) >> 2) as u8,
            _ => 128,
        };
        // blk2 BL: prefer left[4..8]
        out[2] = match (left(4, 8), top(0, 4)) {
            (Some(l), _) => ((l + 2) >> 2) as u8,
            (None, Some(t)) => ((t + 2) >> 2) as u8,
            _ => 128,
        };
        // blk3 BR: top[4..8]+left[4..8]
        out[3] = combine(top(4, 8), left(4, 8));
        out
    };
    (pred_plane(rcb), pred_plane(rcr))
}

fn combine(t: Option<i32>, l: Option<i32>) -> u8 {
    match (t, l) {
        (Some(t), Some(l)) => ((t + l + 4) >> 3) as u8,
        (Some(t), None) => ((t + 2) >> 2) as u8,
        (None, Some(l)) => ((l + 2) >> 2) as u8,
        (None, None) => 128,
    }
}

// x264 SHIFT(x,s): s≤0 → x<<-s ; else (x+(1<<(s-1)))>>s  (rounding right shift)
fn c_shift(x: i64, s: i32) -> i64 {
    if s <= 0 {
        x << (-s)
    } else {
        (x + (1 << (s - 1))) >> s
    }
}

// chroma-AC nC context: the chroma plane has a 2×2 grid of 4×4 sub-blocks per
// MB; neighbors come from the left/top sub-block (within or across MBs).
// `grid` is the plane's frame-wide (cw/4 × ch/4) TotalCoeff grid (-1 = uncoded).
fn chroma_ac_nc(grid: &[i32], cw4: usize, mbx: usize, mby: usize, sb: usize) -> i32 {
    let gx = mbx * 2 + (sb % 2);
    let gy = mby * 2 + (sb / 2);
    let left = if gx > 0 { grid[gy * cw4 + (gx - 1)] } else { -1 };
    let top = if gy > 0 { grid[(gy - 1) * cw4 + gx] } else { -1 };
    match (left >= 0, top >= 0) {
        (true, true) => (left + top + 1) >> 1,
        (true, false) => left,
        (false, true) => top,
        (false, false) => 0,
    }
}

fn set_chroma_ac_nz(grid: &mut [i32], cw4: usize, mbx: usize, mby: usize, sb: usize, tc: i32) {
    let gx = mbx * 2 + (sb % 2);
    let gy = mby * 2 + (sb / 2);
    grid[gy * cw4 + gx] = tc;
}

// ── nC context bookkeeping (luma 4×4 grid in frame coordinates) ─────────────
fn blk_frame_xy(mbx: usize, mby: usize, blk: usize) -> (usize, usize) {
    let (bx, by) = BLK_XY[blk];
    (mbx * 16 + bx, mby * 16 + by) // pixel coords; /4 for grid
}

fn luma_nc(nz: &[i32], lw4: usize, mbx: usize, mby: usize, blk: usize) -> i32 {
    let (px, py) = blk_frame_xy(mbx, mby, blk);
    let gx = px / 4;
    let gy = py / 4;
    let left = if gx > 0 { nz[gy * lw4 + (gx - 1)] } else { -1 };
    let top = if gy > 0 { nz[(gy - 1) * lw4 + gx] } else { -1 };
    match (left >= 0, top >= 0) {
        (true, true) => (left + top + 1) >> 1,
        (true, false) => left,
        (false, true) => top,
        (false, false) => 0,
    }
}

fn set_luma_nz(nz: &mut [i32], lw4: usize, mbx: usize, mby: usize, blk: usize, tc: i32) {
    let (px, py) = blk_frame_xy(mbx, mby, blk);
    nz[(py / 4) * lw4 + (px / 4)] = tc;
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    // Forward (B/U) then inverse (U/B) must recover the residual within
    // quantization error. This isolates fwd_core/inv_core/quant/dequant from
    // CAVLC, prediction, and the DC Hadamard.
    #[test]
    fn core_transform_roundtrips_at_low_qp() {
        let qp = 8i32;
        let shift = qp / 6;
        let rem = (qp % 6) as usize;
        let qbits = 15 + qp / 6;
        let f = (1i64 << qbits) / 3;
        // a non-trivial residual (values a real intra residual might have)
        let resid: [i32; 16] = [
            12, -5, 3, 1, -8, 7, -2, 0, 4, -3, 6, -1, 2, 0, -4, 5,
        ];
        let w = fwd_core(&resid);
        // forward quant (all positions, class-dependent)
        let mut z = [0i32; 16];
        for i in 0..16 {
            let cl = pos_class(i);
            let mf = MF[rem][cl] as i64;
            let wv = w[i] as i64;
            let zz = ((wv.abs() * mf + f) >> qbits) as i32;
            z[i] = if wv < 0 { -zz } else { zz };
        }
        // dequant (the formula encode() uses for AC, applied to all positions)
        let mut wq = [0i32; 16];
        for i in 0..16 {
            let cl = pos_class(i);
            let vv = V[rem][cl];
            if shift >= 4 {
                wq[i] = (z[i] * vv) << shift;
            } else {
                wq[i] = (z[i] * vv * 16 + (1 << (3 - shift))) >> (4 - shift);
            }
        }
        let back = inv_core(&wq);
        let mut maxerr = 0i32;
        for i in 0..16 {
            maxerr = maxerr.max((back[i] - resid[i]).abs());
        }
        eprintln!("residual: {:?}", resid);
        eprintln!("recon:    {:?}", back);
        eprintln!("max abs error at QP{qp}: {maxerr}");
        assert!(maxerr <= 3, "core transform round-trip error {maxerr} too large (QP{qp})");
    }
}

#[cfg(test)]
mod dc_roundtrip {
    use super::*;
    // The luma-DC Hadamard path must recover the 16 block-DC values (the W[0]
    // of each block) within quant error — i.e. dc_recon ≈ dc_vals, so that
    // inv_core reproduces each block's DC. This mirrors the encode() DC code.
    #[test]
    fn dc_hadamard_roundtrips() {
        let qp = 16i32; // shift = 2 (< 6 path)
        let shift = qp / 6;
        let rem = (qp % 6) as usize;
        let qbits = 15 + qp / 6;
        // 16 block DCs (as W[0] would be: multiples of 16, varied like a gradient)
        let dc_vals: [i32; 16] = [
            160, 320, 480, 640, 320, 480, 640, 800, 480, 640, 800, 960, 640, 800, 960, 1120,
        ];
        // forward: Hadamard then DC quant (encode's formula) — fq = qbits+4
        let dc_had = hadamard4(&dc_vals);
        let fq = qbits + 4;
        let f = (1i64 << fq) / 3;
        let mut dc_z = [0i32; 16];
        for i in 0..16 {
            let mf = MF[rem][0] as i64;
            let v = dc_had[i] as i64;
            let zz = ((v.abs() * mf + f) >> fq) as i32;
            dc_z[i] = if v < 0 { -zz } else { zz };
        }
        // inverse: Hadamard then DC dequant (spec §8.5.10, 16·V)
        let dc_deq = hadamard4(&dc_z);
        let mut dc_recon = [0i32; 16];
        for i in 0..16 {
            let vv = V[rem][0];
            if shift >= 6 {
                dc_recon[i] = (dc_deq[i] * vv) << (shift - 2);
            } else {
                dc_recon[i] = (dc_deq[i] * vv * 16 + (1 << (5 - shift))) >> (6 - shift);
            }
        }
        let mut maxerr = 0i32;
        for i in 0..16 {
            maxerr = maxerr.max((dc_recon[i] - dc_vals[i]).abs());
        }
        eprintln!("dc_vals:  {:?}", dc_vals);
        eprintln!("dc_recon: {:?}", dc_recon);
        eprintln!("DC max abs error at QP{qp}: {maxerr}  (should be ~quant step, not ~values)");
        assert!(maxerr < 100, "DC Hadamard round-trip error {maxerr} — scaling bug");
    }
}

#[cfg(test)]
mod cavlc_fuzz {
    use super::*;
    struct Br<'a> { d: &'a [u8], pos: usize }
    impl<'a> Br<'a> {
        fn bit(&mut self) -> u32 { let b=(self.d[self.pos/8]>>(7-(self.pos%8)))&1; self.pos+=1; b as u32 }
        fn bits(&mut self, n: u32) -> u32 { let mut v=0; for _ in 0..n { v=(v<<1)|self.bit(); } v }
    }
    fn vlc(br:&mut Br, t:&[(u8,u16)])->usize{
        let mut code=0u32; let mut len=0u8;
        for _ in 0..16 { code=(code<<1)|br.bit(); len+=1;
            for (s,&(l,c)) in t.iter().enumerate(){ if l!=0 && l==len && c as u32==code {return s;} } }
        panic!("vlc decode fail");
    }
    fn dec(bytes:&[u8], nc:i32, max:usize)->Vec<i32>{
        let mut br=Br{d:bytes,pos:0};
        let (mut tc, mut t1)=(0usize,0usize);
        { let mut code=0u32; let mut len=0u8; 'o: for _ in 0..16 { code=(code<<1)|br.bit(); len+=1;
            for c in 0..=16usize { for o in 0..=c.min(3) {
                if nc<0 && c>4 {continue}
                let (l,cc)=coeff_token(c,o,nc);
                if l==len as u32 && cc==code { tc=c; t1=o; break 'o; } } } } }
        if tc==0 { return vec![0;max]; }
        let mut lv=vec![0i32;tc];
        for j in 0..t1 { lv[j]= if br.bit()==1 {-1}else{1}; }
        let mut sl:u32= if tc>10 && t1<3 {1}else{0};
        for j in t1..tc {
            let mut lp=0u32; while br.bit()==0 {lp+=1;}
            let mut code;
            if sl==0 { if lp<14 {code=lp as i32;} else if lp==14 {code=14+br.bits(4)as i32;} else {code=30+br.bits(12)as i32;} }
            else { code=((lp<<sl)|br.bits(sl)) as i32; }
            if j==t1 && t1<3 { code+=2; }
            lv[j]= if code%2==0 {(code+2)>>1} else {-((code+1)>>1)};
            if sl==0 {sl=1;}
            if lv[j].unsigned_abs() > (3u32<<(sl-1)) && sl<6 { sl+=1; }
        }
        let tz= if tc<max {
            let tbl:Vec<(u8,u16)>=(0..=(max-tc)).map(|z|{let (l,c)= if max==4 {total_zeros_chroma_dc(tc,z as i32)} else {total_zeros_4x4(tc,z as i32)}; (l as u8,c as u16)}).collect();
            vlc(&mut br,&tbl) as i32 } else {0};
        let mut zl=tz; let mut runs=vec![0i32;tc];
        for j in 0..tc-1 { if zl<=0 {break;}
            let tbl:Vec<(u8,u16)>=(0..=zl).map(|r|{let (l,c)=run_before(r,zl); (l as u8,c as u16)}).collect();
            runs[j]=vlc(&mut br,&tbl) as i32; zl-=runs[j]; }
        runs[tc-1]=zl;
        let mut pos=vec![0i32;tc]; let mut acc=0i32;
        for i in (0..tc).rev(){ acc+=runs[i]+1; pos[i]=acc-1; }
        let mut out=vec![0i32;max];
        for i in 0..tc { out[pos[i] as usize]=lv[i]; }
        out
    }
    #[test]
    fn block_roundtrips() {
        let mut st=0xBADC0FFEu64;
        let mut rng=|| { st=st.wrapping_mul(6364136223846793005).wrapping_add(1); (st>>33) as u32 };
        for trial in 0..200000u32 {
            let max= match trial%3 {0=>4,1=>15,_=>16};
            let nc= (rng()%9) as i32;
            let mut coeffs=vec![0i32;max];
            let k=(rng() as usize)%(max+1);
            for _ in 0..k { let p=(rng() as usize)%max; let m=1+(rng()%15) as i32; coeffs[p]= if rng()&1==1 {m}else{-m}; }
            let mut bw=BitWriter::new();
            { let mut c=Cavlc{bw:&mut bw}; c.block(&coeffs,nc); }
            bw.put_bit(1); bw.align_zero();
            let got=dec(&bw.into_rbsp(),nc,max);
            assert_eq!(got,coeffs,"trial {trial} nc={nc} max={max}");
        }
    }
}

#[cfg(test)]
mod chroma_dc_unit {
    use super::*;
    fn shift(x: i64, s: i32) -> i64 { if s <= 0 { x << (-s) } else { (x + (1 << (s - 1))) >> s } }
    fn chroma_dc_qdq(dc: [i32;4], qpc: i32) -> [i32;4] {
        let rem=(qpc%6) as usize;
        let q4mf=shift(MF[rem][0] as i64, qpc/6 - 1);
        let mf=q4mf>>1; let bias=((1i64<<15)/q4mf)<<1;
        let had=hadamard2(&dc);
        let mut z=[0i32;4];
        for i in 0..4 { let d=had[i] as i64; let q=((bias+d.abs())*mf)>>16; z[i]= if d<0 {-(q as i32)} else {q as i32}; }
        let deq=hadamard2(&z);
        let dmf=(V[rem][0] as i64)<<(qpc/6);
        let mut r=[0i32;4]; for i in 0..4 { r[i]=((deq[i] as i64*dmf)>>5) as i32; }
        r
    }
    #[test]
    fn matches_x264_golden() {
        // hand-traced verbatim from x264 encoder/macroblock.c at qpc=6
        assert_eq!(chroma_dc_qdq([120,-80,40,-16], 6), [28,-20,10,-4]);
        // flat DC in → uniform DC out (inverse Hadamard of a single DC coeff)
        let r = chroma_dc_qdq([100,100,100,100], 6);
        assert!(r.iter().all(|&v| (v - r[0]).abs() <= 1), "flat in → flat out, got {r:?}");
        assert!(r[0] > 0);
    }
}
