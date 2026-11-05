// ═══════════════════════════════════════════════════════════════════════════
// h264.rs — sovereign H.264 encoder, rung 1: valid Annex-B IDR via I_PCM
// ═══════════════════════════════════════════════════════════════════════════
// Produces a spec-conformant H.264 (AVC) Annex-B elementary stream that ffmpeg
// decodes: SPS + PPS + one IDR I-slice per frame. This first rung uses I_PCM
// macroblocks — raw uncompressed samples — so it is NOT compression yet (an
// I_PCM frame is larger than raw). What it proves is the hard scaffolding that
// every later rung hangs on, bit-exactly: NAL framing, Exp-Golomb, the SPS/PPS
// parameter sets, the IDR slice header, the macroblock layer, and RBSP
// emulation-prevention.
//
// RUNG MAP (honest):
//   rung 1 (this file): NAL/SPS/PPS/slice/MB framing via I_PCM        ← here
//   rung 2: 16×16 + 4×4 intra prediction, 4×4 integer transform, quant
//   rung 3: CAVLC residual coding (coeff_token / total_zeros / run_before)
//   rung 4: deblocking filter
//   rung 5+: P-slices — motion estimation + quarter-pel compensation
//
// There is no `x = B/U` shortcut for rungs 2/5: the integer transform is a
// real linear transform (factorable, like the DCT) but quantization is the
// lossy, non-invertible step (`BeyondTheThrone.zero_not_being`), and motion
// search is a data-dependent minimisation. Confirmed by reading the proofs.
//
// Baseline profile (profile_idc 66), 4:2:0, CAVLC, frame_mbs_only. Color is
// limited-range BT.601 to match a decoder's default (no-VUI) interpretation.
// ═══════════════════════════════════════════════════════════════════════════

// ─────────────────────────────────────────────────────────────────────────
// MSB-first bit writer with Exp-Golomb + byte alignment (RBSP builder)
// ─────────────────────────────────────────────────────────────────────────

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
    fn put_bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | ((b & 1) as u8);
        self.nbits += 1;
        if self.nbits == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }
    fn put_bits(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((val >> i) & 1);
        }
    }
    fn put_flag(&mut self, f: bool) {
        self.put_bit(f as u32);
    }
    /// ue(v): unsigned Exp-Golomb.
    fn ue(&mut self, v: u32) {
        let code = v + 1;
        let nbits = 32 - code.leading_zeros(); // # of significant bits
                                               // (nbits - 1) leading zeros, then `code` in nbits bits
        for _ in 0..(nbits - 1) {
            self.put_bit(0);
        }
        self.put_bits(code, nbits);
    }
    /// se(v): signed Exp-Golomb.
    fn se(&mut self, v: i32) {
        let mapped = if v <= 0 {
            (-2 * v) as u32
        } else {
            (2 * v - 1) as u32
        };
        self.ue(mapped);
    }
    fn byte_aligned(&self) -> bool {
        self.nbits == 0
    }
    /// pcm_alignment_zero_bit padding up to a byte boundary.
    fn align_zero(&mut self) {
        while self.nbits != 0 {
            self.put_bit(0);
        }
    }
    /// Append a raw byte; must be byte-aligned (used for PCM samples).
    fn put_byte(&mut self, b: u8) {
        debug_assert!(self.byte_aligned(), "put_byte while not aligned");
        self.buf.push(b);
    }
    /// rbsp_trailing_bits(): a stop-one bit then zero padding.
    fn trailing_bits(&mut self) {
        self.put_bit(1);
        self.align_zero();
    }
    fn into_rbsp(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            // shouldn't happen after trailing_bits, but flush defensively
            self.cur <<= 8 - self.nbits;
            self.buf.push(self.cur);
            self.nbits = 0;
        }
        self.buf
    }
}

/// Build a NAL unit: header byte + emulation-prevented RBSP, NO start code
/// (for AVCC/MP4 length-prefixed framing and as the Annex-B NAL body).
fn nal_unit(nal_ref_idc: u8, nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 1);
    out.push(((nal_ref_idc & 3) << 5) | (nal_unit_type & 0x1F));
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// Wrap an RBSP in an Annex-B NAL: 4-byte start code + NAL unit.
fn emit_nal(out: &mut Vec<u8>, nal_ref_idc: u8, nal_unit_type: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&nal_unit(nal_ref_idc, nal_unit_type, rbsp));
}

