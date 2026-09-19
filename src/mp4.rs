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

/// Build the `avc1` VisualSampleEntry, including the `avcC` configuration record
/// (ISO 14496-15) that carries SPS/PPS. `sps`/`pps` are NAL units (no start code).
fn avc1_sample_entry(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    if sps.len() < 4 {
        return Err("MP4: SPS too short for avcC".into());
    }
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
    Ok(visual_sample_entry(
        b"avc1", width, height, b"H.264", &avcc_box,
    ))
}

/// Re-frame raw NAL units as AVCC samples: 4-byte big-endian length + NAL.
/// `avcC` above declares `lengthSizeMinusOne = 3`, so the two must agree.
fn avcc_frame_samples(samples: &[Vec<u8>]) -> Vec<Vec<u8>> {
    samples
        .iter()
        .map(|nal| {
            let mut s = Vec::with_capacity(nal.len() + 4);
            u32be(&mut s, nal.len() as u32);
            s.extend_from_slice(nal);
            s
        })
        .collect()
}

/// Core H.264 ISOBMFF muxer: `avc1` sample entry with an `avcC` config carrying
/// SPS/PPS, samples as length-prefixed NAL units. `ftyp` selects the brand
/// (`.mp4` vs `.mov`).
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
    let entry = avc1_sample_entry(sps, pps, width, height)?;
    let framed = avcc_frame_samples(samples);
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

/// Build an `esds` box: the MPEG-4 ES_Descriptor carrying an AudioSpecificConfig.
/// Descriptor lengths use the single-byte short form, which is valid for every
/// payload under 128 bytes — an ASC is 2.
fn esds(asc: &[u8], max_bitrate: u32, avg_bitrate: u32) -> Vec<u8> {
    fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut d = Vec::with_capacity(body.len() + 2);
        d.push(tag);
        d.push(body.len() as u8);
        d.extend_from_slice(body);
        d
    }

    // DecoderSpecificInfo (tag 0x05) — the AudioSpecificConfig itself.
    let dsi = descriptor(0x05, asc);

    // DecoderConfigDescriptor (tag 0x04)
    let mut dcd_body = Vec::new();
    dcd_body.push(0x40); // objectTypeIndication: MPEG-4 Audio
    dcd_body.push(0x15); // streamType 5 (audio) << 2 | upStream 0 | reserved 1
    dcd_body.extend_from_slice(&[0, 0, 0]); // bufferSizeDB (24-bit)
    u32be(&mut dcd_body, max_bitrate);
    u32be(&mut dcd_body, avg_bitrate);
    dcd_body.extend_from_slice(&dsi);
    let dcd = descriptor(0x04, &dcd_body);

    // SLConfigDescriptor (tag 0x06) — 0x02 = "predefined: MP4"
    let sl = descriptor(0x06, &[0x02]);

    // ES_Descriptor (tag 0x03)
    let mut es_body = Vec::new();
    u16be(&mut es_body, 0); // ES_ID
    es_body.push(0); // flags: no dependency, no URL, no OCR
    es_body.extend_from_slice(&dcd);
    es_body.extend_from_slice(&sl);
    let es = descriptor(0x03, &es_body);

    boxed(b"esds", &fullbox_body(0, 0, &es))
}

/// Build an `mp4a` AudioSampleEntry wrapping an `esds` config.
fn audio_sample_entry_aac(asc: &[u8], channels: u16, rate: u32, avg_bitrate: u32) -> Vec<u8> {
    let mut ase = Vec::new();
    ase.extend_from_slice(&[0u8; 6]); // reserved
    u16be(&mut ase, 1); // data_reference_index
    u32be(&mut ase, 0); // reserved[2]
    u32be(&mut ase, 0);
    u16be(&mut ase, channels.max(1));
    u16be(&mut ase, 16); // samplesize
    u16be(&mut ase, 0); // pre_defined
    u16be(&mut ase, 0); // reserved
    // samplerate as 16.16 fixed point. Rates above 65535 cannot be expressed
    // here; MP4 readers take the real rate from the AudioSpecificConfig anyway.
    u32be(&mut ase, (rate.min(65535)) << 16);
    ase.extend_from_slice(&esds(asc, avg_bitrate, avg_bitrate));
    boxed(b"mp4a", &ase)
}

