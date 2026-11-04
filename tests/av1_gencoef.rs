// Encode a 64×64 KEY_FRAME whose luma block carries DC + 2 AC coeffs via the
// unified grammar (encode_block_2d), then dav1d must decode them. Confirms the
// grammar ≡ dav1d (round-trip already proves Enc≡Dec).
use std::process::Command;
use transcode::av1_coef::{encode_block_2d, CoefCdf};
use transcode::av1_ec::Writer;
use transcode::av1_obu::{
    sequence_header_obu, temporal_delimiter, wrap_obu, write_frame_header, OBU_FRAME,
};
use transcode::h264bits::BitWriter;
fn cdf(a: &[u16]) -> Vec<u16> {
    let mut v: Vec<u16> = a.iter().map(|&x| 32768 - x).collect();
    v.push(0);
    v.push(0);
    v
}
fn bcdf(a: u16) -> [u16; 2] {
    [32768 - a, 0]
}
fn ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn build(coeffs: &[i32]) -> Vec<u8> {
    let part = [
        20137u16, 21547, 23078, 29566, 29837, 30261, 30524, 30892, 31724,
    ];
    let ym = [
        15588u16, 17027, 19338, 20218, 20682, 21110, 21825, 23244, 24189, 28165, 29093, 30466,
    ];
    let uv = [
        22631u16, 24152, 25378, 25661, 25986, 26520, 27055, 27923, 28244, 30059, 30941, 31961,
    ];
    let mut w = Writer::new();
    let mut c = cdf(&part);
    w.symbol(0, &mut c, 10);
    let mut s = bcdf(31671);
    w.bool_adapt(false, &mut s);
    let mut c = cdf(&ym);
    w.symbol(0, &mut c, 13);
    let mut c = cdf(&uv);
    w.symbol(0, &mut c, 13);
    let mut cc = CoefCdf::new();
    encode_block_2d(&mut w, &mut cc, coeffs);
    let mut cz = bcdf(4656);
    w.bool_adapt(true, &mut cz);
    w.bool_adapt(true, &mut cz);
    let tile = w.finish();
    let mut out = temporal_delimiter();
    out.extend_from_slice(&sequence_header_obu(64, 64));
    let mut fh = BitWriter::new();
    write_frame_header(&mut fh, 64, 64, 128);
    fh.align_zero();
    let mut pl = fh.into_rbsp();
    pl.extend_from_slice(&tile);
    out.extend_from_slice(&wrap_obu(OBU_FRAME, &pl));
    out
}
#[test]
fn gencoef_decodes_in_ffmpeg() {
    if !ffmpeg() {
        eprintln!("SKIP");
        return;
    }
    let mut coeffs = vec![0i32; 1024];
    coeffs[0] = 2;
    coeffs[32] = 1;
    coeffs[1] = 3;
    let s = build(&coeffs);
    let dir = env!("CARGO_TARGET_TMPDIR");
    std::fs::write(format!("{dir}/gc.obu"), &s).unwrap();
    let st = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-i",
            &format!("{dir}/gc.obu"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            &format!("{dir}/gc.yuv"),
        ])
        .status()
        .unwrap();
    assert!(st.success(), "ffmpeg failed to decode multi-coeff frame");
    let d = std::fs::read(format!("{dir}/gc.yuv")).unwrap();
    let y = &d[..4096];
    let distinct: std::collections::HashSet<_> = y.iter().collect();
    eprintln!("Y distinct={} sample={:?}", distinct.len(), &y[..8]);
    assert!(distinct.len() >= 2, "multi-coeff residual must be non-flat");
}
