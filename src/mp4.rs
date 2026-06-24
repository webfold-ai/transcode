// ═══════════════════════════════════════════════════════════════════════════
// mp4.rs — sovereign ISOBMFF muxer (MP4 / MOV), std-only
// ═══════════════════════════════════════════════════════════════════════════
// MP4 and MOV are the same base format (ISO/IEC 14496-12, ISOBMFF). This module
// muxes a single video track of JPEG samples (Motion-JPEG, QuickTime 'mjpa'
// coding) into a valid .mp4/.mov, one JPEG keyframe per sample. It reuses
// jpeg::encode and is the modern-container counterpart to the AVI muxer in
// mjpeg.rs.
//
// Box layout written (ftyp, mdat, moov — mdat before moov so chunk offsets are
// known when the sample tables are written):
//   ftyp
//   mdat                       (all JPEG samples concatenated)
//   moov
//     mvhd
//     trak
//       tkhd
//       mdia
//         mdhd
//         hdlr  ('vide')
//         minf
//           vmhd
//           dinf / dref        (self-contained: url with flag 1)
//           stbl
//             stsd  → 'mjpa' VisualSampleEntry
//             stts  (constant frame duration)
//             stsc  (one sample per chunk)
//             stsz  (per-sample sizes)
//             stco  (chunk offsets into mdat)
//             stss  (all samples sync — every frame a keyframe)
//
// Big-endian throughout (ISOBMFF convention), unlike RIFF/AVI.
// ═══════════════════════════════════════════════════════════════════════════

use crate::jpeg;

fn u32be(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn u16be(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Wrap `body` in a box with the 4-byte `tag`, prefixed by its total size.
fn boxed(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    u32be(&mut b, (body.len() + 8) as u32);
    b.extend_from_slice(tag);
    b.extend_from_slice(body);
    b
}

/// Full-box header (version + 24-bit flags) followed by `rest`.
fn fullbox_body(version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(rest.len() + 4);
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..]); // low 3 bytes
    b.extend_from_slice(rest);
    b
}

const MOVIE_TIMESCALE: u32 = 1000;

/// Build a VisualSampleEntry box with the given codingname, optional codec
/// config (e.g. avcC), and a compressor name.
fn visual_sample_entry(
    codingname: &[u8; 4],
    width: u32,
    height: u32,
    name: &[u8],
    extra: &[u8],
) -> Vec<u8> {
    let mut vse = Vec::new();
    vse.extend_from_slice(&[0u8; 6]); // reserved
    u16be(&mut vse, 1); // data_reference_index
    u16be(&mut vse, 0); // pre_defined
    u16be(&mut vse, 0); // reserved
    for _ in 0..3 {
        u32be(&mut vse, 0); // pre_defined[3]
    }
    u16be(&mut vse, width as u16);
    u16be(&mut vse, height as u16);
    u32be(&mut vse, 0x0048_0000); // horizresolution 72dpi
    u32be(&mut vse, 0x0048_0000); // vertresolution
    u32be(&mut vse, 0); // reserved
    u16be(&mut vse, 1); // frame_count
    let mut compressor = [0u8; 32];
    let nl = name.len().min(31);
    compressor[0] = nl as u8;
    compressor[1..1 + nl].copy_from_slice(&name[..nl]);
    vse.extend_from_slice(&compressor);
    u16be(&mut vse, 0x0018); // depth = 24
    vse.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
    vse.extend_from_slice(extra); // codec config (avcC) or empty
    boxed(codingname, &vse)
}

/// Mux already-encoded JPEG samples into an ISOBMFF (.mp4/.mov) byte stream.
pub fn mux(samples: &[Vec<u8>], width: u32, height: u32, fps: u32) -> Result<Vec<u8>, String> {
    if samples.is_empty() {
        return Err("MP4: no samples".into());
    }
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"qt  "); // QuickTime — 'mjpa' is a QT codec
    u32be(&mut ftyp_body, 0);
    ftyp_body.extend_from_slice(b"qt  ");
    ftyp_body.extend_from_slice(b"isom");
    let ftyp = boxed(b"ftyp", &ftyp_body);
    let entry = visual_sample_entry(b"mjpa", width, height, b"Motion JPEG", &[]);
    Ok(assemble(&ftyp, &entry, samples, width, height, fps))
}