/// Encode RGBA frames for MP4 muxing: returns (SPS NAL, PPS NAL, per-frame IDR
/// NAL units) — all without start codes, ready for AVCC length-prefixing.
/// Default quantiser for the compressed intra path (SliceQP = 26).
pub const DEFAULT_QP: i32 = 26;

/// Build an MP4 video track: returns (SPS NAL, PPS NAL, per-frame IDR NAL
/// samples). Uses the **compressed** adaptive intra encoder (per-MB I_16x16 /
/// I_4x4 free-energy decision) at `DEFAULT_QP`. The SPS/PPS are the
/// `h264bits` parameter sets the intra slices are validated against (PPS has
/// `deblocking_filter_control_present` so the slice can disable the loop
/// filter for decoder-exact reconstruction).
pub fn encode_mp4_track(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>), String> {
    encode_mp4_track_qp(frames, width, height, DEFAULT_QP)
}

/// As [`encode_mp4_track`] but with an explicit quantiser (lower = higher
/// quality + larger). `qp` is the H.264 SliceQP (0..51).
pub fn encode_mp4_track_qp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    qp: i32,
) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>), String> {
    if frames.is_empty() {
        return Err("H264: no frames".into());
    }
    if width == 0 || height == 0 {
        return Err("H264: zero dimension".into());
    }
    let need = (width * height * 4) as usize;
    let sps = nal_unit(3, 7, &crate::h264bits::build_sps(width, height));
    let pps = nal_unit(3, 8, &crate::h264bits::build_pps());
    let mut samples = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        if f.len() < need {
            return Err(format!("H264: frame {i} buffer too small"));
        }
        let slice = crate::h264_intra::encode_idr_adaptive(width, height, f, qp, i as u32)?;
        samples.push(nal_unit(3, 5, &slice));
    }
    Ok((sps, pps, samples))
}

/// Lossless I_PCM track (every MB stored raw, no transform/quant). Larger than
/// any compressed mode but exactly reconstructs the source — kept for the
/// archival/verification use case. Returns (SPS, PPS, samples).
pub fn encode_mp4_track_lossless(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>), String> {
    if frames.is_empty() {
        return Err("H264: no frames".into());
    }
    if width == 0 || height == 0 {
        return Err("H264: zero dimension".into());
    }
    let need = (width * height * 4) as usize;
    let sps = nal_unit(3, 7, &build_sps(width, height));
    let pps = nal_unit(3, 8, &build_pps());
    let mut samples = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        if f.len() < need {
            return Err(format!("H264: frame {i} buffer too small"));
        }
        let slice = build_idr_slice(width, height, f, i as u32);
        samples.push(nal_unit(3, 5, &slice));
    }
    Ok((sps, pps, samples))
}

// ─────────────────────────────────────────────────────────────────────────
// Parameter sets
// ─────────────────────────────────────────────────────────────────────────

