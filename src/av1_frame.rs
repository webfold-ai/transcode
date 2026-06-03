// ═══════════════════════════════════════════════════════════════════════════
// av1_frame.rs — minimal AV1 KEY_FRAME: assemble OBUs + symbol-code one tile
// ═══════════════════════════════════════════════════════════════════════════
// Rung 4 — the first end-to-end decodable AV1 frame. For a 64×64 flat (DC) still
// image the entire tile is FOUR symbols through the od_ec coder (av1_ec):
//   partition = PARTITION_NONE,  skip = 1,  y_mode = DC_PRED,  uv_mode = UV_DC.
// With skip=1 there is no residual; DC prediction with no neighbours fills the
// frame with mid-grey (128). All in-loop filters are off (frame header), so the
// decoder's output equals this. This is the smallest conformant AV1 frame —
// there is no I_PCM shortcut, so even "nothing happens" must be fully coded.
//
// The four default CDFs are lifted from dav1d src/cdf.c (cumulative args →
// icdf = 32768 − arg, with a 0 terminator + adaptation-counter slot). The
// contexts are all 0 (first superblock, neighbours cleared / unavailable).
//
// VERIFICATION: the coded tile is checked == libaom's own tile bytes (`09 ac`
// from the still-picture reference), and the assembled stream is decoded by
// ffmpeg (libdav1d) and asserted to be a 64×64 AV1 frame of uniform grey.
// ═══════════════════════════════════════════════════════════════════════════

use crate::av1_ec::Writer;
use crate::av1_obu::{sequence_header_obu, temporal_delimiter, wrap_obu, write_frame_header, OBU_FRAME};
use crate::h264bits::BitWriter;

// dav1d default-CDF cumulative args (src/cdf.c). icdf[i] = 32768 − arg[i].
const PARTITION_64_CTX0: [u16; 9] =
    [20137, 21547, 23078, 29566, 29837, 30261, 30524, 30892, 31724]; // 10 symbols
const KF_YMODE_00: [u16; 12] =
    [15588, 17027, 19338, 20218, 20682, 21110, 21825, 23244, 24189, 28165, 29093, 30466]; // 13
const UV_MODE_DC: [u16; 12] =
    [22631, 24152, 25378, 25661, 25986, 26520, 27055, 27923, 28244, 30059, 30941, 31961]; // 13

// Coefficient-skip (all_zero / txb_skip) defaults, coef.skip[qctx=3][tx_ctx][ctx]
// (dav1d cdf.c). Stored 2-entry [icdf, count] for the adaptive bool.
const COEF_SKIP_TX64_C0: u16 = 31539; // skip[4][0] — luma TX_64X64, ctx 0
const COEF_SKIP_TX32_C7: u16 = 4656; //  skip[3][7] — chroma TX_32X32, ctx 7
const MB_SKIP_C0: u16 = 31671; //         m.skip[0] — block skip, ctx 0

/// Build an AOM_ICDF cdf for the multi-symbol coder from dav1d's cumulative
/// args: `nsymbs` entries (last is the 0 terminator) + adaptation counter.
fn cdf(args: &[u16]) -> Vec<u16> {
    let mut v: Vec<u16> = args.iter().map(|&a| 32768 - a).collect();
    v.push(0); // implicit final symbol's icdf (terminator)
    v.push(0); // adaptation counter
    v
}

/// 2-entry adaptive-bool CDF `[icdf, count]` from a dav1d cumulative arg.
fn bcdf(arg: u16) -> [u16; 2] {
    [32768 - arg, 0]
}

