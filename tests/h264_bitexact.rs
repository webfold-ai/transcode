// ═══════════════════════════════════════════════════════════════════════════
// h264_bitexact — verify our intra H.264 encoder against ffmpeg's decoder.
//
// The contract for a sovereign encoder: a real decoder, fed our bitstream, must
// reconstruct exactly the samples our encoder reconstructed. We test both the
// I_16x16 path and the I_4x4 (I_NxN) path on several patterns over a QP sweep.
// ffmpeg decodes the Annex-B stream to planar yuv420p; we compare byte-for-byte
// against the encoder's own (cropped) reconstruction.
//
// If ffmpeg is not installed the test prints a skip notice and passes — the
// encoder still builds and the unit tests still run; only the oracle is absent.
// ═══════════════════════════════════════════════════════════════════════════

use std::process::Command;

const W: u32 = 64;
const H: u32 = 48;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// Deterministic test patterns as RGBA8 (W×H).
fn pattern(kind: &str) -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (r, g, b) = match kind {
                "flat" => (128u8, 128, 128),
                "ramp" => {
                    let v = (x * 255 / (w - 1)) as u8;
                    (v, v, v)
                }
                "color" => {
                    // diagonal colour bands — exercises chroma + directional modes
                    let r = ((x + y) * 255 / (w + h)) as u8;
                    let g = (y * 255 / (h - 1)) as u8;
                    let b = (x * 255 / (w - 1)) as u8;
                    (r, g, b)
                }
                "noise" => {
                    // deterministic LCG pseudo-random — stresses every mode/coeff
                    let s = (y * w + x) as u32;
                    let n = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((n >> 16) as u8, (n >> 8) as u8, n as u8)
                }
                _ => (0, 0, 0),
            };
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    rgba
}

// Decode an Annex-B H.264 stream with ffmpeg → concatenated yuv420p frames.
fn ffmpeg_decode_yuv420p(stream: &[u8], tag: &str) -> Vec<u8> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let inp = format!("{dir}/h264_{tag}.264");
    let outp = format!("{dir}/h264_{tag}.yuv");
    std::fs::write(&inp, stream).expect("write .264");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "h264",
            "-i",
            &inp,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            &outp,
        ])
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg decode failed for {tag}");
    std::fs::read(&outp).expect("read decoded yuv")
}

// Compare ffmpeg's decoded yuv420p against the encoder's reconstruction,
// frame by frame, byte-exact.
fn assert_bitexact(
    decoded: &[u8],
    recons: &[(Vec<u8>, Vec<u8>, Vec<u8>)],
    label: &str,
) {
    let (w, h) = (W as usize, H as usize);
    let ysz = w * h;
    let csz = (w / 2) * (h / 2);
    let fsz = ysz + 2 * csz;
    assert_eq!(
        decoded.len(),
        fsz * recons.len(),
        "{label}: decoded byte count {} != expected {}",
        decoded.len(),
        fsz * recons.len()
    );
    for (f, (ry, rcb, rcr)) in recons.iter().enumerate() {
        let base = f * fsz;
        let dy = &decoded[base..base + ysz];
        let dcb = &decoded[base + ysz..base + ysz + csz];
        let dcr = &decoded[base + ysz + csz..base + fsz];
        for (plane, (dec, ours)) in
            [("Y", (dy, ry)), ("Cb", (dcb, rcb)), ("Cr", (dcr, rcr))].into_iter()
        {
            assert_eq!(dec.len(), ours.len(), "{label} f{f} {plane}: size mismatch");
            if let Some((idx, (&a, &b))) =
                dec.iter().zip(ours.iter()).enumerate().find(|(_, (a, b))| a != b)
            {
                let ndiff = dec.iter().zip(ours.iter()).filter(|(a, b)| a != b).count();
                panic!(
                    "{label} f{f} {plane}: NOT bit-exact — {ndiff} differing samples; \
                     first at idx {idx}: ffmpeg={a} ours={b}"
                );
            }
        }
    }
}

fn run_path<F>(label: &str, encode: F)
where
    F: Fn(&[Vec<u8>], u32, u32, i32) -> Result<(Vec<u8>, Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>), String>,
{
    for kind in ["flat", "ramp", "color", "noise"] {
        let frames = vec![pattern(kind)];
        for qp in [18i32, 26, 34, 40] {
            let (stream, recons) = encode(&frames, W, H, qp).expect("encode");
            let tag = format!("{label}_{kind}_q{qp}");
            let decoded = ffmpeg_decode_yuv420p(&stream, &tag);
            assert_bitexact(&decoded, &recons, &format!("{label}/{kind}/qp{qp}"));
        }
    }
}