/// Core H.264 ISOBMFF muxer: `avc1` sample entry with an `avcC` config carrying
/// SPS/PPS, samples as length-prefixed NAL units. `ftyp` selects the brand
/// (`.mp4` vs `.mov`). `sps`/`pps` are NAL units (no start code).
fn mux_h264_branded(
    ftyp: &[u8],
    sps: &[u8],
    pps: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    if samples.is_empty() {
        return Err("MP4: no samples".into());
    }
    if sps.len() < 4 {
        return Err("MP4: SPS too short for avcC".into());
    }
    // avcC configuration record (ISO 14496-15)
    let mut avcc = Vec::new();
    avcc.push(1); // configurationVersion
    avcc.push(sps[1]); // AVCProfileIndication (profile_idc)
    avcc.push(sps[2]); // profile_compatibility (constraint flags)
    avcc.push(sps[3]); // AVCLevelIndication
    avcc.push(0xFF); // 6 bits reserved | lengthSizeMinusOne = 3 (4-byte NAL lengths)
    avcc.push(0xE1); // 3 bits reserved | numOfSequenceParameterSets = 1
    u16be(&mut avcc, sps.len() as u16);
    avcc.extend_from_slice(sps);
    avcc.push(1); // numOfPictureParameterSets
    u16be(&mut avcc, pps.len() as u16);
    avcc.extend_from_slice(pps);
    let avcc_box = boxed(b"avcC", &avcc);
    let entry = visual_sample_entry(b"avc1", width, height, b"H.264", &avcc_box);

    // Each sample = 4-byte big-endian length + NAL unit (AVCC framing).
    let framed: Vec<Vec<u8>> = samples
        .iter()
        .map(|nal| {
            let mut s = Vec::with_capacity(nal.len() + 4);
            u32be(&mut s, nal.len() as u32);
            s.extend_from_slice(nal);
            s
        })
        .collect();
    Ok(assemble(ftyp, &entry, &framed, width, height, fps))
}

/// Mux H.264 into an `.mp4` (ISO Base Media, `isom`/`avc1` brand).
pub fn mux_h264(
    sps: &[u8],
    pps: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"isom");
    u32be(&mut ftyp_body, 0x200);
    for b in [b"isom", b"iso2", b"avc1", b"mp41"] {
        ftyp_body.extend_from_slice(b);
    }
    let ftyp = boxed(b"ftyp", &ftyp_body);
    mux_h264_branded(&ftyp, sps, pps, samples, width, height, fps)
}

/// Mux H.264 into a QuickTime `.mov` (`qt ` brand). Same ISOBMFF box tree;
/// only the `ftyp` major brand differs from the `.mp4` form.
pub fn mux_h264_mov(
    sps: &[u8],
    pps: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"qt  ");
    u32be(&mut ftyp_body, 0x200);
    ftyp_body.extend_from_slice(b"qt  ");
    let ftyp = boxed(b"ftyp", &ftyp_body);
    mux_h264_branded(&ftyp, sps, pps, samples, width, height, fps)
}

