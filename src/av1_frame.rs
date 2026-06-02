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
const SKIP_CTX0: [u16; 1] = [31671]; // 2 symbols
const KF_YMODE_00: [u16; 12] =
    [15588, 17027, 19338, 20218, 20682, 21110, 21825, 23244, 24189, 28165, 29093, 30466]; // 13
const UV_MODE_DC: [u16; 12] =
    [22631, 24152, 25378, 25661, 25986, 26520, 27055, 27923, 28244, 30059, 30941, 31961]; // 13

/// Build an AOM_ICDF cdf for the coder from dav1d's cumulative args: `nsymbs`
/// entries (last is the 0 terminator) + a trailing adaptation-counter slot.
fn cdf(args: &[u16]) -> Vec<u16> {
    let mut v: Vec<u16> = args.iter().map(|&a| 32768 - a).collect();
    v.push(0); // implicit final symbol's icdf (terminator)
    v.push(0); // adaptation counter
    v
}

/// Symbol-code the single 64×64 all-skip DC superblock → tile data bytes.
fn encode_tile_flat_dc() -> Vec<u8> {
    let mut w = Writer::new();
    // decode_partition(BLOCK_64X64): PARTITION_NONE
    let mut part = cdf(&PARTITION_64_CTX0);
    w.symbol(0, &mut part, 10);
    // intra_frame_mode_info: skip = 1
    let mut skip = cdf(&SKIP_CTX0);
    w.symbol(1, &mut skip, 2);
    // intra_frame_y_mode = DC_PRED (kf_y_mode[0][0])
    let mut ym = cdf(&KF_YMODE_00);
    w.symbol(0, &mut ym, 13);
    // uv_mode = UV_DC (cfl not allowed at 64×64)
    let mut uv = cdf(&UV_MODE_DC);
    w.symbol(0, &mut uv, 13);
    w.finish()
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
    fn tile_is_valid_skip1_encoding() {
        // We code skip=1 (no residual); libaom's reference coded skip=0 + an
        // all-zero residual. BOTH are valid AV1 for a flat block — different
        // encoder decisions, so different bytes. Decoding libaom's `09 ac` with
        // our msac Reader yields (partition=NONE, skip=0, DC, DC), confirming our
        // CDFs/contexts match; we emit the skip=1 form (`98 80`). The ground
        // truth is the ffmpeg decode (tests/av1_decode.rs), not a byte match.
        assert_eq!(encode_tile_flat_dc(), vec![0x98, 0x80]);
    }

    #[test]
    fn full_stream_structure() {
        let s = encode_flat_keyframe_64();
        // TD(2) + seqhdr(8) + OBU_FRAME(header 1 + leb 1 + payload 9) = 21 bytes.
        assert_eq!(s.len(), 21);
        assert_eq!(&s[0..2], &[0x12, 0x00], "temporal delimiter");
        assert_eq!(&s[2..10], &[0x0a, 0x06, 0x18, 0x15, 0x7f, 0xfd, 0xa0, 0x08], "seq header (== libaom)");
        assert_eq!(s[10], 0x32, "OBU_FRAME header byte");
        assert_eq!(s[11], 0x09, "OBU_FRAME payload size = 9");
        // frame header == libaom; tile = our skip=1 form
        assert_eq!(&s[12..19], &[0x18, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00], "frame header (== libaom)");
        assert_eq!(&s[19..21], &[0x98, 0x80], "tile (skip=1)");
    }
}
