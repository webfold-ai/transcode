// ═══════════════════════════════════════════════════════════════════════════
// av1_coeff — nonzero coefficient coding (arm ③), end-to-end vs ffmpeg/dav1d.
// A 64×64 KEY_FRAME with a single luma DC coefficient: skip=0, eob=0 (dc-only),
// the level coded via eob_base_tok + br_tok hi_tok (+ Exp-Golomb tail), dc_sign.
// The whole path was pinned to dav1d's per-symbol rng trace (av1_msac_trace).
// ═══════════════════════════════════════════════════════════════════════════
use std::process::Command;
fn ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
fn decode_y(stream: &[u8], tag: &str) -> Vec<u8> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let (o, y) = (format!("{dir}/c_{tag}.obu"), format!("{dir}/c_{tag}.yuv"));
    std::fs::write(&o, stream).unwrap();
    let st = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-i",
            &o,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            &y,
        ])
        .status()
        .unwrap();
    assert!(st.success(), "ffmpeg failed to decode {tag}");
    std::fs::read(&y).unwrap()[..4096].to_vec()
}

#[test]
fn dc_tile_reproduces_libaom_byte_exact() {
    // The (7, negative) coeff is exactly what libaom emitted for its grey still.
    let s = transcode::av1_frame::encode_dc_keyframe_64(7, true);
    assert_eq!(
        &s[19..],
        &[0x09, 0xac],
        "DC tile must reproduce libaom's `09 ac`"
    );
}

#[test]
fn dc_coeff_decodes_to_offset_grey() {
    if !ffmpeg() {
        eprintln!("SKIP: no ffmpeg");
        return;
    }
    // ±7 ⇒ symmetric ±2 around mid-grey (sign controls direction).
    let neg = decode_y(&transcode::av1_frame::encode_dc_keyframe_64(7, true), "n7");
    assert!(
        neg.iter().all(|&p| p == 126),
        "neg DC: expected uniform 126, got {:?}",
        &neg[..4]
    );
    let pos = decode_y(&transcode::av1_frame::encode_dc_keyframe_64(7, false), "p7");
    assert!(
        pos.iter().all(|&p| p == 130),
        "pos DC: expected uniform 130, got {:?}",
        &pos[..4]
    );
    // every level in the clean (pre-saturation) range decodes to a uniform frame
    for tok in [1u32, 3, 7, 14, 20, 40] {
        let y = decode_y(
            &transcode::av1_frame::encode_dc_keyframe_64(tok, false),
            &format!("s{tok}"),
        );
        let set: std::collections::HashSet<_> = y.iter().collect();
        assert_eq!(
            set.len(),
            1,
            "dc_tok={tok}: frame must be uniform, got {} values",
            set.len()
        );
    }
}

#[test]
fn ac_coeff_decodes_nonuniform() {
    if !ffmpeg() {
        eprintln!("SKIP: no ffmpeg");
        return;
    }
    // DC + one vertical-AC coeff ⇒ the decoded luma must NOT be flat.
    let y = decode_y(
        &transcode::av1_frame::encode_ac_keyframe_64(1, false, 1, false),
        "ac11",
    );
    let distinct: std::collections::HashSet<_> = y.iter().collect();
    assert!(
        distinct.len() >= 2,
        "AC coeff must produce spatial variation, got {} value(s)",
        distinct.len()
    );
    // a few level/sign variants must all decode without error
    for (d, dn, a, an) in [
        (1u32, false, 1u32, false),
        (2, true, 1, false),
        (1, false, 2, true),
        (2, true, 2, true),
    ] {
        let s = transcode::av1_frame::encode_ac_keyframe_64(d, dn, a, an);
        let _ = decode_y(&s, &format!("ac{d}{}{a}{}", dn as u8, an as u8)); // asserts ffmpeg success inside
    }
}
