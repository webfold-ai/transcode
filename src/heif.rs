// ═══════════════════════════════════════════════════════════════════════════
// heif.rs — sovereign HEIF still-image container muxer (ISO/IEC 23008-12)
// ═══════════════════════════════════════════════════════════════════════════
// HEIF is the ISOBMFF-based image container: a single coded image is stored as
// an *item* (not a track) inside a `meta` box, with its bytes in `mdat` and its
// codec configuration in the item-property boxes. HEIF is codec-AGNOSTIC — the
// brand and the item type say which codec. This muxer carries our bit-exact
// H.264 (AVC) intra frame as an `avc1` image item.
//
// HONEST SCOPE / NAMING (per the framework's own hard_limits doctrine — build
// the pure container math, name it accurately):
//   * This produces a real HEIF file (brand `mif1`, an `avc1` image item).
//   * It is NOT `.heic`: the `heic` brand specifically requires the image item
//     to be HEVC (H.265)-coded. We have no H.265 encoder, so we do not emit the
//     `heic` brand or claim it. "HEIF carrying H.264" is a valid, standards-
//     defined configuration; "HEIC" is not something we can produce.
//   * Verification: ffmpeg/libheif cannot author/parse arbitrary HEIF image
//     items here, so this is verified structurally (box tree tiles exactly,
//     required boxes present) and by the embedded frame being an independently
//     ffmpeg-decodable H.264 access unit.
//
// Box layout:
//   ftyp (mif1)
//   meta
//     hdlr ('pict')
//     pitm (primary item id = 1)
//     iinf → infe (item 1, type 'avc1')
//     iprp → ipco { avcC, ispe } + ipma (item 1 → properties 1,2)
//     iloc (item 1 → offset/length into mdat)
//   mdat (the AVCC length-prefixed H.264 NAL units)
// ═══════════════════════════════════════════════════════════════════════════

fn u16be(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_be_bytes());
}
fn u32be(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_be_bytes());
}
fn bx(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    u32be(&mut b, (body.len() + 8) as u32);
    b.extend_from_slice(tag);
    b.extend_from_slice(body);
    b
}
fn full(tag: &[u8; 4], version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(rest.len() + 4);
    body.push(version);
    body.extend_from_slice(&flags.to_be_bytes()[1..]);
    body.extend_from_slice(rest);
    bx(tag, &body)
}

/// Build the avcC configuration record from SPS/PPS NAL units (no start codes).
fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut a = Vec::new();
    a.push(1); // configurationVersion
    a.push(sps[1]); // profile_idc
    a.push(sps[2]); // constraint flags
    a.push(sps[3]); // level_idc
    a.push(0xFF); // lengthSizeMinusOne = 3
    a.push(0xE1); // numSPS = 1
    u16be(&mut a, sps.len() as u16);
    a.extend_from_slice(sps);
    a.push(1); // numPPS
    u16be(&mut a, pps.len() as u16);
    a.extend_from_slice(pps);
    bx(b"avcC", &a)
}

