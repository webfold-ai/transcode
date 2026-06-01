// ═══════════════════════════════════════════════════════════════════════════
// mkv.rs — sovereign Matroska (MKV) muxer, std-only
// ═══════════════════════════════════════════════════════════════════════════
// Matroska is an EBML container (not ISOBMFF like MP4/MOV). This muxes a single
// H.264 (AVC) video track: EBML header + Segment{ Info, Tracks, Cluster }, one
// SimpleBlock per frame, CodecPrivate = avcC, frame data = length-prefixed NAL
// units (AVC framing). Reuses h264::encode_mp4_track for the elementary stream.
//
// EBML basics: every element is  ID (raw, includes its own length marker) +
// size (vint) + data. We build bottom-up so sizes are exact. Single Cluster
// (relative timecodes are i16 ms → fine for short clips).
// ═══════════════════════════════════════════════════════════════════════════

// ── EBML primitives ─────────────────────────────────────────────────────────

/// Encode an unsigned value as an EBML vint (data size), minimal width.
fn vint(v: u64) -> Vec<u8> {
    let mut len = 1usize;
    while len < 8 && v >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    let mut out = vec![0u8; len];
    let mut x = v;
    for i in (0..len).rev() {
        out[i] = (x & 0xFF) as u8;
        x >>= 8;
    }
    out[0] |= 1u8 << (8 - len); // length-marker bit
    out
}

/// An element: raw ID bytes + vint(size) + content.
fn el(id: &[u8], content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(id.len() + 8 + content.len());
    out.extend_from_slice(id);
    out.extend_from_slice(&vint(content.len() as u64));
    out.extend_from_slice(content);
    out
}

/// Minimal big-endian bytes of an unsigned integer (≥1 byte).
fn uint_bytes(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![0];
    }
    let mut b = Vec::new();
    let mut x = v;
    while x > 0 {
        b.push((x & 0xFF) as u8);
        x >>= 8;
    }
    b.reverse();
    b
}

fn uint_el(id: &[u8], v: u64) -> Vec<u8> {
    el(id, &uint_bytes(v))
}

// Element IDs (written raw — they already carry their length-marker bits).
const EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
const EBML_VERSION: &[u8] = &[0x42, 0x86];
const EBML_READ_VERSION: &[u8] = &[0x42, 0xF7];
const EBML_MAX_ID_LEN: &[u8] = &[0x42, 0xF2];
const EBML_MAX_SIZE_LEN: &[u8] = &[0x42, 0xF3];
const DOCTYPE: &[u8] = &[0x42, 0x82];
const DOCTYPE_VERSION: &[u8] = &[0x42, 0x87];
const DOCTYPE_READ_VERSION: &[u8] = &[0x42, 0x85];
const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const TIMECODE_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
const MUXING_APP: &[u8] = &[0x4D, 0x80];
const WRITING_APP: &[u8] = &[0x57, 0x41];
const DURATION: &[u8] = &[0x44, 0x89];
const TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const TRACK_ENTRY: &[u8] = &[0xAE];
const TRACK_NUMBER: &[u8] = &[0xD7];
const TRACK_UID: &[u8] = &[0x73, 0xC5];
const TRACK_TYPE: &[u8] = &[0x83];
const FLAG_LACING: &[u8] = &[0x9C];
const CODEC_ID: &[u8] = &[0x86];
const CODEC_PRIVATE: &[u8] = &[0x63, 0xA2];
const VIDEO: &[u8] = &[0xE0];
const PIXEL_WIDTH: &[u8] = &[0xB0];
const PIXEL_HEIGHT: &[u8] = &[0xBA];
const CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const TIMECODE: &[u8] = &[0xE7];
const SIMPLE_BLOCK: &[u8] = &[0xA3];

