// ═══════════════════════════════════════════════════════════════════════════
// h264bits.rs — shared H.264 primitives: bit writer, NAL, SPS/PPS, color
// ═══════════════════════════════════════════════════════════════════════════
// Extracted so both the rung-1 I_PCM path (h264.rs) and the rung-2 intra path
// (h264_intra.rs) share one correct implementation of Exp-Golomb, NAL framing,
// and the parameter sets.
// ═══════════════════════════════════════════════════════════════════════════

/// MSB-first bit writer with Exp-Golomb + byte alignment (RBSP builder).
pub struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter { buf: Vec::new(), cur: 0, nbits: 0 }
    }
    pub fn put_bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | ((b & 1) as u8);
        self.nbits += 1;
        if self.nbits == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }
    pub fn put_bits(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((val >> i) & 1);
        }
    }
    pub fn put_flag(&mut self, f: bool) {
        self.put_bit(f as u32);
    }
    pub fn ue(&mut self, v: u32) {
        let code = v + 1;
        let nbits = 32 - code.leading_zeros();
        for _ in 0..(nbits - 1) {
            self.put_bit(0);
        }
        self.put_bits(code, nbits);
    }
    pub fn se(&mut self, v: i32) {
        let mapped = if v <= 0 { (-2 * v) as u32 } else { (2 * v - 1) as u32 };
        self.ue(mapped);
    }
    pub fn byte_aligned(&self) -> bool {
        self.nbits == 0
    }
    pub fn align_zero(&mut self) {
        while self.nbits != 0 {
            self.put_bit(0);
        }
    }
    pub fn put_byte(&mut self, b: u8) {
        debug_assert!(self.byte_aligned(), "put_byte while not aligned");
        self.buf.push(b);
    }
    pub fn trailing_bits(&mut self) {
        self.put_bit(1);
        self.align_zero();
    }
    pub fn into_rbsp(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.buf.push(self.cur);
            self.nbits = 0;
        }
        self.buf
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a NAL unit: header byte + RBSP with emulation prevention, NO start
/// code. Used for AVCC/MP4 (length-prefixed) and as the body of Annex-B NALs.
pub fn nal_unit(nal_ref_idc: u8, nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
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

/// Emit a NAL in Annex-B form: 4-byte start code + the NAL unit.
pub fn emit_nal(out: &mut Vec<u8>, nal_ref_idc: u8, nal_unit_type: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&nal_unit(nal_ref_idc, nal_unit_type, rbsp));
}

pub fn build_sps(width: u32, height: u32) -> Vec<u8> {
    let mbw = (width + 15) / 16;
    let mbh = (height + 15) / 16;
    let mut w = BitWriter::new();
    w.put_bits(66, 8); // profile_idc = Baseline
    w.put_bits(0, 8); // constraint flags
    w.put_bits(30, 8); // level_idc 3.0
    w.ue(0); // sps id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(0); // pic_order_cnt_type
    w.ue(0); // log2_max_poc_lsb_minus4
    w.ue(1); // max_num_ref_frames
    w.put_flag(false); // gaps_in_frame_num
    w.ue(mbw - 1);
    w.ue(mbh - 1);
    w.put_flag(true); // frame_mbs_only
    w.put_flag(false); // direct_8x8
    let crop_r = (mbw * 16 - width) / 2;
    let crop_b = (mbh * 16 - height) / 2;
    if crop_r != 0 || crop_b != 0 {
        w.put_flag(true);
        w.ue(0);
        w.ue(crop_r);
        w.ue(0);
        w.ue(crop_b);
    } else {
        w.put_flag(false);
    }
    w.put_flag(false); // vui
    w.trailing_bits();
    w.into_rbsp()
}

pub fn build_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pps id
    w.ue(0); // sps id
    w.put_flag(false); // CAVLC
    w.put_flag(false); // bottom_field_pic_order
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.put_flag(false); // weighted_pred
    w.put_bits(0, 2); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.put_flag(true); // deblocking_filter_control_present (so slice can disable it)
    w.put_flag(false); // constrained_intra_pred
    w.put_flag(false); // redundant_pic_cnt_present
    w.trailing_bits();
    w.into_rbsp()
}

#[inline]
pub fn clamp8(v: f32) -> u8 {
    if v < 0.0 {
        0
    } else if v > 255.0 {
        255
    } else {
        (v + 0.5) as u8
    }
}

#[inline]
pub fn rgb_to_ycbcr601_limited(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
    let y = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
    let cb = -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
    let cr = 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
    (clamp8(y), clamp8(cb), clamp8(cr))
}