/// Shared ISOBMFF assembly: ftyp + mdat(samples) + moov(single video track
/// with the given sample-description entry). One sample per chunk; all sync.
fn assemble(
    ftyp: &[u8],
    sample_entry: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<u8> {
    let fps = fps.max(1);
    let n = samples.len() as u32;

    // ── mdat (samples) + offset table ────────────────────────────────────────
    let mdat_payload_off = ftyp.len() + 8; // ftyp + this mdat's 8-byte header
    let mut mdat_body = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(samples.len());
    let mut sizes: Vec<u32> = Vec::with_capacity(samples.len());
    for s in samples {
        offsets.push((mdat_payload_off + mdat_body.len()) as u32);
        sizes.push(s.len() as u32);
        mdat_body.extend_from_slice(s);
    }
    let mdat = boxed(b"mdat", &mdat_body);

    // ── sample tables ─────────────────────────────────────────────────────────
    let media_timescale = fps;
    let media_duration = n; // delta = 1 per sample at timescale = fps
    let movie_duration = (n as u64 * MOVIE_TIMESCALE as u64 / fps as u64) as u32;

    let mut stsd_rest = Vec::new();
    u32be(&mut stsd_rest, 1); // entry_count
    stsd_rest.extend_from_slice(sample_entry);
    let stsd = boxed(b"stsd", &fullbox_body(0, 0, &stsd_rest));

    // stts: one entry, all samples same delta (=1 in media timescale)
    let mut stts_rest = Vec::new();
    u32be(&mut stts_rest, 1); // entry_count
    u32be(&mut stts_rest, n); // sample_count
    u32be(&mut stts_rest, 1); // sample_delta
    let stts = boxed(b"stts", &fullbox_body(0, 0, &stts_rest));

    // stsc: one chunk per sample → samples_per_chunk = 1
    let mut stsc_rest = Vec::new();
    u32be(&mut stsc_rest, 1); // entry_count
    u32be(&mut stsc_rest, 1); // first_chunk
    u32be(&mut stsc_rest, 1); // samples_per_chunk
    u32be(&mut stsc_rest, 1); // sample_description_index
    let stsc = boxed(b"stsc", &fullbox_body(0, 0, &stsc_rest));

    // stsz: per-sample sizes
    let mut stsz_rest = Vec::new();
    u32be(&mut stsz_rest, 0); // sample_size = 0 → use table
    u32be(&mut stsz_rest, n); // sample_count
    for s in &sizes {
        u32be(&mut stsz_rest, *s);
    }
    let stsz = boxed(b"stsz", &fullbox_body(0, 0, &stsz_rest));

    // stco: chunk offsets (one chunk per sample)
    let mut stco_rest = Vec::new();
    u32be(&mut stco_rest, n); // entry_count
    for o in &offsets {
        u32be(&mut stco_rest, *o);
    }
    let stco = boxed(b"stco", &fullbox_body(0, 0, &stco_rest));

    // stss: all samples are sync samples
    let mut stss_rest = Vec::new();
    u32be(&mut stss_rest, n); // entry_count
    for i in 1..=n {
        u32be(&mut stss_rest, i);
    }
    let stss = boxed(b"stss", &fullbox_body(0, 0, &stss_rest));

    let mut stbl_body = Vec::new();
    stbl_body.extend_from_slice(&stsd);
    stbl_body.extend_from_slice(&stts);
    stbl_body.extend_from_slice(&stsc);
    stbl_body.extend_from_slice(&stsz);
    stbl_body.extend_from_slice(&stco);
    stbl_body.extend_from_slice(&stss);
    let stbl = boxed(b"stbl", &stbl_body);

    // vmhd
    let mut vmhd_rest = Vec::new();
    u16be(&mut vmhd_rest, 0); // graphicsmode
    for _ in 0..3 {
        u16be(&mut vmhd_rest, 0); // opcolor
    }
    let vmhd = boxed(b"vmhd", &fullbox_body(0, 1, &vmhd_rest));

    // dinf → dref → url (self-contained, flag 1)
    let url = boxed(b"url ", &fullbox_body(0, 1, &[]));
    let mut dref_rest = Vec::new();
    u32be(&mut dref_rest, 1); // entry_count
    dref_rest.extend_from_slice(&url);
    let dref = boxed(b"dref", &fullbox_body(0, 0, &dref_rest));
    let dinf = boxed(b"dinf", &dref);

    let mut minf_body = Vec::new();
    minf_body.extend_from_slice(&vmhd);
    minf_body.extend_from_slice(&dinf);
    minf_body.extend_from_slice(&stbl);
    let minf = boxed(b"minf", &minf_body);

    // mdhd
    let mut mdhd_rest = Vec::new();
    u32be(&mut mdhd_rest, 0); // creation_time
    u32be(&mut mdhd_rest, 0); // modification_time
    u32be(&mut mdhd_rest, media_timescale);
    u32be(&mut mdhd_rest, media_duration);
    u16be(&mut mdhd_rest, 0x55C4); // language = 'und'
    u16be(&mut mdhd_rest, 0); // pre_defined
    let mdhd = boxed(b"mdhd", &fullbox_body(0, 0, &mdhd_rest));

    // hdlr (vide)
    let mut hdlr_rest = Vec::new();
    u32be(&mut hdlr_rest, 0); // pre_defined
    hdlr_rest.extend_from_slice(b"vide"); // handler_type
    for _ in 0..3 {
        u32be(&mut hdlr_rest, 0); // reserved[3]
    }
    hdlr_rest.extend_from_slice(b"VideoHandler\0");
    let hdlr = boxed(b"hdlr", &fullbox_body(0, 0, &hdlr_rest));

    let mut mdia_body = Vec::new();
    mdia_body.extend_from_slice(&mdhd);
    mdia_body.extend_from_slice(&hdlr);
    mdia_body.extend_from_slice(&minf);
    let mdia = boxed(b"mdia", &mdia_body);

    // tkhd (flags 7 = enabled|in_movie|in_preview)
    let mut tkhd_rest = Vec::new();
    u32be(&mut tkhd_rest, 0); // creation_time
    u32be(&mut tkhd_rest, 0); // modification_time
    u32be(&mut tkhd_rest, 1); // track_ID
    u32be(&mut tkhd_rest, 0); // reserved
    u32be(&mut tkhd_rest, movie_duration);
    for _ in 0..2 {
        u32be(&mut tkhd_rest, 0); // reserved[2]
    }
    u16be(&mut tkhd_rest, 0); // layer
    u16be(&mut tkhd_rest, 0); // alternate_group
    u16be(&mut tkhd_rest, 0); // volume (0 for video)
    u16be(&mut tkhd_rest, 0); // reserved
    // unity transformation matrix
    for &v in &[
        0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
    ] {
        u32be(&mut tkhd_rest, v);
    }
    u32be(&mut tkhd_rest, width << 16); // width (16.16 fixed)
    u32be(&mut tkhd_rest, height << 16); // height
    let tkhd = boxed(b"tkhd", &fullbox_body(0, 7, &tkhd_rest));

    let mut trak_body = Vec::new();
    trak_body.extend_from_slice(&tkhd);
    trak_body.extend_from_slice(&mdia);
    let trak = boxed(b"trak", &trak_body);

    // mvhd
    let mut mvhd_rest = Vec::new();
    u32be(&mut mvhd_rest, 0); // creation_time
    u32be(&mut mvhd_rest, 0); // modification_time
    u32be(&mut mvhd_rest, MOVIE_TIMESCALE);
    u32be(&mut mvhd_rest, movie_duration);
    u32be(&mut mvhd_rest, 0x0001_0000); // rate 1.0
    u16be(&mut mvhd_rest, 0x0100); // volume 1.0
    u16be(&mut mvhd_rest, 0); // reserved
    for _ in 0..2 {
        u32be(&mut mvhd_rest, 0); // reserved[2]
    }
    for &v in &[
        0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
    ] {
        u32be(&mut mvhd_rest, v);
    }
    for _ in 0..6 {
        u32be(&mut mvhd_rest, 0); // pre_defined[6]
    }
    u32be(&mut mvhd_rest, 2); // next_track_ID
    let mvhd = boxed(b"mvhd", &fullbox_body(0, 0, &mvhd_rest));

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd);
    moov_body.extend_from_slice(&trak);
    let moov = boxed(b"moov", &moov_body);

    let mut out = Vec::with_capacity(ftyp.len() + mdat.len() + moov.len());
    out.extend_from_slice(ftyp);
    out.extend_from_slice(&mdat);
    out.extend_from_slice(&moov);
    out
}

