// ═══════════════════════════════════════════════════════════════════════════
// av1_decode — verify our sovereign AV1 encoder against ffmpeg/libdav1d.
// The contract: our bitstream, fed to a real AV1 decoder, must decode to the
// frame our encoder intended. Skipped (passes) if ffmpeg is absent.
// ═══════════════════════════════════════════════════════════════════════════
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn flat_keyframe_decodes_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not available");
        return;
    }
    let stream = transcode::av1_frame::encode_flat_keyframe_64();
    let dir = env!("CARGO_TARGET_TMPDIR");
    let obu = format!("{dir}/av1_flat.obu");
    let yuv = format!("{dir}/av1_flat.yuv");
    std::fs::write(&obu, &stream).expect("write obu");

    // decode to planar yuv420p
    let st = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-i",
            &obu,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            &yuv,
        ])
        .status()
        .expect("spawn ffmpeg");
    assert!(st.success(), "ffmpeg failed to decode our AV1 stream");

    let d = std::fs::read(&yuv).expect("read yuv");
    // 64×64 4:2:0 = 4096 Y + 1024 U + 1024 V
    assert_eq!(d.len(), 64 * 64 + 2 * 32 * 32, "decoded frame size");
    let (y, u, v) = (&d[..4096], &d[4096..5120], &d[5120..]);
    // flat DC prediction, no residual ⇒ every sample is mid-grey 128
    assert!(
        y.iter().all(|&p| p == 128),
        "Y not uniform 128: {:?}",
        &y[..8]
    );
    assert!(u.iter().all(|&p| p == 128), "U not uniform 128");
    assert!(v.iter().all(|&p| p == 128), "V not uniform 128");
}