fn build_sps(width: u32, height: u32) -> Vec<u8> {
    let mbw = (width + 15) / 16;
    let mbh = (height + 15) / 16;
    let mut w = BitWriter::new();
    w.put_bits(66, 8); // profile_idc = Baseline
    w.put_bits(0, 8); // constraint_set flags + reserved
    w.put_bits(30, 8); // level_idc = 3.0
    w.ue(0); // seq_parameter_set_id
             // (baseline: no chroma_format_idc / bit_depth fields)
    w.ue(0); // log2_max_frame_num_minus4  → frame_num is 4 bits
    w.ue(0); // pic_order_cnt_type = 0
    w.ue(0); // log2_max_pic_order_cnt_lsb_minus4 → poc_lsb is 4 bits
    w.ue(1); // max_num_ref_frames
    w.put_flag(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(mbw - 1); // pic_width_in_mbs_minus1
    w.ue(mbh - 1); // pic_height_in_map_units_minus1
    w.put_flag(true); // frame_mbs_only_flag
    w.put_flag(false); // direct_8x8_inference_flag
                       // frame cropping for non-multiple-of-16 dimensions (4:2:0 → CropUnit 2×2)
    let crop_r = (mbw * 16 - width) / 2;
    let crop_b = (mbh * 16 - height) / 2;
    if crop_r != 0 || crop_b != 0 {
        w.put_flag(true); // frame_cropping_flag
        w.ue(0); // left
        w.ue(crop_r); // right
        w.ue(0); // top
        w.ue(crop_b); // bottom
    } else {
        w.put_flag(false);
    }
    w.put_flag(false); // vui_parameters_present_flag
    w.trailing_bits();
    w.into_rbsp()
}

fn build_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.put_flag(false); // entropy_coding_mode_flag = 0 (CAVLC)
    w.put_flag(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.put_flag(false); // weighted_pred_flag
    w.put_bits(0, 2); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.put_flag(false); // deblocking_filter_control_present_flag
    w.put_flag(false); // constrained_intra_pred_flag
    w.put_flag(false); // redundant_pic_cnt_present_flag
    w.trailing_bits();
    w.into_rbsp()
}

// ─────────────────────────────────────────────────────────────────────────
// Color: limited-range BT.601 (matches a no-VUI decoder's default)
// ─────────────────────────────────────────────────────────────────────────

#[inline]
fn clamp8(v: f32) -> u8 {
    if v < 0.0 {
        0
    } else if v > 255.0 {
        255
    } else {
        (v + 0.5) as u8
    }
}

#[inline]
fn rgb_to_ycbcr601_limited(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
    let y = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
    let cb = -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
    let cr = 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
    (clamp8(y), clamp8(cb), clamp8(cr))
}

// ─────────────────────────────────────────────────────────────────────────
// IDR slice with I_PCM macroblocks
// ─────────────────────────────────────────────────────────────────────────

/// Build one IDR slice RBSP for the given frame (RGBA8 → 4:2:0 I_PCM).
fn build_idr_slice(width: u32, height: u32, rgba: &[u8], idr_id: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mbw = ((width + 15) / 16) as usize;
    let mbh = ((height + 15) / 16) as usize;
    let pw = mbw * 16;
    let ph = mbh * 16;

    // Padded full-res Y/Cb/Cr planes (edge-replicated).
    let mut yp = vec![0u8; pw * ph];
    let mut cbp = vec![0u8; pw * ph];
    let mut crp = vec![0u8; pw * ph];
    for y in 0..ph {
        for x in 0..pw {
            let sx = x.min(w - 1);
            let sy = y.min(h - 1);
            let i = (sy * w + sx) * 4;
            let (yy, cb, cr) =
                rgb_to_ycbcr601_limited(rgba[i] as f32, rgba[i + 1] as f32, rgba[i + 2] as f32);
            yp[y * pw + x] = yy;
            cbp[y * pw + x] = cb;
            crp[y * pw + x] = cr;
        }
    }

    let mut bw = BitWriter::new();
    // slice_header
    bw.ue(0); // first_mb_in_slice
    bw.ue(7); // slice_type = I (all slices in pic are I)
    bw.ue(0); // pic_parameter_set_id
    bw.put_bits(0, 4); // frame_num (4 bits; IDR → 0)
    bw.ue(idr_id); // idr_pic_id
    bw.put_bits(0, 4); // pic_order_cnt_lsb (4 bits)
                       // dec_ref_pic_marking (IDR): no_output_of_prior_pics_flag, long_term_reference_flag
    bw.put_flag(false);
    bw.put_flag(false);
    bw.se(0); // slice_qp_delta (QP=26; irrelevant for I_PCM)

    // slice_data: every MB is I_PCM (mb_type 25 in an I-slice).
    for mby in 0..mbh {
        for mbx in 0..mbw {
            bw.ue(25); // mb_type = I_PCM
            bw.align_zero(); // pcm_alignment_zero_bit(s)
                             // luma 16×16
            for j in 0..16 {
                for i in 0..16 {
                    bw.put_byte(yp[(mby * 16 + j) * pw + (mbx * 16 + i)]);
                }
            }
            // chroma 4:2:0: Cb 8×8 then Cr 8×8 (each = average of 2×2 luma-grid)
            for plane in [&cbp, &crp] {
                for cj in 0..8 {
                    for ci in 0..8 {
                        let x0 = mbx * 16 + ci * 2;
                        let y0 = mby * 16 + cj * 2;
                        let s = plane[y0 * pw + x0] as u32
                            + plane[y0 * pw + x0 + 1] as u32
                            + plane[(y0 + 1) * pw + x0] as u32
                            + plane[(y0 + 1) * pw + x0 + 1] as u32;
                        bw.put_byte(((s + 2) / 4) as u8);
                    }
                }
            }
        }
    }
    bw.trailing_bits();
    bw.into_rbsp()
}

/// Encode a single RGBA8 frame to a compressed H.264 Annex-B stream
/// (SPS + PPS + 1 IDR) at [`DEFAULT_QP`].
pub fn encode_frame(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    encode_sequence(std::slice::from_ref(&rgba.to_vec()), width, height)
}

/// Encode a sequence of RGBA8 frames as a **compressed** intra-only (all-IDR)
/// H.264 Annex-B stream at [`DEFAULT_QP`], using the adaptive per-MB
/// I_16x16/I_4x4 encoder. `frames[i]` is `width*height*4` bytes.
pub fn encode_sequence(frames: &[Vec<u8>], width: u32, height: u32) -> Result<Vec<u8>, String> {
    encode_sequence_qp(frames, width, height, DEFAULT_QP)
}

/// As [`encode_sequence`] with an explicit quantiser.
pub fn encode_sequence_qp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    qp: i32,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("H264: zero dimension".into());
    }
    crate::h264_intra::encode_sequence_adaptive_with_recon(frames, width, height, qp)
        .map(|(s, _)| s)
}