#[test]
fn i16x16_decodes_bitexact_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available — cannot run the decode oracle");
        return;
    }
    run_path("i16", transcode::h264_intra::encode_sequence_with_recon);
}

#[test]
fn i4x4_decodes_bitexact_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available — cannot run the decode oracle");
        return;
    }
    run_path("i4x4", transcode::h264_intra::encode_sequence_i4x4_with_recon);
}

#[test]
fn adaptive_decodes_bitexact_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available — cannot run the decode oracle");
        return;
    }
    // The per-MB free-energy decision mixes I_16x16 and I_4x4 MBs in one slice;
    // ffmpeg must still reconstruct every MB exactly.
    run_path("adaptive", transcode::h264_intra::encode_sequence_adaptive_with_recon);
}

#[test]
fn mp4_product_path_decodes_in_ffmpeg() {
    // The public product path: RGBA → compressed adaptive H.264 → MP4 mux
    // (avcC from SPS/PPS, length-prefixed samples). ffmpeg must demux + decode
    // it as a real H.264 yuv420p track at the right dimensions.
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available");
        return;
    }
    let frame = pattern("color");
    let mp4 = transcode::mp4::encode_rgba_h264_qp(&[frame], W, H, 1, 26).expect("mux mp4");
    let dir = env!("CARGO_TARGET_TMPDIR");
    let path = format!("{dir}/product.mp4");
    std::fs::write(&path, &mp4).expect("write mp4");
    // demux + decode with no errors
    let dec = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i", &path, "-f", "null", "-"])
        .status()
        .expect("spawn ffmpeg");
    assert!(dec.success(), "ffmpeg failed to decode the muxed MP4");
    // ffprobe: it really is H.264 at W×H
    let probe = Command::new("ffprobe")
        .args([
            "-hide_banner", "-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=codec_name,width,height",
            "-of", "csv=p=0", &path,
        ])
        .output()
        .expect("spawn ffprobe");
    let s = String::from_utf8_lossy(&probe.stdout);
    assert!(s.contains("h264"), "not an h264 track: {s}");
    assert!(s.contains(&W.to_string()) && s.contains(&H.to_string()), "wrong dims: {s}");
    // the compressed MP4 must be far smaller than raw I_PCM (≈ YUV420 raw size)
    let ipcm_floor = (W * H * 3 / 2) as usize;
    assert!(mp4.len() < ipcm_floor, "not compressed: {} >= {}", mp4.len(), ipcm_floor);
}

// `cargo test --release --test h264_bitexact -- --ignored --nocapture report_sizes`
// Bitstream-size comparison of the two intra paths over a QP sweep. Both are
// decoder-exact (asserted above), so this isolates coding efficiency. I_4x4
// wins where directional prediction tracks the content (e.g. gradients) and
// loses where the per-block mode signalling (16 modes/MB) isn't recouped —
// there is no per-MB I_16x16-vs-I_4x4 rate-distortion decision yet, so every
// MB here is forced to one type.
#[test]
#[ignore]
fn report_sizes() {
    for kind in ["flat", "ramp", "color", "noise"] {
        let frames = vec![pattern(kind)];
        println!("\n[{kind}]");
        for qp in [18i32, 26, 34, 40] {
            let (s16, _) = transcode::h264_intra::encode_sequence_with_recon(&frames, W, H, qp).unwrap();
            let (s4, _) = transcode::h264_intra::encode_sequence_i4x4_with_recon(&frames, W, H, qp).unwrap();
            let (sa, _) = transcode::h264_intra::encode_sequence_adaptive_with_recon(&frames, W, H, qp).unwrap();
            let best = s16.len().min(s4.len());
            println!(
                "  qp{qp:>2}: i16={:>5}B  i4x4={:>5}B  adaptive={:>5}B  (vs best-fixed {:+.0}%)",
                s16.len(),
                s4.len(),
                sa.len(),
                100.0 * (sa.len() as f64 - best as f64) / best as f64,
            );
        }
    }
}

// ── A/V mux: the muxed file must be readable by a real demuxer ──────────────
// The unit tests in src/mp4.rs check our own box tree against our own
// assumptions. This checks it against ffmpeg, which is the only opinion that
// matters for a container format.