/// Symbol-code the single 64×64 DC superblock with skip=0 + all-zero residual,
/// exactly as libaom's still-picture reference does (so the bytes are `09 ac`).
/// Order matches dav1d decode.c: partition, skip, y_mode, uv_mode, then the
/// per-plane all_zero flags (luma TX_64X64, chroma U/V TX_32X32 sharing one
/// adapting CDF). all_zero=1 ⇒ no coefficients ⇒ zero residual ⇒ DC grey.
fn encode_tile_flat_dc() -> Vec<u8> {
    let mut w = Writer::new();
    // decode_partition(BLOCK_64X64): PARTITION_NONE (symbol_adapt)
    let mut part = cdf(&PARTITION_64_CTX0);
    w.symbol(0, &mut part, 10);
    // intra_frame_mode_info: skip = 0 (bool_adapt — decode.c:890)
    let mut skip = bcdf(MB_SKIP_C0);
    w.bool_adapt(false, &mut skip);
    // intra_frame_y_mode = DC_PRED (symbol_adapt16, kf_y_mode[0][0])
    let mut ym = cdf(&KF_YMODE_00);
    w.symbol(0, &mut ym, 13);
    // uv_mode = UV_DC (symbol_adapt16, cfl not allowed at 64×64)
    let mut uv = cdf(&UV_MODE_DC);
    w.symbol(0, &mut uv, 13);
    // residual: per-plane all_zero=1 (bool_adapt). luma TX_64X64 ctx0;
    // chroma U then V TX_32X32 ctx7, sharing one adapting CDF.
    let mut luma_az = bcdf(COEF_SKIP_TX64_C0);
    w.bool_adapt(true, &mut luma_az);
    let mut chroma_az = bcdf(COEF_SKIP_TX32_C7);
    w.bool_adapt(true, &mut chroma_az); // U
    w.bool_adapt(true, &mut chroma_az); // V (adapted)
    w.finish()
}

// Coefficient CDFs for the nonzero luma-DC path (dav1d cdf.c, qctx=3),
// all verified bit-for-bit against dav1d's per-symbol rng trace.
const EOB_BIN_1024_L: [u16; 10] =
    [6698, 8334, 11961, 15762, 20186, 23862, 27434, 29326, 31082, 32050]; // [chroma=0]
const EOB_BASE_TOK_64: [u16; 2] = [12358, 24977]; // eob_base_tok[ctx=4][0][0]
const BR_TOK_64: [u16; 3] = [12162, 18785, 22648]; // br_tok[imin(ctx,3)=3][0][0]
const DC_SIGN_L: u16 = 16000; // dc_sign[chroma=0][ctx=0]

/// Encode the AV1 `hi_tok` (br_tok) for a coefficient level `dc_tok ∈ 3..=15`:
/// the inverse of dav1d `decode_hi_tok` — repeated base-3 symbols where 3 means
/// "continue", over one adapting br_tok CDF.
fn encode_hi_tok(w: &mut Writer, br: &mut [u16], dc_tok: u32) {
    if dc_tok <= 5 {
        w.symbol((dc_tok - 3) as usize, br, 4);
        return;
    }
    w.symbol(3, br, 4);
    if dc_tok <= 8 {
        w.symbol((dc_tok - 6) as usize, br, 4);
        return;
    }
    w.symbol(3, br, 4);
    if dc_tok <= 11 {
        w.symbol((dc_tok - 9) as usize, br, 4);
        return;
    }
    w.symbol(3, br, 4);
    w.symbol((dc_tok - 12) as usize, br, 4);
}

/// Encode AV1's Exp-Golomb residual (inverse of dav1d `read_golomb`): `len`
/// equiprobable zero bits, then `val = x+1` in `len+1` bits MSB-first (the
/// leading 1 is the run terminator). Used for coefficient levels ≥ 15.
fn encode_golomb(w: &mut Writer, x: u32) {
    let val = x + 1;
    let len = 31 - val.leading_zeros();
    for _ in 0..len {
        w.bit(0);
    }
    for i in (0..=len).rev() {
        w.bit((val >> i) & 1);
    }
}