/// One elementary stream to be written as an ISOBMFF `trak`.
struct Track<'a> {
    handler: &'a [u8; 4],
    handler_name: &'a [u8],
    /// `vmhd` for video, `smhd` for sound.
    media_header: Vec<u8>,
    sample_entry: Vec<u8>,
    samples: &'a [Vec<u8>],
    /// Ticks per second for this track's media timeline.
    timescale: u32,
    /// Duration of one sample in `timescale` ticks (1 for video at
    /// timescale=fps; 1024 for an AAC access unit).
    sample_delta: u32,
    /// How many samples to group per chunk — roughly one second's worth, so the
    /// tracks interleave at a granularity players can stream.
    samples_per_chunk: usize,
    width: u32,
    height: u32,
    volume: u16,
    /// Video is all-intra here, so every sample is a sync sample and `stss` is
    /// written. Audio omits `stss` entirely, which means the same thing.
    write_stss: bool,
}

/// Assemble ftyp + mdat + moov for one or more tracks, interleaving their
/// samples chunk by chunk.
///
/// Interleaving matters: with both tracks' samples grouped into ~1s chunks and
/// written round-robin, a player reading linearly always has the audio it needs
/// for the video it is about to show. Concatenating one whole track then the
/// other is legal — the offset tables are explicit — but forces a seek per
/// track and breaks progressive playback.
fn assemble_tracks(ftyp: &[u8], tracks: &[Track]) -> Vec<u8> {
    // ── chunk plan ───────────────────────────────────────────────────────────
    let mut plans: Vec<Vec<(usize, usize)>> = Vec::with_capacity(tracks.len());
    for t in tracks {
        let spc = t.samples_per_chunk.max(1);
        let mut chunks = Vec::new();
        let mut i = 0;
        while i < t.samples.len() {
            let count = spc.min(t.samples.len() - i);
            chunks.push((i, count));
            i += count;
        }
        plans.push(chunks);
    }

    // ── mdat, written round-robin across tracks ──────────────────────────────
    let mdat_payload_off = ftyp.len() + 8;
    let mut mdat_body = Vec::new();
    let mut chunk_offsets: Vec<Vec<u32>> = vec![Vec::new(); tracks.len()];
    let max_chunks = plans.iter().map(|c| c.len()).max().unwrap_or(0);
    for ci in 0..max_chunks {
        for (ti, chunks) in plans.iter().enumerate() {
            let Some(&(start, count)) = chunks.get(ci) else {
                continue;
            };
            chunk_offsets[ti].push((mdat_payload_off + mdat_body.len()) as u32);
            for s in &tracks[ti].samples[start..start + count] {
                mdat_body.extend_from_slice(s);
            }
        }
    }
    let mdat = boxed(b"mdat", &mdat_body);

    // ── one trak per track ───────────────────────────────────────────────────
    let mut traks = Vec::new();
    let mut movie_duration_max = 0u32;

    for (ti, t) in tracks.iter().enumerate() {
        let n = t.samples.len() as u32;
        let timescale = t.timescale.max(1);
        let media_duration = n * t.sample_delta;
        let movie_duration =
            (media_duration as u64 * MOVIE_TIMESCALE as u64 / timescale as u64) as u32;
        movie_duration_max = movie_duration_max.max(movie_duration);

        let mut stsd_rest = Vec::new();
        u32be(&mut stsd_rest, 1); // entry_count
        stsd_rest.extend_from_slice(&t.sample_entry);
        let stsd = boxed(b"stsd", &fullbox_body(0, 0, &stsd_rest));

        let mut stts_rest = Vec::new();
        u32be(&mut stts_rest, 1); // entry_count
        u32be(&mut stts_rest, n); // sample_count
        u32be(&mut stts_rest, t.sample_delta);
        let stts = boxed(b"stts", &fullbox_body(0, 0, &stts_rest));

        // stsc, run-length encoded: a new entry only where the per-chunk sample
        // count changes, which in practice is just the short final chunk.
        let mut stsc_entries: Vec<(u32, u32)> = Vec::new(); // (first_chunk, count)
        for (ci, &(_, count)) in plans[ti].iter().enumerate() {
            let count = count as u32;
            if stsc_entries.last().map(|&(_, c)| c) != Some(count) {
                stsc_entries.push((ci as u32 + 1, count));
            }
        }
        let mut stsc_rest = Vec::new();
        u32be(&mut stsc_rest, stsc_entries.len() as u32);
        for (first_chunk, count) in &stsc_entries {
            u32be(&mut stsc_rest, *first_chunk);
            u32be(&mut stsc_rest, *count);
            u32be(&mut stsc_rest, 1); // sample_description_index
        }
        let stsc = boxed(b"stsc", &fullbox_body(0, 0, &stsc_rest));

        let mut stsz_rest = Vec::new();
        u32be(&mut stsz_rest, 0); // sample_size = 0 → per-sample table
        u32be(&mut stsz_rest, n);
        for s in t.samples {
            u32be(&mut stsz_rest, s.len() as u32);
        }
        let stsz = boxed(b"stsz", &fullbox_body(0, 0, &stsz_rest));

        let mut stco_rest = Vec::new();
        u32be(&mut stco_rest, chunk_offsets[ti].len() as u32);
        for o in &chunk_offsets[ti] {
            u32be(&mut stco_rest, *o);
        }
        let stco = boxed(b"stco", &fullbox_body(0, 0, &stco_rest));

        let mut stbl_body = Vec::new();
        stbl_body.extend_from_slice(&stsd);
        stbl_body.extend_from_slice(&stts);
        stbl_body.extend_from_slice(&stsc);
        stbl_body.extend_from_slice(&stsz);
        stbl_body.extend_from_slice(&stco);
        if t.write_stss {
            let mut stss_rest = Vec::new();
            u32be(&mut stss_rest, n);
            for i in 1..=n {
                u32be(&mut stss_rest, i);
            }
            stbl_body.extend_from_slice(&boxed(b"stss", &fullbox_body(0, 0, &stss_rest)));
        }
        let stbl = boxed(b"stbl", &stbl_body);

        // dinf → dref → url (self-contained, flag 1)
        let url = boxed(b"url ", &fullbox_body(0, 1, &[]));
        let mut dref_rest = Vec::new();
        u32be(&mut dref_rest, 1);
        dref_rest.extend_from_slice(&url);
        let dref = boxed(b"dref", &fullbox_body(0, 0, &dref_rest));
        let dinf = boxed(b"dinf", &dref);

        let mut minf_body = Vec::new();
        minf_body.extend_from_slice(&t.media_header);
        minf_body.extend_from_slice(&dinf);
        minf_body.extend_from_slice(&stbl);
        let minf = boxed(b"minf", &minf_body);

        let mut mdhd_rest = Vec::new();
        u32be(&mut mdhd_rest, 0); // creation_time
        u32be(&mut mdhd_rest, 0); // modification_time
        u32be(&mut mdhd_rest, timescale);
        u32be(&mut mdhd_rest, media_duration);
        u16be(&mut mdhd_rest, 0x55C4); // language = 'und'
        u16be(&mut mdhd_rest, 0); // pre_defined
        let mdhd = boxed(b"mdhd", &fullbox_body(0, 0, &mdhd_rest));

        let mut hdlr_rest = Vec::new();
        u32be(&mut hdlr_rest, 0); // pre_defined
        hdlr_rest.extend_from_slice(t.handler);
        for _ in 0..3 {
            u32be(&mut hdlr_rest, 0); // reserved[3]
        }
        hdlr_rest.extend_from_slice(t.handler_name);
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
        u32be(&mut tkhd_rest, ti as u32 + 1); // track_ID
        u32be(&mut tkhd_rest, 0); // reserved
        u32be(&mut tkhd_rest, movie_duration);
        for _ in 0..2 {
            u32be(&mut tkhd_rest, 0); // reserved[2]
        }
        u16be(&mut tkhd_rest, 0); // layer
        u16be(&mut tkhd_rest, 0); // alternate_group
        u16be(&mut tkhd_rest, t.volume);
        u16be(&mut tkhd_rest, 0); // reserved
        for &v in &[
            0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
        ] {
            u32be(&mut tkhd_rest, v);
        }
        u32be(&mut tkhd_rest, t.width << 16); // 16.16 fixed
        u32be(&mut tkhd_rest, t.height << 16);
        let tkhd = boxed(b"tkhd", &fullbox_body(0, 7, &tkhd_rest));

        let mut trak_body = Vec::new();
        trak_body.extend_from_slice(&tkhd);
        trak_body.extend_from_slice(&mdia);
        traks.push(boxed(b"trak", &trak_body));
    }

    // ── mvhd ─────────────────────────────────────────────────────────────────
    let mut mvhd_rest = Vec::new();
    u32be(&mut mvhd_rest, 0); // creation_time
    u32be(&mut mvhd_rest, 0); // modification_time
    u32be(&mut mvhd_rest, MOVIE_TIMESCALE);
    u32be(&mut mvhd_rest, movie_duration_max);
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
    u32be(&mut mvhd_rest, tracks.len() as u32 + 1); // next_track_ID
    let mvhd = boxed(b"mvhd", &fullbox_body(0, 0, &mvhd_rest));

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd);
    for trak in &traks {
        moov_body.extend_from_slice(trak);
    }
    let moov = boxed(b"moov", &moov_body);

    let mut out = Vec::with_capacity(ftyp.len() + mdat.len() + moov.len());
    out.extend_from_slice(ftyp);
    out.extend_from_slice(&mdat);
    out.extend_from_slice(&moov);
    out
}