#[test]
fn av_mux_decodes_both_streams_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }
    let (w, h, fps) = (64u32, 64u32, 10u32);
    let frames: Vec<Vec<u8>> = (0..20)
        .map(|t| {
            let mut px = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = (y * w as usize + x) * 4;
                    px[i] = (x * 4 + t * 8) as u8;
                    px[i + 1] = (y * 4) as u8;
                    px[i + 2] = (t * 12) as u8;
                    px[i + 3] = 255;
                }
            }
            px
        })
        .collect();
    let rate = 44100u32;
    let pcm: Vec<i16> = (0..rate as usize * 2)
        .map(|i| (((i % 128) as i16) - 64) * 200)
        .collect();

    let mp4 = transcode::mp4::encode_rgba_h264_aac(
        &frames,
        w,
        h,
        fps,
        transcode::h264::DEFAULT_QP,
        &pcm,
        rate,
        1,
        96,
    )
    .expect("A/V mux");

    let path = std::env::temp_dir().join("transcode_av_mux_test.mp4");
    std::fs::write(&path, &mp4).expect("write mp4");

    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_type,codec_name",
            "-of", "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe rejected the file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let streams = String::from_utf8_lossy(&out.stdout);
    assert!(
        streams.contains("h264") && streams.contains("video"),
        "no H.264 video stream found: {streams}"
    );
    assert!(
        streams.contains("aac") && streams.contains("audio"),
        "no AAC audio stream found: {streams}"
    );

    // Demux and decode both streams for real — a container can list streams it
    // cannot actually deliver.
    for (kind, args) in [
        ("video", ["-map", "0:v:0", "-f", "rawvideo"]),
        ("audio", ["-map", "0:a:0", "-f", "s16le"]),
    ] {
        let status = Command::new("ffmpeg")
            .arg("-v").arg("error")
            .arg("-i").arg(&path)
            .args(args)
            .arg("-y").arg("/dev/null")
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "ffmpeg failed to decode the {kind} stream");
    }
    let _ = std::fs::remove_file(&path);
}

// ── streaming writer ───────────────────────────────────────────────────────

#[test]
fn streamed_mp4_decodes_and_matches_the_in_memory_muxer() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }
    let (w, h, fps) = (64u32, 64u32, 10u32);
    let frames: Vec<Vec<u8>> = (0..12)
        .map(|t| {
            let mut px = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = (y * w as usize + x) * 4;
                    px[i] = (x * 3 + t * 9) as u8;
                    px[i + 1] = (y * 5) as u8;
                    px[i + 2] = ((x ^ y) + t) as u8;
                    px[i + 3] = 255;
                }
            }
            px
        })
        .collect();
    let (sps, pps, samples) =
        transcode::h264::encode_mp4_track_qp(&frames, w, h, transcode::h264::DEFAULT_QP).unwrap();

    let mut streamed = Vec::new();
    let n = transcode::mp4::write_h264_mp4(&mut streamed, &sps, &pps, &samples, w, h, fps).unwrap();
    assert_eq!(n as usize, streamed.len(), "reported length must match output");

    let in_memory = transcode::mp4::mux_h264(&sps, &pps, &samples, w, h, fps).unwrap();
    // The two are not byte-identical and are not meant to be: the in-memory
    // muxer groups `fps` samples per chunk while the streaming writer emits one
    // sample per chunk, which trades a larger `stco` for a smaller `stsc`. What
    // must match is the payload and what a decoder reads back out.
    let payload: usize = samples.iter().map(|s| s.len() + 4).sum();
    assert!(streamed.len() > payload, "output must contain the samples");
    assert!(
        (streamed.len() as i64 - in_memory.len() as i64).abs() < 1024,
        "layouts differ by chunking only: {} vs {}",
        streamed.len(),
        in_memory.len()
    );

    let path = std::env::temp_dir().join("transcode_streamed.mp4");
    std::fs::write(&path, &streamed).unwrap();
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-count_frames",
            "-show_entries", "stream=codec_name,width,height,nb_read_frames",
            "-of", "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe rejected the streamed file");
    let info = String::from_utf8_lossy(&out.stdout);
    assert!(info.contains("h264"), "no h264 stream: {info}");
    assert!(info.contains("12"), "expected 12 decodable frames: {info}");
    let _ = std::fs::remove_file(&path);
}