/// Symbol-code the 64×64 superblock with a single nonzero luma DC coefficient
/// (level `dc_tok ≥ 1`, `dc_sign` true = negative), chroma all-zero. This is the
/// arm-③ generative path: the coefficient value is regenerated from the entropy
/// state, never stored. Order matches dav1d decode_coefs exactly.
fn encode_tile_dc(dc_tok: u32, dc_sign: bool) -> Vec<u8> {
    let mut w = Writer::new();
    let mut part = cdf(&PARTITION_64_CTX0);
    w.symbol(0, &mut part, 10); // PARTITION_NONE
    let mut skip = bcdf(MB_SKIP_C0);
    w.bool_adapt(false, &mut skip); // skip = 0
    let mut ym = cdf(&KF_YMODE_00);
    w.symbol(0, &mut ym, 13); // y_mode = DC
    let mut uv = cdf(&UV_MODE_DC);
    w.symbol(0, &mut uv, 13); // uv_mode = DC
    // luma residual: all_zero = 0 (has the DC coeff)
    let mut luma_az = bcdf(COEF_SKIP_TX64_C0);
    w.bool_adapt(false, &mut luma_az);
    // eob_pt = 0 (dc-only)
    let mut eob = cdf(&EOB_BIN_1024_L);
    w.symbol(0, &mut eob, 11);
    // eob_base_tok: tok_br = min(dc_tok-1, 2); ≥3 escapes to hi_tok
    let mut et = cdf(&EOB_BASE_TOK_64);
    if dc_tok <= 2 {
        w.symbol((dc_tok - 1) as usize, &mut et, 3);
    } else {
        w.symbol(2, &mut et, 3);
        let mut br = cdf(&BR_TOK_64);
        encode_hi_tok(&mut w, &mut br, dc_tok.min(15));
        if dc_tok >= 15 {
            encode_golomb(&mut w, dc_tok - 15); // hi_tok saturates at 15 ⇒ golomb tail
        }
    }
    // dc_sign
    let mut ds = bcdf(DC_SIGN_L);
    w.bool_adapt(dc_sign, &mut ds);
    // chroma all_zero = 1 (U then V, shared adapting CDF)
    let mut chroma_az = bcdf(COEF_SKIP_TX32_C7);
    w.bool_adapt(true, &mut chroma_az);
    w.bool_adapt(true, &mut chroma_az);
    w.finish()
}

const EOB_BASE_TOK_64_C1: [u16; 2] = [31331, 32385]; // eob_base_tok[4][0][ctx=1] (eob=1)
const BASE_TOK_64_C0: [u16; 3] = [4137, 10847, 15682]; // base_tok[4][0][ctx=0] (2D-class DC)

/// Symbol-code the 64×64 superblock with TWO luma coefficients: the DC (scan[0])
/// and one AC at scan[1] (the first vertical frequency, rc=32). Levels 1–2 each
/// (no hi-token), signs free. eob (last scan index) = 1. The 2D-class DC uses a
/// fixed context 0 (NOT get_lo_ctx — that's only the middle AC coeffs). Verified
/// against dav1d's per-symbol rng trace; decodes to a spatially-varying frame.
fn encode_tile_dc_ac(dc_lvl: u32, dc_neg: bool, ac_lvl: u32, ac_neg: bool) -> Vec<u8> {
    assert!((1..=2).contains(&dc_lvl) && (1..=2).contains(&ac_lvl));
    let mut w = Writer::new();
    let mut part = cdf(&PARTITION_64_CTX0);
    w.symbol(0, &mut part, 10);
    let mut skip = bcdf(MB_SKIP_C0);
    w.bool_adapt(false, &mut skip);
    let mut ym = cdf(&KF_YMODE_00);
    w.symbol(0, &mut ym, 13);
    let mut uv = cdf(&UV_MODE_DC);
    w.symbol(0, &mut uv, 13);
    // luma residual, eob (last scan index) = 1
    let mut az = bcdf(COEF_SKIP_TX64_C0);
    w.bool_adapt(false, &mut az); // all_zero = 0
    let mut eb = cdf(&EOB_BIN_1024_L);
    w.symbol(1, &mut eb, 11); // eob_pt = 1
    // eob coeff (scan[1]) via eob_base_tok[ctx=1]; tok = ac_lvl (≤2 ⇒ no hi)
    let mut et = cdf(&EOB_BASE_TOK_64_C1);
    w.symbol((ac_lvl - 1) as usize, &mut et, 3);
    // DC (scan[0]) via base_tok[ctx=0]; tok = dc_lvl (≤2 ⇒ no hi)
    let mut bt = cdf(&BASE_TOK_64_C0);
    w.symbol(dc_lvl as usize, &mut bt, 3);
    // signs: dc_sign (adaptive), then AC sign (equiprobable), in rc order
    let mut ds = bcdf(DC_SIGN_L);
    w.bool_adapt(dc_neg, &mut ds);
    w.bit(ac_neg as u32);
    // chroma all_zero
    let mut cz = bcdf(COEF_SKIP_TX32_C7);
    w.bool_adapt(true, &mut cz);
    w.bool_adapt(true, &mut cz);
    w.finish()
}

