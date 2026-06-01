// ═══════════════════════════════════════════════════════════════════════════
// mjpeg.rs — sovereign AVI muxer carrying Motion-JPEG, std-only
// ═══════════════════════════════════════════════════════════════════════════
// The first video rung: a real RIFF/AVI container holding one MJPG video
// stream, one JPEG (from jpeg::encode) per frame. Intra-only — no motion
// compensation, no inter-frame prediction — so every frame is a keyframe.
// This is the honest minimal "video" milestone: a .avi that ffmpeg, VLC and
// browsers play, built end-to-end from our own JPEG codec + this muxer, with
// zero external crates.
//
// AVI is little-endian RIFF. Layout written here:
//   RIFF 'AVI '
//     LIST 'hdrl'
//       avih                         (MainAVIHeader)
//       LIST 'strl'
//         strh                       (AVIStreamHeader, vids/MJPG)
//         strf                       (BITMAPINFOHEADER, biCompression=MJPG)
//     LIST 'movi'
//       '00dc' <jpeg> ...            (one keyframe chunk per frame)
//     idx1                           (legacy index, all frames keyframes)
//
// Temporal compression (P/B frames, motion vectors) is the NEXT rung and is a
// genuinely different problem — there is no closed form for a block-matching
// motion search, the Sanguine affine-loop results do not apply (confirmed by
// reading those proofs). This file makes no such claim: MJPEG is spatial-only.
// ═══════════════════════════════════════════════════════════════════════════

use crate::jpeg;