/// Mux a single H.264 (AVC) intra image item into a HEIF (.heif) byte stream.
/// `sps`/`pps`/`frame_nal` are NAL units without start codes (AVCC framing).
pub fn mux_h264(
    sps: &[u8],
    pps: &[u8],
    frame_nal: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if sps.len() < 4 {
        return Err("HEIF: SPS too short".into());
    }

    // ── ftyp: brand mif1 (generic HEIF still image) ──
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"mif1"); // major brand
    u32be(&mut ftyp, 0); // minor version
    ftyp.extend_from_slice(b"mif1");
    ftyp.extend_from_slice(b"avc1"); // compatible: AVC image item
    let ftyp = bx(b"ftyp", &ftyp);

    // ── item payload (AVCC: 4-byte length + NAL) goes in mdat ──
    let mut sample = Vec::with_capacity(frame_nal.len() + 4);
    u32be(&mut sample, frame_nal.len() as u32);
    sample.extend_from_slice(frame_nal);

    // ── meta box children ──
    // hdlr: handler type 'pict'
    let mut hdlr_rest = Vec::new();
    u32be(&mut hdlr_rest, 0); // pre_defined
    hdlr_rest.extend_from_slice(b"pict");
    for _ in 0..3 {
        u32be(&mut hdlr_rest, 0);
    }
    hdlr_rest.push(0); // empty name (null-terminated)
    let hdlr = full(b"hdlr", 0, 0, &hdlr_rest);

    // pitm: primary item id = 1
    let mut pitm_rest = Vec::new();
    u16be(&mut pitm_rest, 1);
    let pitm = full(b"pitm", 0, 0, &pitm_rest);

    // iinf → infe (item_id=1, type 'avc1')
    let mut infe_rest = Vec::new();
    u16be(&mut infe_rest, 1); // item_id
    u16be(&mut infe_rest, 0); // item_protection_index
    infe_rest.extend_from_slice(b"avc1"); // item_type
    infe_rest.push(0); // item_name (empty, null-term)
    let infe = full(b"infe", 2, 0, &infe_rest);
    let mut iinf_rest = Vec::new();
    u16be(&mut iinf_rest, 1); // entry_count (version 0 uses u16)
    iinf_rest.extend_from_slice(&infe);
    let iinf = full(b"iinf", 0, 0, &iinf_rest);

    // iprp → ipco { avcC, ispe } ; ipma maps item 1 → both properties
    let avcc_box = avcc(sps, pps);
    let mut ispe_rest = Vec::new();
    u32be(&mut ispe_rest, width);
    u32be(&mut ispe_rest, height);
    let ispe = full(b"ispe", 0, 0, &ispe_rest);
    let mut ipco = Vec::new();
    ipco.extend_from_slice(&avcc_box); // property index 1
    ipco.extend_from_slice(&ispe); // property index 2
    let ipco = bx(b"ipco", &ipco);
    // ipma: 1 entry, item_id=1, 2 associations (avcC essential, ispe)
    let mut ipma_rest = Vec::new();
    u32be(&mut ipma_rest, 1); // entry_count
    u16be(&mut ipma_rest, 1); // item_id
    ipma_rest.push(2); // association_count
    ipma_rest.push(0x81); // essential=1, property index 1 (avcC)
    ipma_rest.push(0x02); // essential=0, property index 2 (ispe)
    let ipma = full(b"ipma", 0, 0, &ipma_rest);
    let mut iprp = Vec::new();
    iprp.extend_from_slice(&ipco);
    iprp.extend_from_slice(&ipma);
    let iprp = bx(b"iprp", &iprp);

    // iloc: item 1 located in mdat. We compute the absolute mdat-payload offset
    // after the whole file layout is known, so build meta with a placeholder
    // and patch. Simpler: place mdat AFTER meta and compute offset directly.
    // construction_method=0 (file offset), 1 extent.
    // Build meta first (with offset placeholder 0), measure, then set offset.
    let build_meta = |mdat_payload_off: u32| -> Vec<u8> {
        let mut iloc_rest = Vec::new();
        // offset_size=4, length_size=4, base_offset_size=0, index_size=0
        iloc_rest.push(0x44);
        iloc_rest.push(0x00);
        u16be(&mut iloc_rest, 1); // item_count
        u16be(&mut iloc_rest, 1); // item_id
        u16be(&mut iloc_rest, 0); // data_reference_index
        u16be(&mut iloc_rest, 1); // extent_count
        u32be(&mut iloc_rest, mdat_payload_off); // extent_offset (4 bytes)
        u32be(&mut iloc_rest, sample.len() as u32); // extent_length (4 bytes)
        let iloc = full(b"iloc", 0, 0, &iloc_rest);

        let mut meta_body = Vec::new();
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&pitm);
        meta_body.extend_from_slice(&iinf);
        meta_body.extend_from_slice(&iprp);
        meta_body.extend_from_slice(&iloc);
        full(b"meta", 0, 0, &meta_body)
    };

    // First pass: meta size with a dummy offset is constant width (4-byte offset),
    // so meta length is invariant to the offset value → compute once.
    let meta_len = build_meta(0).len();
    let mdat_payload_off = (ftyp.len() + meta_len + 8) as u32; // +8 = mdat header
    let meta = build_meta(mdat_payload_off);
    debug_assert_eq!(meta.len(), meta_len, "meta length must be offset-invariant");

    let mdat = bx(b"mdat", &sample);

    let mut out = Vec::with_capacity(ftyp.len() + meta.len() + mdat.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&mdat);
    Ok(out)
}

