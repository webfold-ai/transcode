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