/// Lossless I_PCM Annex-B stream (every MB stored raw). Exactly reconstructs
/// the source; far larger than the compressed path.
pub fn encode_sequence_lossless(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("H264: no frames".into());
    }
    if width == 0 || height == 0 {
        return Err("H264: zero dimension".into());
    }
    let need = (width * height * 4) as usize;
    let mut out = Vec::new();
    emit_nal(&mut out, 3, 7, &build_sps(width, height)); // SPS
    emit_nal(&mut out, 3, 8, &build_pps()); // PPS
    for (i, f) in frames.iter().enumerate() {
        if f.len() < need {
            return Err(format!("H264: frame {i} buffer too small"));
        }
        let slice = build_idr_slice(width, height, f, i as u32);
        emit_nal(&mut out, 3, 5, &slice); // IDR slice
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_ue_known() {
        // ue: 0→"1", 1→"010", 2→"011", 3→"00100", 4→"00101"
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(1);
        w.ue(2);
        // bits so far: 1 010 011 = 1010011 (7 bits) → padded
        w.align_zero();
        let b = w.into_rbsp();
        assert_eq!(b[0], 0b1010_0110); // 1 010 011 + one pad zero
    }

    #[test]
    fn annexb_has_three_nals() {
        let (w, h) = (32u32, 16u32);
        let frame = vec![128u8; (w * h * 4) as usize];
        let stream = encode_frame(w, h, &frame).expect("encode");
        // count start codes 00 00 00 01
        let starts = stream.windows(4).filter(|x| *x == [0, 0, 0, 1]).count();
        assert_eq!(starts, 3, "expected SPS+PPS+IDR start codes");
        // first NAL after start code is SPS (type 7)
        assert_eq!(stream[4] & 0x1F, 7);
    }

    #[test]
    fn emulation_prevention_inserted() {
        // an RBSP with 00 00 00 must get a 0x03 inserted
        let mut out = Vec::new();
        emit_nal(&mut out, 0, 1, &[0, 0, 0, 1]);
        // after 4-byte start + 1 header byte: 00 00 03 00 01
        let body = &out[5..];
        assert_eq!(body, &[0, 0, 3, 0, 1]);
    }
}