/// Convenience: encode RGBA8 frames to H.264 (intra) and mux into an .mp4.
pub fn encode_rgba_h264(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    encode_rgba_h264_qp(frames, width, height, fps, crate::h264::DEFAULT_QP)
}

/// As [`encode_rgba_h264`] with an explicit H.264 quantiser (lower = higher
/// quality + larger file).
pub fn encode_rgba_h264_qp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
    qp: i32,
) -> Result<Vec<u8>, String> {
    let (sps, pps, samples) = crate::h264::encode_mp4_track_qp(frames, width, height, qp)?;
    mux_h264(&sps, &pps, &samples, width, height, fps)
}

/// Convenience: encode RGBA8 frames to H.264 (intra) and mux into a QuickTime `.mov`.
pub fn encode_rgba_h264_mov(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, String> {
    let (sps, pps, samples) = crate::h264::encode_mp4_track(frames, width, height)?;
    mux_h264_mov(&sps, &pps, &samples, width, height, fps)
}

/// Convenience: encode RGBA8 frames to JPEG then mux into MP4/MOV.
pub fn encode_rgba_sequence(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
) -> Result<Vec<u8>, String> {
    let mut jpegs = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        jpegs.push(
            jpeg::encode(width, height, f, quality)
                .map_err(|e| format!("MP4: frame {i} encode failed: {e}"))?,
        );
    }
    mux(&jpegs, width, height, fps)
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
                px[i] = ((x + t * 5) % 256) as u8;
                px[i + 1] = ((y + t * 3) % 256) as u8;
                px[i + 2] = ((x ^ y) % 256) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Walk the top-level box tree and confirm sizes are self-consistent.
    fn walk_boxes(data: &[u8]) -> Vec<[u8; 4]> {
        let mut tags = Vec::new();
        let mut p = 0usize;
        while p + 8 <= data.len() {
            let size =
                u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
            let mut tag = [0u8; 4];
            tag.copy_from_slice(&data[p + 4..p + 8]);
            tags.push(tag);
            assert!(size >= 8, "box {:?} size {size} too small", tag);
            assert!(p + size <= data.len(), "box {:?} overruns file", tag);
            p += size;
        }
        assert_eq!(p, data.len(), "top-level boxes do not tile the file exactly");
        tags
    }

    #[test]
    fn mp4_box_tree_is_consistent() {
        let (w, h) = (32u32, 24u32);
        let frames: Vec<Vec<u8>> = (0..6).map(|t| frame(w as usize, h as usize, t)).collect();
        let mp4 = encode_rgba_sequence(&frames, w, h, 30, 85).expect("mux");
        let tags = walk_boxes(&mp4);
        assert_eq!(&tags[0], b"ftyp");
        assert!(tags.iter().any(|t| t == b"mdat"));
        assert!(tags.iter().any(|t| t == b"moov"));
    }

    #[test]
    fn h264_mp4_box_tree_is_consistent() {
        let (w, h) = (32u32, 32u32);
        let frames: Vec<Vec<u8>> = (0..4).map(|t| frame(w as usize, h as usize, t)).collect();
        let mp4 = encode_rgba_h264(&frames, w, h, 25).expect("h264 mux");
        let tags = walk_boxes(&mp4);
        assert_eq!(&tags[0], b"ftyp");
        assert!(tags.iter().any(|t| t == b"mdat"));
        assert!(tags.iter().any(|t| t == b"moov"));
        // avc1 sample entry + avcC config must be present somewhere in moov
        let find = |needle: &[u8]| mp4.windows(needle.len()).any(|w| w == needle);
        assert!(find(b"avc1"), "missing avc1 sample entry");
        assert!(find(b"avcC"), "missing avcC config box");
    }
}