/// `vmhd` — the video media header.
fn vmhd() -> Vec<u8> {
    let mut rest = Vec::new();
    u16be(&mut rest, 0); // graphicsmode
    for _ in 0..3 {
        u16be(&mut rest, 0); // opcolor
    }
    boxed(b"vmhd", &fullbox_body(0, 1, &rest))
}

/// `smhd` — the sound media header.
fn smhd() -> Vec<u8> {
    let mut rest = Vec::new();
    u16be(&mut rest, 0); // balance (8.8 fixed, centre)
    u16be(&mut rest, 0); // reserved
    boxed(b"smhd", &fullbox_body(0, 0, &rest))
}

/// Build the video `Track` shared by every video-only and A/V path here.
fn video_track<'a>(
    sample_entry: Vec<u8>,
    samples: &'a [Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Track<'a> {
    Track {
        handler: b"vide",
        handler_name: b"VideoHandler\0",
        media_header: vmhd(),
        sample_entry,
        samples,
        timescale: fps.max(1),
        sample_delta: 1, // one tick per frame, since timescale == fps
        samples_per_chunk: fps.max(1) as usize,
        width,
        height,
        volume: 0,
        write_stss: true,
    }
}

/// Shared ISOBMFF assembly: ftyp + mdat(samples) + moov(single video track
/// with the given sample-description entry).
fn assemble(
    ftyp: &[u8],
    sample_entry: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<u8> {
    let track = video_track(sample_entry.to_vec(), samples, width, height, fps);
    assemble_tracks(ftyp, &[track])
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

/// Mux an H.264 video track and an AAC audio track into one `.mp4`.
///
/// `aac_adts` is an ADTS stream as produced by [`crate::aac::encode_pcm`]; its
/// per-frame headers are stripped and its configuration hoisted into the track's
/// `esds`, because that is how MP4 carries it (once, out of band) versus how
/// ADTS does (on every frame).
pub fn mux_h264_aac(
    sps: &[u8],
    pps: &[u8],
    video_samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
    aac_adts: &[u8],
    audio_rate: u32,
    audio_channels: u16,
) -> Result<Vec<u8>, String> {
    if video_samples.is_empty() {
        return Err("MP4: no video samples".into());
    }
    let (audio_samples, asc) = crate::aac::adts_to_raw_frames(aac_adts)?;
    if audio_samples.is_empty() {
        return Err("MP4: no audio samples".into());
    }

    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"isom");
    u32be(&mut ftyp_body, 0x200);
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        ftyp_body.extend_from_slice(brand);
    }
    let ftyp = boxed(b"ftyp", &ftyp_body);

    let video_entry = avc1_sample_entry(sps, pps, width, height)?;
    let framed_video = avcc_frame_samples(video_samples);

    // An AAC access unit is always 1024 samples, so a second of audio is
    // rate/1024 frames — the chunk size that interleaves with one second of video.
    let frames_per_second = (audio_rate as usize).div_ceil(1024).max(1);
    let avg_bitrate = if audio_samples.is_empty() {
        0
    } else {
        let total_bytes: usize = audio_samples.iter().map(|s| s.len()).sum();
        let seconds = (audio_samples.len() * 1024) as f64 / audio_rate.max(1) as f64;
        if seconds > 0.0 {
            (total_bytes as f64 * 8.0 / seconds) as u32
        } else {
            0
        }
    };

    let tracks = [
        video_track(video_entry, &framed_video, width, height, fps),
        Track {
            handler: b"soun",
            handler_name: b"SoundHandler\0",
            media_header: smhd(),
            sample_entry: audio_sample_entry_aac(&asc, audio_channels, audio_rate, avg_bitrate),
            samples: &audio_samples,
            timescale: audio_rate.max(1),
            sample_delta: 1024,
            samples_per_chunk: frames_per_second,
            width: 0,
            height: 0,
            volume: 0x0100, // 1.0
            write_stss: false, // every audio sample is already a sync sample
        },
    ];
    Ok(assemble_tracks(&ftyp, &tracks))
}