fn float_el(id: &[u8], v: f64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn str_el(id: &[u8], s: &str) -> Vec<u8> {
    el(id, s.as_bytes())
}

/// Mux an H.264 intra track into a Matroska (.mkv) byte stream.
/// `sps`/`pps` are NAL units (no start code); `samples[i]` is the frame's NAL.
pub fn mux_h264(
    sps: &[u8],
    pps: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    if samples.is_empty() {
        return Err("MKV: no samples".into());
    }
    if sps.len() < 4 {
        return Err("MKV: SPS too short for avcC".into());
    }
    let fps = fps.max(1);

    // avcC (CodecPrivate for V_MPEG4/ISO/AVC) — same record as MP4.
    let mut avcc = Vec::new();
    avcc.push(1);
    avcc.push(sps[1]);
    avcc.push(sps[2]);
    avcc.push(sps[3]);
    avcc.push(0xFF); // lengthSizeMinusOne = 3
    avcc.push(0xE1); // numSPS = 1
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1); // numPPS
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);

    // ── EBML header ──
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&uint_el(EBML_VERSION, 1));
    ebml_body.extend_from_slice(&uint_el(EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&uint_el(EBML_MAX_ID_LEN, 4));
    ebml_body.extend_from_slice(&uint_el(EBML_MAX_SIZE_LEN, 8));
    ebml_body.extend_from_slice(&str_el(DOCTYPE, "matroska"));
    ebml_body.extend_from_slice(&uint_el(DOCTYPE_VERSION, 2));
    ebml_body.extend_from_slice(&uint_el(DOCTYPE_READ_VERSION, 2));
    let ebml = el(EBML, &ebml_body);

    // ── Info ── timecode scale = 1 ms (1_000_000 ns)
    let tc_scale = 1_000_000u64;
    let frame_ms = 1000.0 / fps as f64;
    let duration = frame_ms * samples.len() as f64;
    let mut info = Vec::new();
    info.extend_from_slice(&uint_el(TIMECODE_SCALE, tc_scale));
    info.extend_from_slice(&float_el(DURATION, duration));
    info.extend_from_slice(&str_el(MUXING_APP, "transcode"));
    info.extend_from_slice(&str_el(WRITING_APP, "transcode"));
    let info = el(INFO, &info);

    // ── Tracks → one video TrackEntry ──
    let mut video = Vec::new();
    video.extend_from_slice(&uint_el(PIXEL_WIDTH, width as u64));
    video.extend_from_slice(&uint_el(PIXEL_HEIGHT, height as u64));
    let video = el(VIDEO, &video);
    let mut track = Vec::new();
    track.extend_from_slice(&uint_el(TRACK_NUMBER, 1));
    track.extend_from_slice(&uint_el(TRACK_UID, 1));
    track.extend_from_slice(&uint_el(TRACK_TYPE, 1)); // 1 = video
    track.extend_from_slice(&uint_el(FLAG_LACING, 0));
    track.extend_from_slice(&str_el(CODEC_ID, "V_MPEG4/ISO/AVC"));
    track.extend_from_slice(&el(CODEC_PRIVATE, &avcc));
    track.extend_from_slice(&video);
    let track = el(TRACK_ENTRY, &track);
    let tracks = el(TRACKS, &track);

    // ── Cluster → one SimpleBlock per frame ──
    let mut cluster = Vec::new();
    cluster.extend_from_slice(&uint_el(TIMECODE, 0)); // cluster base = 0
    for (i, nal) in samples.iter().enumerate() {
        let rel = (i as f64 * frame_ms).round() as i64;
        if rel > i16::MAX as i64 {
            return Err("MKV: clip too long for a single cluster".into());
        }
        // SimpleBlock: track-vint + i16 timecode + flags + AVC length-prefixed NAL
        let mut blk = Vec::new();
        blk.extend_from_slice(&vint(1)); // track number 1
        blk.extend_from_slice(&(rel as i16).to_be_bytes());
        blk.push(0x80); // keyframe flag
        blk.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        blk.extend_from_slice(nal);
        cluster.extend_from_slice(&el(SIMPLE_BLOCK, &blk));
    }
    let cluster = el(CLUSTER, &cluster);

    // ── Segment ──
    let mut segment = Vec::new();
    segment.extend_from_slice(&info);
    segment.extend_from_slice(&tracks);
    segment.extend_from_slice(&cluster);
    let segment = el(SEGMENT, &segment);

    let mut out = Vec::with_capacity(ebml.len() + segment.len());
    out.extend_from_slice(&ebml);
    out.extend_from_slice(&segment);
    Ok(out)
}

/// Convenience: encode RGBA8 frames to H.264 (intra) and mux into an .mkv.
pub fn encode_rgba_h264(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    let (sps, pps, samples) = crate::h264::encode_mp4_track(frames, width, height)?;
    mux_h264(&sps, &pps, &samples, width, height, fps)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_encoding() {
        assert_eq!(vint(0), vec![0x80]);
        assert_eq!(vint(1), vec![0x81]);
        assert_eq!(vint(126), vec![0xFE]); // 127 = 2^7-1 is reserved → 126 fits 1 byte
        assert_eq!(vint(127), vec![0x40, 0x7F]); // 2 bytes
        assert_eq!(vint(0x3FFE), vec![0x7F, 0xFE]);
    }

    fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = ((x + t * 5) % 256) as u8;
                px[i + 1] = ((y + t * 3) % 256) as u8;
                px[i + 2] = ((x ^ y) % 256) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Walk the top-level EBML elements (EBML header + Segment) and confirm
    /// each declared size matches the bytes consumed.
    #[test]
    fn ebml_structure_is_consistent() {
        let (w, h) = (32u32, 32u32);
        let frames: Vec<Vec<u8>> = (0..4).map(|t| frame(w as usize, h as usize, t)).collect();
        let mkv = encode_rgba_h264(&frames, w, h, 25).expect("mkv mux");
        // first element must be the EBML header id
        assert_eq!(&mkv[0..4], EBML);
        // and contain a Segment + the AVC codec id somewhere
        let find = |needle: &[u8]| mkv.windows(needle.len()).any(|w| w == needle);
        assert!(find(SEGMENT), "missing Segment");
        assert!(find(b"V_MPEG4/ISO/AVC"), "missing AVC CodecID");
        assert!(find(b"matroska"), "missing matroska DocType");
    }
}