fn u32le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn u16le(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn fourcc(out: &mut Vec<u8>, tag: &[u8; 4]) {
    out.extend_from_slice(tag);
}

/// Mux a sequence of already-encoded JPEG frames into a complete AVI byte
/// stream. `frames[i]` is the full JPEG (SOI..EOI) for frame i.
pub fn mux_avi(frames: &[Vec<u8>], width: u32, height: u32, fps: u32) -> Result<Vec<u8>, String> {
    if frames.is_empty() {
        return Err("MJPEG: no frames".into());
    }
    let fps = fps.max(1);
    let n = frames.len() as u32;
    let max_frame = frames.iter().map(|f| f.len()).max().unwrap_or(0) as u32;

    // ── hdrl ──────────────────────────────────────────────────────────────
    let mut avih = Vec::new();
    u32le(&mut avih, 1_000_000 / fps); // dwMicroSecPerFrame
    u32le(&mut avih, max_frame * fps); // dwMaxBytesPerSec (approx)
    u32le(&mut avih, 0); // dwPaddingGranularity
    u32le(&mut avih, 0x10); // dwFlags = AVIF_HASINDEX
    u32le(&mut avih, n); // dwTotalFrames
    u32le(&mut avih, 0); // dwInitialFrames
    u32le(&mut avih, 1); // dwStreams
    u32le(&mut avih, max_frame); // dwSuggestedBufferSize
    u32le(&mut avih, width);
    u32le(&mut avih, height);
    for _ in 0..4 {
        u32le(&mut avih, 0); // dwReserved[4]
    }

    let mut strh = Vec::new();
    fourcc(&mut strh, b"vids");
    fourcc(&mut strh, b"MJPG");
    u32le(&mut strh, 0); // dwFlags
    u16le(&mut strh, 0); // wPriority
    u16le(&mut strh, 0); // wLanguage
    u32le(&mut strh, 0); // dwInitialFrames
    u32le(&mut strh, 1); // dwScale
    u32le(&mut strh, fps); // dwRate → rate/scale = fps
    u32le(&mut strh, 0); // dwStart
    u32le(&mut strh, n); // dwLength
    u32le(&mut strh, max_frame); // dwSuggestedBufferSize
    u32le(&mut strh, 0xFFFF_FFFF); // dwQuality
    u32le(&mut strh, 0); // dwSampleSize
    u16le(&mut strh, 0); // rcFrame.left
    u16le(&mut strh, 0); // rcFrame.top
    u16le(&mut strh, width as u16); // rcFrame.right
    u16le(&mut strh, height as u16); // rcFrame.bottom

    let mut strf = Vec::new();
    u32le(&mut strf, 40); // biSize
    u32le(&mut strf, width); // biWidth
    u32le(&mut strf, height); // biHeight
    u16le(&mut strf, 1); // biPlanes
    u16le(&mut strf, 24); // biBitCount
    fourcc(&mut strf, b"MJPG"); // biCompression
    u32le(&mut strf, width * height * 3); // biSizeImage
    u32le(&mut strf, 0); // biXPelsPerMeter
    u32le(&mut strf, 0); // biYPelsPerMeter
    u32le(&mut strf, 0); // biClrUsed
    u32le(&mut strf, 0); // biClrImportant

    // strl = strh + strf, wrapped
    let mut strl = Vec::new();
    fourcc(&mut strl, b"LIST");
    u32le(&mut strl, (4 + 8 + strh.len() + 8 + strf.len()) as u32);
    fourcc(&mut strl, b"strl");
    fourcc(&mut strl, b"strh");
    u32le(&mut strl, strh.len() as u32);
    strl.extend_from_slice(&strh);
    fourcc(&mut strl, b"strf");
    u32le(&mut strl, strf.len() as u32);
    strl.extend_from_slice(&strf);

    let mut hdrl = Vec::new();
    fourcc(&mut hdrl, b"LIST");
    u32le(&mut hdrl, (4 + 8 + avih.len() + strl.len()) as u32);
    fourcc(&mut hdrl, b"hdrl");
    fourcc(&mut hdrl, b"avih");
    u32le(&mut hdrl, avih.len() as u32);
    hdrl.extend_from_slice(&avih);
    hdrl.extend_from_slice(&strl);

    // ── movi (frame chunks) + index entries ────────────────────────────────
    // Build movi body; record each chunk's offset relative to the start of the
    // 'movi' list data (i.e. relative to the 'movi' FOURCC), the convention
    // ffmpeg expects for idx1.
    let mut movi_body = Vec::new();
    fourcc(&mut movi_body, b"movi"); // movi data begins with its own FOURCC; offsets are relative to here
    let mut index: Vec<(u32, u32)> = Vec::new(); // (offset, size)
    for f in frames {
        let off = movi_body.len() as u32; // relative to 'movi' fourcc
        fourcc(&mut movi_body, b"00dc");
        u32le(&mut movi_body, f.len() as u32);
        movi_body.extend_from_slice(f);
        if f.len() % 2 == 1 {
            movi_body.push(0); // pad to even
        }
        index.push((off, f.len() as u32));
    }
    let mut movi = Vec::new();
    fourcc(&mut movi, b"LIST");
    u32le(&mut movi, movi_body.len() as u32);
    movi.extend_from_slice(&movi_body);

    // idx1: entries reference '00dc', AVIIF_KEYFRAME (0x10), offset, size.
    let mut idx1 = Vec::new();
    fourcc(&mut idx1, b"idx1");
    u32le(&mut idx1, (index.len() * 16) as u32);
    for (off, size) in &index {
        fourcc(&mut idx1, b"00dc");
        u32le(&mut idx1, 0x10); // AVIIF_KEYFRAME
        u32le(&mut idx1, *off);
        u32le(&mut idx1, *size);
    }

    // ── RIFF wrapper ────────────────────────────────────────────────────────
    let payload_len = 4 + hdrl.len() + movi.len() + idx1.len(); // 4 = 'AVI '
    let mut out = Vec::with_capacity(payload_len + 8);
    fourcc(&mut out, b"RIFF");
    u32le(&mut out, payload_len as u32);
    fourcc(&mut out, b"AVI ");
    out.extend_from_slice(&hdrl);
    out.extend_from_slice(&movi);
    out.extend_from_slice(&idx1);
    Ok(out)
}

/// Convenience: encode a sequence of RGBA8 frames to a Motion-JPEG AVI.
/// `frames[i]` is `width*height*4` RGBA bytes. `quality` is the JPEG quality.
pub fn encode_rgba_sequence(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
) -> Result<Vec<u8>, String> {
    let mut jpegs = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        let jpg = jpeg::encode(width, height, f, quality)
            .map_err(|e| format!("MJPEG: frame {i} encode failed: {e}"))?;
        jpegs.push(jpg);
    }
    mux_avi(&jpegs, width, height, fps)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = ((x + t * 4) % 256) as u8;
                px[i + 1] = ((y + t * 2) % 256) as u8;
                px[i + 2] = ((x ^ y).wrapping_add(t * 8) % 256) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn avi_structure_is_well_formed() {
        let (w, h) = (32u32, 24u32);
        let frames: Vec<Vec<u8>> = (0..5).map(|t| frame(w as usize, h as usize, t)).collect();
        let avi = encode_rgba_sequence(&frames, w, h, 25, 85).expect("mux");

        // RIFF header
        assert_eq!(&avi[0..4], b"RIFF");
        assert_eq!(&avi[8..12], b"AVI ");
        // declared RIFF size == file length - 8
        let riff_size = u32::from_le_bytes([avi[4], avi[5], avi[6], avi[7]]) as usize;
        assert_eq!(riff_size, avi.len() - 8, "RIFF size field wrong");

        // hdrl + movi + idx1 present
        let s = &avi;
        let find = |needle: &[u8]| s.windows(needle.len()).position(|w| w == needle);
        assert!(find(b"hdrl").is_some());
        assert!(find(b"strh").is_some());
        assert!(find(b"MJPG").is_some());
        assert!(find(b"movi").is_some());
        assert!(find(b"idx1").is_some());

        // exactly 5 frame chunks
        let chunks = s.windows(4).filter(|w| *w == b"00dc").count();
        // 5 in movi + 5 in idx1 = 10 occurrences of the fourcc
        assert_eq!(chunks, 10, "expected 5 movi + 5 idx1 '00dc' tags");
    }
}