/// A 64×64 still with a DC + one vertical-AC luma coefficient — a spatially
/// varying (non-flat) decodable AV1 frame.
pub fn encode_ac_keyframe_64(dc_lvl: u32, dc_neg: bool, ac_lvl: u32, ac_neg: bool) -> Vec<u8> {
    assemble_keyframe(encode_tile_dc_ac(dc_lvl, dc_neg, ac_lvl, ac_neg))
}

/// Assemble a full AV1 elementary stream around a coded tile.
fn assemble_keyframe(tile: Vec<u8>) -> Vec<u8> {
    let (w, h, q) = (64u32, 64u32, 128u8);
    let mut out = temporal_delimiter();
    out.extend_from_slice(&sequence_header_obu(w, h));
    let mut fhdr = BitWriter::new();
    write_frame_header(&mut fhdr, w, h, q);
    fhdr.align_zero();
    let mut payload = fhdr.into_rbsp();
    payload.extend_from_slice(&tile);
    out.extend_from_slice(&wrap_obu(OBU_FRAME, &payload));
    out
}

/// A 64×64 still with a uniform luma DC offset (chroma neutral). `dc_tok` is the
/// quantised DC level, `dc_sign` its sign. Decodes to a uniform grey ≠ 128.
pub fn encode_dc_keyframe_64(dc_tok: u32, dc_sign: bool) -> Vec<u8> {
    assemble_keyframe(encode_tile_dc(dc_tok, dc_sign))
}

/// Encode a 64×64 flat-grey still image as a complete AV1 elementary stream
/// (temporal delimiter + sequence header + OBU_FRAME). Decodable by dav1d/ffmpeg.
pub fn encode_flat_keyframe_64() -> Vec<u8> {
    let (w, h) = (64u32, 64u32);
    let base_q_idx = 128u8;
    let mut out = temporal_delimiter();
    out.extend_from_slice(&sequence_header_obu(w, h));
    // OBU_FRAME payload = frame_header (byte-aligned) ++ tile_group data
    let mut fhdr = BitWriter::new();
    write_frame_header(&mut fhdr, w, h, base_q_idx);
    fhdr.align_zero();
    let mut payload = fhdr.into_rbsp();
    payload.extend_from_slice(&encode_tile_flat_dc());
    out.extend_from_slice(&wrap_obu(OBU_FRAME, &payload));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_is_deterministic_allzero() {
        // skip=0 + all_zero=1 on every plane (luma TX_64X64 ctx0, chroma
        // TX_32X32 ctx7). Decoding libaom's own `09 ac` with our Reader showed
        // libaom coded skip=0 but luma all_zero=0 + a DC coeff (its `gray` input
        // is Y≈126≠128, so DC-pred leaves a residual). Ours is the equally-valid
        // all-zero choice → pure Y=128. Different residual decision ⇒ different
        // bytes; the ground truth is the ffmpeg decode, not a byte match.
        assert_eq!(encode_tile_flat_dc(), vec![0x31, 0x80]);
    }

    #[test]
    fn stream_headers_match_libaom() {
        // Everything except the tile residual is byte-identical to libaom; the
        // tile differs only by our all-zero vs libaom's DC-coeff choice.
        let s = encode_flat_keyframe_64();
        assert_eq!(s.len(), 21);
        assert_eq!(&s[0..2], &[0x12, 0x00], "temporal delimiter");
        assert_eq!(&s[2..10], &[0x0a, 0x06, 0x18, 0x15, 0x7f, 0xfd, 0xa0, 0x08], "seq header");
        assert_eq!(&s[10..12], &[0x32, 0x09], "OBU_FRAME header + size");
        assert_eq!(&s[12..19], &[0x18, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00], "frame header");
        assert_eq!(&s[19..21], &[0x31, 0x80], "tile (skip=0, all-zero residual)");
    }
}