/// Convenience: encode RGBA8 frames to H.264 and interleaved 16-bit PCM to
/// AAC-LC, then mux both into one `.mp4`.
///
/// This is the whole point of the audio track: a renderer that produces frames
/// and a soundtrack can emit a single finished file, with no external muxer.
#[allow(clippy::too_many_arguments)]
pub fn encode_rgba_h264_aac(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
    qp: i32,
    pcm: &[i16],
    audio_rate: u32,
    audio_channels: u16,
    audio_bitrate_kbps: u32,
) -> Result<Vec<u8>, String> {
    let (sps, pps, samples) = crate::h264::encode_mp4_track_qp(frames, width, height, qp)?;
    let adts = crate::aac::encode_pcm(pcm, audio_rate, audio_channels, audio_bitrate_kbps);
    mux_h264_aac(
        &sps,
        &pps,
        &samples,
        width,
        height,
        fps,
        &adts,
        audio_rate,
        audio_channels,
    )
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
mod av_tests {
    use super::*;

    /// Walk the top-level box list, returning (tag, payload) pairs.
    fn top_level(mp4: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= mp4.len() {
            let size = u32::from_be_bytes(mp4[i..i + 4].try_into().unwrap()) as usize;
            let tag = String::from_utf8_lossy(&mp4[i + 4..i + 8]).to_string();
            assert!(size >= 8 && i + size <= mp4.len(), "bad box {tag} size {size}");
            out.push((tag, &mp4[i + 8..i + size]));
            i += size;
        }
        assert_eq!(i, mp4.len(), "trailing bytes after last top-level box");
        out
    }

    /// Count occurrences of a 4-byte tag anywhere in the stream.
    fn count(hay: &[u8], tag: &[u8; 4]) -> usize {
        hay.windows(4).filter(|w| *w == tag).count()
    }

    fn silence(rate: u32, channels: u16, secs: f32) -> Vec<i16> {
        let n = (rate as f32 * secs) as usize * channels as usize;
        // A quiet ramp rather than true silence: an all-zero signal can encode
        // to degenerate frames, which would not exercise the muxer honestly.
        (0..n).map(|i| ((i % 64) as i16 - 32) * 4).collect()
    }

    #[test]
    fn adts_round_trips_to_raw_frames_and_config() {
        let rate = 44100;
        let adts = crate::aac::encode_pcm(&silence(rate, 1, 0.25), rate, 1, 64);
        let (frames, asc) = adts_to_raw_frames_for_test(&adts);
        assert!(!frames.is_empty(), "no frames recovered from ADTS");
        assert_eq!(asc.len(), 2, "AudioSpecificConfig must be 2 bytes");
        // objectType 2 (AAC-LC) in the top 5 bits.
        assert_eq!(asc[0] >> 3, 2, "expected AAC-LC object type");
        // sampling_frequency_index for 44100 is 4.
        let sf_index = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
        assert_eq!(sf_index, 4, "expected 44.1 kHz sample rate index");
        // Raw frames must have no syncword left on them.
        for f in &frames {
            assert!(
                !(f.len() >= 2 && f[0] == 0xFF && (f[1] & 0xF0) == 0xF0),
                "ADTS header survived into a raw frame"
            );
        }
    }

    fn adts_to_raw_frames_for_test(adts: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        crate::aac::adts_to_raw_frames(adts).expect("valid ADTS")
    }

    #[test]
    fn av_mux_writes_two_tracks_and_one_mdat() {
        let (w, h, fps) = (16u32, 16u32, 10u32);
        let frames: Vec<Vec<u8>> = (0..10).map(|t| super::tests::frame(16, 16, t)).collect();
        let rate = 44100;
        let mp4 = encode_rgba_h264_aac(
            &frames,
            w,
            h,
            fps,
            crate::h264::DEFAULT_QP,
            &silence(rate, 2, 1.0),
            rate,
            2,
            96,
        )
        .expect("A/V mux");

        let boxes = top_level(&mp4);
        let tags: Vec<&str> = boxes.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(tags, vec!["ftyp", "mdat", "moov"], "top-level box order");

        let moov = boxes.iter().find(|(t, _)| t == "moov").unwrap().1;
        assert_eq!(count(moov, b"trak"), 2, "expected exactly two traks");
        assert_eq!(count(moov, b"vide"), 1, "expected one video handler");
        assert_eq!(count(moov, b"soun"), 1, "expected one sound handler");
        assert_eq!(count(moov, b"vmhd"), 1);
        assert_eq!(count(moov, b"smhd"), 1);
        assert_eq!(count(moov, b"avc1"), 1);
        assert_eq!(count(moov, b"mp4a"), 1);
        assert_eq!(count(moov, b"esds"), 1);
        // Video is all-intra so it writes stss; audio must not.
        assert_eq!(count(moov, b"stss"), 1, "only the video track writes stss");
    }

    #[test]
    fn av_chunk_offsets_land_inside_mdat_and_interleave() {
        let (w, h, fps) = (16u32, 16u32, 10u32);
        let frames: Vec<Vec<u8>> = (0..20).map(|t| super::tests::frame(16, 16, t)).collect();
        let rate = 44100;
        let mp4 = encode_rgba_h264_aac(
            &frames, w, h, fps, crate::h264::DEFAULT_QP,
            &silence(rate, 1, 2.0), rate, 1, 64,
        )
        .expect("A/V mux");

        // Locate mdat's payload range.
        let mut i = 0usize;
        let mut mdat_range = None;
        while i + 8 <= mp4.len() {
            let size = u32::from_be_bytes(mp4[i..i + 4].try_into().unwrap()) as usize;
            if &mp4[i + 4..i + 8] == b"mdat" {
                mdat_range = Some((i + 8, i + size));
            }
            i += size;
        }
        let (mdat_start, mdat_end) = mdat_range.expect("mdat present");

        // Every stco entry in the file must point inside mdat.
        let mut offsets_per_track: Vec<Vec<u32>> = Vec::new();
        let mut j = 0usize;
        while j + 8 <= mp4.len() {
            if &mp4[j..j + 4] == b"stco" {
                let body = &mp4[j + 4..];
                let n = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                let mut offs = Vec::with_capacity(n);
                for k in 0..n {
                    let o = u32::from_be_bytes(body[8 + k * 4..12 + k * 4].try_into().unwrap());
                    assert!(
                        (o as usize) >= mdat_start && (o as usize) < mdat_end,
                        "chunk offset {o} outside mdat [{mdat_start},{mdat_end})"
                    );
                    offs.push(o);
                }
                offsets_per_track.push(offs);
            }
            j += 1;
        }
        assert_eq!(offsets_per_track.len(), 2, "one stco per track");

        // Interleaving: the first audio chunk must sit before the last video
        // chunk, which is exactly what concatenating the tracks would violate.
        let (a, b) = (&offsets_per_track[0], &offsets_per_track[1]);
        assert!(!a.is_empty() && !b.is_empty());
        assert!(
            b[0] < *a.last().unwrap(),
            "tracks are concatenated, not interleaved"
        );
    }

    #[test]
    fn av_mux_rejects_empty_inputs() {
        let frames = vec![super::tests::frame(16, 16, 0)];
        let (sps, pps, samples) =
            crate::h264::encode_mp4_track_qp(&frames, 16, 16, crate::h264::DEFAULT_QP).unwrap();
        assert!(mux_h264_aac(&sps, &pps, &[], 16, 16, 10, &[1, 2], 44100, 1).is_err());
        assert!(mux_h264_aac(&sps, &pps, &samples, 16, 16, 10, &[], 44100, 1).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn frame(w: usize, h: usize, t: usize) -> Vec<u8> {
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

// ═══════════════════════════════════════════════════════════════════════════
// Streaming writer
// ═══════════════════════════════════════════════════════════════════════════

/// Write an H.264 `.mp4` without ever holding the whole file in memory.
///
/// [`mux_h264`] returns a `Vec<u8>` of the finished file, so a twenty-second
/// 1080p render holds the encoded stream twice — once as samples, once as the
/// assembled result. This writes `ftyp` and `moov` first and then streams the
/// samples straight through into `mdat`, so the peak is the sample list alone.
///
/// Putting `moov` ahead of `mdat` is also what "faststart" means: a reader
/// learns the sample tables before it reaches the data, so playback can begin
/// without seeking to the end of the file.
///
/// The circularity — `moov` holds absolute chunk offsets, which depend on how
/// long `moov` itself is — is resolved by building it twice. Offsets are
/// fixed-width, so the second build is byte-identical in length to the first;
/// only the values differ. The assertion below states that invariant rather
/// than trusting it.
pub fn write_h264_mp4<W: std::io::Write>(
    out: &mut W,
    sps: &[u8],
    pps: &[u8],
    samples: &[Vec<u8>],
    width: u32,
    height: u32,
    fps: u32,
) -> Result<u64, String> {
    if samples.is_empty() {
        return Err("MP4: no samples".into());
    }
    let entry = avc1_sample_entry(sps, pps, width, height)?;

    // AVCC framing adds a 4-byte length prefix per sample; account for it in
    // the sizes without materialising the framed copies.
    let sizes: Vec<u32> = samples.iter().map(|s| (s.len() + 4) as u32).collect();

    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"isom");
    u32be(&mut ftyp_body, 0x200);
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        ftyp_body.extend_from_slice(brand);
    }
    let ftyp = boxed(b"ftyp", &ftyp_body);

    let build_moov = |mdat_payload_off: u64| -> Vec<u8> {
        let mut offsets = Vec::with_capacity(sizes.len());
        let mut at = mdat_payload_off;
        for s in &sizes {
            offsets.push(at as u32);
            at += *s as u64;
        }
        moov_for_video(&entry, &sizes, &offsets, width, height, fps)
    };

    // First build: placeholder offsets, to learn moov's length.
    let probe = build_moov(0);
    let mdat_payload_off = (ftyp.len() + probe.len() + 8) as u64;
    let moov = build_moov(mdat_payload_off);
    assert_eq!(
        probe.len(),
        moov.len(),
        "moov length must not depend on offset values"
    );

    let mdat_payload: u64 = sizes.iter().map(|s| *s as u64).sum();
    let mdat_total = mdat_payload + 8;
    if mdat_total > u32::MAX as u64 {
        return Err("MP4: mdat exceeds 4 GB; a 64-bit box is not implemented".into());
    }

    out.write_all(&ftyp).map_err(|e| e.to_string())?;
    out.write_all(&moov).map_err(|e| e.to_string())?;
    out.write_all(&(mdat_total as u32).to_be_bytes())
        .map_err(|e| e.to_string())?;
    out.write_all(b"mdat").map_err(|e| e.to_string())?;
    for s in samples {
        out.write_all(&(s.len() as u32).to_be_bytes())
            .map_err(|e| e.to_string())?;
        out.write_all(s).map_err(|e| e.to_string())?;
    }
    Ok(ftyp.len() as u64 + moov.len() as u64 + mdat_total)
}

/// Build a `moov` for a single all-intra video track with explicit offsets.
fn moov_for_video(
    sample_entry: &[u8],
    sizes: &[u32],
    offsets: &[u32],
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<u8> {
    let fps = fps.max(1);
    let n = sizes.len() as u32;
    let media_duration = n;
    let movie_duration = (n as u64 * MOVIE_TIMESCALE as u64 / fps as u64) as u32;

    let mut stsd_rest = Vec::new();
    u32be(&mut stsd_rest, 1);
    stsd_rest.extend_from_slice(sample_entry);
    let stsd = boxed(b"stsd", &fullbox_body(0, 0, &stsd_rest));

    let mut stts_rest = Vec::new();
    u32be(&mut stts_rest, 1);
    u32be(&mut stts_rest, n);
    u32be(&mut stts_rest, 1);
    let stts = boxed(b"stts", &fullbox_body(0, 0, &stts_rest));

    let mut stsc_rest = Vec::new();
    u32be(&mut stsc_rest, 1);
    u32be(&mut stsc_rest, 1);
    u32be(&mut stsc_rest, 1);
    u32be(&mut stsc_rest, 1);
    let stsc = boxed(b"stsc", &fullbox_body(0, 0, &stsc_rest));

    let mut stsz_rest = Vec::new();
    u32be(&mut stsz_rest, 0);
    u32be(&mut stsz_rest, n);
    for s in sizes {
        u32be(&mut stsz_rest, *s);
    }
    let stsz = boxed(b"stsz", &fullbox_body(0, 0, &stsz_rest));

    let mut stco_rest = Vec::new();
    u32be(&mut stco_rest, offsets.len() as u32);
    for o in offsets {
        u32be(&mut stco_rest, *o);
    }
    let stco = boxed(b"stco", &fullbox_body(0, 0, &stco_rest));

    let mut stss_rest = Vec::new();
    u32be(&mut stss_rest, n);
    for i in 1..=n {
        u32be(&mut stss_rest, i);
    }
    let stss = boxed(b"stss", &fullbox_body(0, 0, &stss_rest));

    let mut stbl_body = Vec::new();
    for b in [&stsd, &stts, &stsc, &stsz, &stco, &stss] {
        stbl_body.extend_from_slice(b);
    }
    let stbl = boxed(b"stbl", &stbl_body);

    let url = boxed(b"url ", &fullbox_body(0, 1, &[]));
    let mut dref_rest = Vec::new();
    u32be(&mut dref_rest, 1);
    dref_rest.extend_from_slice(&url);
    let dinf = boxed(b"dinf", &boxed(b"dref", &fullbox_body(0, 0, &dref_rest)));

    let mut minf_body = Vec::new();
    minf_body.extend_from_slice(&vmhd());
    minf_body.extend_from_slice(&dinf);
    minf_body.extend_from_slice(&stbl);
    let minf = boxed(b"minf", &minf_body);

    let mut mdhd_rest = Vec::new();
    u32be(&mut mdhd_rest, 0);
    u32be(&mut mdhd_rest, 0);
    u32be(&mut mdhd_rest, fps);
    u32be(&mut mdhd_rest, media_duration);
    u16be(&mut mdhd_rest, 0x55C4);
    u16be(&mut mdhd_rest, 0);
    let mdhd = boxed(b"mdhd", &fullbox_body(0, 0, &mdhd_rest));

    let mut hdlr_rest = Vec::new();
    u32be(&mut hdlr_rest, 0);
    hdlr_rest.extend_from_slice(b"vide");
    for _ in 0..3 {
        u32be(&mut hdlr_rest, 0);
    }
    hdlr_rest.extend_from_slice(b"VideoHandler\0");
    let hdlr = boxed(b"hdlr", &fullbox_body(0, 0, &hdlr_rest));

    let mut mdia_body = Vec::new();
    mdia_body.extend_from_slice(&mdhd);
    mdia_body.extend_from_slice(&hdlr);
    mdia_body.extend_from_slice(&minf);
    let mdia = boxed(b"mdia", &mdia_body);

    let mut tkhd_rest = Vec::new();
    u32be(&mut tkhd_rest, 0);
    u32be(&mut tkhd_rest, 0);
    u32be(&mut tkhd_rest, 1);
    u32be(&mut tkhd_rest, 0);
    u32be(&mut tkhd_rest, movie_duration);
    for _ in 0..2 {
        u32be(&mut tkhd_rest, 0);
    }
    u16be(&mut tkhd_rest, 0);
    u16be(&mut tkhd_rest, 0);
    u16be(&mut tkhd_rest, 0);
    u16be(&mut tkhd_rest, 0);
    for &v in &[0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32be(&mut tkhd_rest, v);
    }
    u32be(&mut tkhd_rest, width << 16);
    u32be(&mut tkhd_rest, height << 16);
    let tkhd = boxed(b"tkhd", &fullbox_body(0, 7, &tkhd_rest));

    let mut trak_body = Vec::new();
    trak_body.extend_from_slice(&tkhd);
    trak_body.extend_from_slice(&mdia);
    let trak = boxed(b"trak", &trak_body);

    let mut mvhd_rest = Vec::new();
    u32be(&mut mvhd_rest, 0);
    u32be(&mut mvhd_rest, 0);
    u32be(&mut mvhd_rest, MOVIE_TIMESCALE);
    u32be(&mut mvhd_rest, movie_duration);
    u32be(&mut mvhd_rest, 0x0001_0000);
    u16be(&mut mvhd_rest, 0x0100);
    u16be(&mut mvhd_rest, 0);
    for _ in 0..2 {
        u32be(&mut mvhd_rest, 0);
    }
    for &v in &[0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32be(&mut mvhd_rest, v);
    }
    for _ in 0..6 {
        u32be(&mut mvhd_rest, 0);
    }
    u32be(&mut mvhd_rest, 2);
    let mvhd = boxed(b"mvhd", &fullbox_body(0, 0, &mvhd_rest));

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd);
    moov_body.extend_from_slice(&trak);
    boxed(b"moov", &moov_body)
}
