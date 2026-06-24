// ═══════════════════════════════════════════════════════════════════════════
// h264_inter.rs — H.264 P-frames via P_Skip (temporal compression, rung 3)
// ═══════════════════════════════════════════════════════════════════════════
// The first inter-frame coding rung: an IPPPP encoder where each P-frame codes
// macroblocks identical to the reference reconstruction as P_Skip (MV=0, zero
// residual bits — the decoder copies the colocated reference samples), and
// re-keys with a full IDR when content changes beyond a threshold.
//
// This is genuine temporal compression — a static or slow clip costs one IDR
// plus a handful of bytes per unchanged frame — built on the bit-exact intra
// encoder (`h264_intra::encode_idr_intra_recon`). It is NOT motion estimation:
// P_Skip is MV=(0,0). Sub-pel motion compensation + MV prediction + inter
// residual is the next rung; this one proves the P-slice / reference-management
// / skip-run machinery against the ffmpeg oracle without bluffing motion comp.
//
// VERIFICATION: ffmpeg's decode of the produced stream must equal our tracked
// reconstruction bit-exact (a P_Skip frame reconstructs to the reference recon,
// which the intra rung already proved bit-exact).
// ═══════════════════════════════════════════════════════════════════════════

use crate::h264_intra::encode_idr_intra_recon;
use crate::h264bits::{build_pps, build_sps, emit_nal, BitWriter};

/// Build an all-skip P-slice RBSP: `mb_skip_run = total_mbs`, so every MB copies
/// the colocated reference reconstruction (MV=0). `frame_num` and `poc` advance
/// per frame; the slice is a reference (nal_ref_idc ≠ 0) so the next P can use it.
fn build_pskip_slice(total_mbs: u32, qp: i32, frame_num: u32, poc: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.ue(0); // first_mb_in_slice
    bw.ue(0); // slice_type = 0 (P)
    bw.ue(0); // pic_parameter_set_id
    bw.put_bits(frame_num & 0xF, 4); // frame_num (4 bits)
    bw.put_bits(poc & 0xF, 4); // pic_order_cnt_lsb (4 bits)
    bw.put_flag(false); // num_ref_idx_active_override_flag
    bw.put_flag(false); // ref_pic_list_modification_flag_l0
    bw.put_flag(false); // adaptive_ref_pic_marking_mode_flag (non-IDR ref)
    bw.se(qp - 26); // slice_qp_delta → SliceQP = qp
    bw.ue(1); // disable_deblocking_filter_idc = 1 (PPS enables this field)
    bw.ue(total_mbs); // mb_skip_run = all MBs skipped
    bw.trailing_bits();
    bw.into_rbsp()
}

/// Sum of absolute differences between two RGBA frames (RGB channels only).
fn frame_sad(a: &[u8], b: &[u8]) -> u64 {
    let n = a.len().min(b.len());
    let mut s = 0u64;
    let mut i = 0;
    while i + 4 <= n {
        s += (a[i] as i32 - b[i] as i32).unsigned_abs() as u64;
        s += (a[i + 1] as i32 - b[i + 1] as i32).unsigned_abs() as u64;
        s += (a[i + 2] as i32 - b[i + 2] as i32).unsigned_abs() as u64;
        i += 4;
    }
    s
}