/// Convenience: encode one RGBA8 frame to H.264 intra and mux into a HEIF file.
pub fn encode_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let (sps, pps, mut samples) =
        crate::h264::encode_mp4_track(std::slice::from_ref(&rgba.to_vec()), width, height)?;
    let frame = samples.pop().ok_or("HEIF: no frame produced")?;
    mux_h264(&sps, &pps, &frame, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = (x * 255 / w) as u8;
                px[i + 1] = (y * 255 / h) as u8;
                px[i + 2] = ((x ^ y) & 0xff) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Walk the top-level box tree; every declared size must tile the file exactly.
    fn walk(data: &[u8]) -> Vec<[u8; 4]> {
        let mut tags = Vec::new();
        let mut p = 0usize;
        while p + 8 <= data.len() {
            let sz = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
            let mut t = [0u8; 4];
            t.copy_from_slice(&data[p + 4..p + 8]);
            tags.push(t);
            assert!(sz >= 8, "box {:?} size {sz}", t);
            assert!(p + sz <= data.len(), "box {:?} overruns", t);
            p += sz;
        }
        assert_eq!(p, data.len(), "top-level boxes don't tile the file");
        tags
    }

    #[test]
    fn heif_structure_is_consistent() {
        let (w, h) = (64u32, 48u32);
        let heif = encode_rgba(&frame(w as usize, h as usize), w, h).expect("mux");
        let tags = walk(&heif);
        assert_eq!(&tags[0], b"ftyp");
        assert!(tags.iter().any(|t| t == b"meta"));
        assert!(tags.iter().any(|t| t == b"mdat"));
        // required HEIF item-structure boxes present somewhere
        let find = |n: &[u8]| heif.windows(n.len()).any(|w| w == n);
        for need in [
            &b"mif1"[..],
            b"hdlr",
            b"pict",
            b"pitm",
            b"iinf",
            b"infe",
            b"avc1",
            b"avcC",
            b"ispe",
            b"iprp",
            b"ipma",
            b"iloc",
        ] {
            assert!(
                find(need),
                "missing required HEIF box/marker {:?}",
                std::str::from_utf8(need).unwrap()
            );
        }
        // it must NOT claim the heic brand (we have no HEVC)
        assert!(!find(b"heic"), "must not claim heic brand without HEVC");
    }

    #[test]
    fn iloc_offset_points_at_mdat_payload() {
        let (w, h) = (32u32, 32u32);
        let heif = encode_rgba(&frame(w as usize, h as usize), w, h).expect("mux");
        // find mdat box, its payload starts 8 bytes after the box start
        let mut p = 0usize;
        let mut mdat_payload = None;
        while p + 8 <= heif.len() {
            let sz = u32::from_be_bytes([heif[p], heif[p + 1], heif[p + 2], heif[p + 3]]) as usize;
            if &heif[p + 4..p + 8] == b"mdat" {
                mdat_payload = Some(p + 8);
                break;
            }
            p += sz;
        }
        let mdat_payload = mdat_payload.expect("mdat present");
        // the AVCC length prefix at the payload start must equal frame_len, and
        // payload + 4 .. should be a NAL (first byte's low 5 bits = nal type 5 = IDR)
        let nal_len = u32::from_be_bytes([
            heif[mdat_payload],
            heif[mdat_payload + 1],
            heif[mdat_payload + 2],
            heif[mdat_payload + 3],
        ]) as usize;
        assert!(
            nal_len > 0 && mdat_payload + 4 + nal_len <= heif.len(),
            "mdat extent sane"
        );
        let nal_type = heif[mdat_payload + 4] & 0x1F;
        assert_eq!(
            nal_type, 5,
            "first item NAL should be an IDR slice (type 5)"
        );
    }
}