/// Encode an IPPPP H.264 Annex-B stream: frame 0 (and any frame differing from
/// the reference by more than `mean_change` per pixel) is an IDR; unchanged
/// frames are all-skip P-frames. Returns the stream and the per-frame
/// reconstructions (for verification).
pub fn encode_ipppp_with_recon(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    qp: i32,
    mean_change: u32,
) -> Result<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>), String> {
    if frames.is_empty() {
        return Err("H264-inter: no frames".into());
    }
    let total_mbs = ((width + 15) / 16) * ((height + 15) / 16);
    let pixels = (width as u64) * (height as u64);
    let sad_threshold = mean_change as u64 * pixels * 3; // 3 channels

    let mut out = Vec::new();
    emit_nal(&mut out, 3, 7, &build_sps(width, height));
    emit_nal(&mut out, 3, 8, &build_pps());

    let mut recons = Vec::with_capacity(frames.len());
    let mut idr_id = 0u32;
    let mut frame_num = 0u32;

    // frame 0 → IDR
    let (slice0, recon0) = encode_idr_intra_recon(width, height, &frames[0], qp, idr_id)?;
    emit_nal(&mut out, 3, 5, &slice0);
    let mut ref_src = frames[0].clone();
    let mut ref_recon = recon0.clone();
    recons.push(recon0);
    idr_id = idr_id.wrapping_add(1);

    for f in &frames[1..] {
        if frame_sad(f, &ref_src) <= sad_threshold {
            // unchanged → all-skip P-frame; reconstruction = reference recon
            frame_num = (frame_num + 1) & 0xF;
            let poc = (frame_num * 2) & 0xF;
            let slice = build_pskip_slice(total_mbs, qp, frame_num, poc);
            emit_nal(&mut out, 2, 1, &slice); // non-IDR reference P-slice
            recons.push(ref_recon.clone());
        } else {
            // content changed → re-key with an IDR (resets frame_num)
            frame_num = 0;
            let (slice, recon) = encode_idr_intra_recon(width, height, f, qp, idr_id)?;
            emit_nal(&mut out, 3, 5, &slice);
            ref_src = f.clone();
            ref_recon = recon.clone();
            recons.push(recon);
            idr_id = idr_id.wrapping_add(1);
        }
    }
    Ok((out, recons))
}

/// Convenience: IPPPP stream without the reconstruction side-channel.
pub fn encode_ipppp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    qp: i32,
    mean_change: u32,
) -> Result<Vec<u8>, String> {
    Ok(encode_ipppp_with_recon(frames, width, height, qp, mean_change)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = (x * 255 / w) as u8;
                px[i + 1] = (y * 255 / h) as u8;
                px[i + 2] = ((x + y + t) % 256) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn static_clip_uses_skip_frames() {
        let (w, h) = (48u32, 48u32);
        let f = frame(w as usize, h as usize, 0);
        let frames: Vec<Vec<u8>> = (0..8).map(|_| f.clone()).collect(); // identical
        let stream = encode_ipppp(&frames, w, h, 28, 2).expect("encode");
        // count start codes: should be SPS + PPS + 1 IDR + 7 tiny P-slices = 10
        let nals = stream.windows(4).filter(|x| *x == [0, 0, 0, 1]).count();
        assert_eq!(nals, 10, "expected SPS+PPS+IDR+7P start codes, got {nals}");
        // a static clip (1 IDR + 7 skip P-slices) must be far smaller than the
        // same 8 frames coded as independent IDRs (all visibly different).
        let diff_frames: Vec<Vec<u8>> =
            (0..8).map(|t| frame(w as usize, h as usize, t * 50)).collect();
        let idr_only = encode_ipppp(&diff_frames, w, h, 28, 2).expect("all-idr");
        let idr_nals = idr_only.windows(4).filter(|x| *x == [0, 0, 0, 1]).count();
        assert_eq!(idr_nals, 10, "all-different frames should be 8 IDRs (+SPS+PPS)");
        assert!(
            stream.len() * 3 < idr_only.len(),
            "skip stream {} should be ≪ all-IDR {}",
            stream.len(),
            idr_only.len()
        );
    }

    #[test]
    fn changed_frames_rekey() {
        let (w, h) = (32u32, 32u32);
        // each frame visibly different → every frame should re-key (IDR)
        let frames: Vec<Vec<u8>> = (0..4).map(|t| frame(w as usize, h as usize, t * 60)).collect();
        let stream = encode_ipppp(&frames, w, h, 28, 2).expect("encode");
        // SPS+PPS + 4 IDR = 6 start codes
        let nals = stream.windows(4).filter(|x| *x == [0, 0, 0, 1]).count();
        assert_eq!(nals, 6, "all-different frames should produce 4 IDRs (+SPS+PPS)");
    }
}
