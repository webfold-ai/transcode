// ═══════════════════════════════════════════════════════════════════════════
// mp3.rs — MPEG-1/2/2.5 Layer III parser (step 1: demux + headers + duration)
// ═══════════════════════════════════════════════════════════════════════════
// Frame-level parsing: ID3 skip, sync, header decode (version/layer/bitrate/
// samplerate/channels/padding), frame-length + samples-per-frame, frame walk.
// Full audio decode (Huffman → requant → IMDCT → synthesis filterbank → PCM)
// builds on this — those are the next increments.
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Version {
    Mpeg1,
    Mpeg2,
    Mpeg25,
}

pub struct Mp3Info {
    pub version: Version,
    pub layer: u8,
    pub samplerate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: u64, // per channel
    pub bitrate_kbps_first: u32,
    pub vbr: bool,
}

// Layer III bitrate tables (kbps), indexed by the 4-bit bitrate index.
const BR_MPEG1: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
const BR_MPEG2: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0];
const SR_MPEG1: [u32; 3] = [44100, 48000, 32000];
const SR_MPEG2: [u32; 3] = [22050, 24000, 16000];
const SR_MPEG25: [u32; 3] = [11025, 12000, 8000];

fn syncsafe(b: &[u8]) -> usize {
    ((b[0] as usize & 0x7f) << 21)
        | ((b[1] as usize & 0x7f) << 14)
        | ((b[2] as usize & 0x7f) << 7)
        | (b[3] as usize & 0x7f)
}

/// Parse one frame header at `data[p..]`. Returns (frame_len_bytes,
/// samples_per_frame, samplerate, channels, bitrate_kbps) if valid. -/
fn parse_header(data: &[u8], p: usize) -> Option<(usize, u32, u32, u16, u32)> {
    if p + 4 > data.len() {
        return None;
    }
    let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
    if (h >> 21) & 0x7FF != 0x7FF {
        return None; // sync
    }
    let version = match (h >> 19) & 0b11 {
        0b00 => Version::Mpeg25,
        0b10 => Version::Mpeg2,
        0b11 => Version::Mpeg1,
        _ => return None, // reserved
    };
    let layer = match (h >> 17) & 0b11 {
        0b01 => 3u8,
        0b10 => 2,
        0b11 => 1,
        _ => return None,
    };
    if layer != 3 {
        return None; // Layer III only here
    }
    let br_idx = ((h >> 12) & 0xF) as usize;
    let sr_idx = ((h >> 10) & 0b11) as usize;
    let padding = ((h >> 9) & 1) as usize;
    let mode = (h >> 6) & 0b11;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let bitrate = match version {
        Version::Mpeg1 => BR_MPEG1[br_idx],
        _ => BR_MPEG2[br_idx],
    };
    let samplerate = match version {
        Version::Mpeg1 => SR_MPEG1[sr_idx],
        Version::Mpeg2 => SR_MPEG2[sr_idx],
        Version::Mpeg25 => SR_MPEG25[sr_idx],
    };
    if bitrate == 0 {
        return None;
    }
    let channels = if mode == 0b11 { 1u16 } else { 2 };
    let spf: u32 = if version == Version::Mpeg1 { 1152 } else { 576 };
    // frame length in bytes
    let len = (spf as usize / 8) * (bitrate as usize * 1000) / samplerate as usize + padding;
    if len < 4 {
        return None;
    }
    Some((len, spf, samplerate, channels, bitrate))
}

/// Demux/parse a Layer III stream: walk frames, total duration, VBR detection. -/
pub fn parse(data: &[u8]) -> Result<Mp3Info, String> {
    let mut p = 0usize;
    // skip ID3v2 tag
    if data.len() > 10 && &data[0..3] == b"ID3" {
        p = 10 + syncsafe(&data[6..10]);
    }
    // find first valid frame
    while p + 4 <= data.len() && parse_header(data, p).is_none() {
        p += 1;
    }
    let first = parse_header(data, p).ok_or("no MPEG Layer III frame found")?;
    let (version, layer) = {
        let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let v = match (h >> 19) & 0b11 { 0b00 => Version::Mpeg25, 0b10 => Version::Mpeg2, _ => Version::Mpeg1 };
        (v, 3u8)
    };
    let (samplerate, channels, br0) = (first.2, first.3, first.4);

    let mut frames = 0u64;
    let mut samples = 0u64;
    let mut vbr = false;
    loop {
        match parse_header(data, p) {
            Some((len, spf, sr, ch, br)) if sr == samplerate && ch == channels => {
                if br != br0 {
                    vbr = true;
                }
                frames += 1;
                samples += spf as u64;
                p += len;
                if p + 4 > data.len() {
                    break;
                }
            }
            _ => {
                // resync (tags/garbage between frames)
                let mut q = p + 1;
                while q + 4 <= data.len() && parse_header(data, q).is_none() {
                    q += 1;
                }
                if q + 4 > data.len() {
                    break;
                }
                p = q;
            }
        }
    }
    Ok(Mp3Info { version, layer, samplerate, channels, frames, samples, bitrate_kbps_first: br0, vbr })
}

// ─── step 1: side-info parsing + bit-reservoir accounting (MPEG-1) ────────────

struct Br<'a> {
    d: &'a [u8],
    pos: usize,
}
impl<'a> Br<'a> {
    fn new(d: &'a [u8]) -> Self {
        Br { d, pos: 0 }
    }
    fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let (byte, off) = (self.pos / 8, 7 - (self.pos % 8));
            let bit = if byte < self.d.len() { (self.d[byte] >> off) & 1 } else { 0 };
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        v
    }
}

#[derive(Default, Clone)]
pub struct Granule {
    pub part2_3_length: u32,
    pub big_values: u32,
    pub global_gain: u32,
    pub scalefac_compress: u32,
    pub window_switching: bool,
    pub block_type: u8,
    pub mixed_block: bool,
    pub table_select: [u32; 3],
    pub subblock_gain: [u32; 3],
    pub region0_count: u32,
    pub region1_count: u32,
    pub preflag: bool,
    pub scalefac_scale: bool,
    pub count1table_select: bool,
}

pub struct SideInfo {
    pub main_data_begin: u32,
    pub scfsi: Vec<u32>,             // [channel] 4-bit scale-factor selection info
    pub granules: Vec<Vec<Granule>>, // [granule][channel]
}

/// Parse the MPEG-1 side-info block (`b` starts at the side info; `nch` channels).
/// Returns the parsed side info (2 granules × nch channels). -/
pub fn parse_side_info(b: &[u8], nch: usize) -> SideInfo {
    let mut r = Br::new(b);
    let main_data_begin = r.bits(9);
    r.bits(if nch == 1 { 5 } else { 3 }); // private bits
    let scfsi: Vec<u32> = (0..nch).map(|_| r.bits(4)).collect();
    let mut granules = vec![vec![Granule::default(); nch]; 2];
    for gr in 0..2 {
        for ch in 0..nch {
            let g = &mut granules[gr][ch];
            g.part2_3_length = r.bits(12);
            g.big_values = r.bits(9);
            g.global_gain = r.bits(8);
            g.scalefac_compress = r.bits(4);
            g.window_switching = r.bits(1) == 1;
            if g.window_switching {
                g.block_type = r.bits(2) as u8;
                g.mixed_block = r.bits(1) == 1;
                for i in 0..2 {
                    g.table_select[i] = r.bits(5);
                }
                for i in 0..3 {
                    g.subblock_gain[i] = r.bits(3);
                }
                // region counts implied for short blocks
                g.region0_count = if g.block_type == 2 && !g.mixed_block { 8 } else { 7 };
                g.region1_count = 36;
            } else {
                for i in 0..3 {
                    g.table_select[i] = r.bits(5);
                }
                g.region0_count = r.bits(4);
                g.region1_count = r.bits(3);
            }
            g.preflag = r.bits(1) == 1;
            g.scalefac_scale = r.bits(1) == 1;
            g.count1table_select = r.bits(1) == 1;
        }
    }
    SideInfo { main_data_begin, scfsi, granules }
}

/// Walk all frames, parse each side-info block, and report aggregate stats +
/// validity (big_values ≤ 288, part2_3_length bit-accounting). Verifies the
/// parser structurally before the Huffman stage. -/
pub fn sideinfo_stats(data: &[u8]) -> Result<String, String> {
    let info = parse(data)?;
    let nch = info.channels as usize;
    let si_bytes = if nch == 1 { 17 } else { 32 };
    // re-walk frames
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" {
        p = 10 + syncsafe(&data[6..10]);
    }
    let mut nframes = 0u64;
    let mut bad = 0u64;
    let mut max_bv = 0u32;
    let mut total_p23 = 0u64;
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != info.channels {
            p += 1;
            continue;
        }
        // protection bit (CRC present if 0) → 2 CRC bytes after the 4-byte header
        let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes <= data.len() {
            let si = parse_side_info(&data[si_start..si_start + si_bytes], nch);
            for gr in &si.granules {
                for g in gr {
                    if g.big_values > 288 {
                        bad += 1;
                    }
                    max_bv = max_bv.max(g.big_values);
                    total_p23 += g.part2_3_length as u64;
                }
            }
        }
        nframes += 1;
        if p + len + 4 > data.len() {
            break;
        }
        p += len;
    }
    Ok(format!(
        "side-info: {nframes} frames parsed, max big_values={max_bv} (≤288 valid), {bad} invalid, total part2_3 bits={total_p23}",
    ))
}

// ─── step 2: bit-reservoir assembly + per-frame main-data accounting ──────────

/// Assemble the bit reservoir across frames and verify the accounting: each
/// frame's granule data begins `main_data_begin` bytes before its own
/// main_data, and Σ part2_3_length bits must be available in the buffer. -/
pub fn reservoir_check(data: &[u8]) -> Result<String, String> {
    let info = parse(data)?;
    let nch = info.channels as usize;
    let si_bytes = if nch == 1 { 17 } else { 32 };
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" {
        p = 10 + syncsafe(&data[6..10]);
    }
    let mut reservoir: Vec<u8> = Vec::new();
    let (mut frames, mut underflow, mut overflow) = (0u64, 0u64, 0u64);
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != info.channels {
            p += 1;
            continue;
        }
        let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes > data.len() {
            break;
        }
        let si = parse_side_info(&data[si_start..si_start + si_bytes], nch);
        let md_start = si_start + si_bytes;
        let md_end = (p + len).min(data.len());
        // this frame's granule data starts main_data_begin bytes back in the buffer
        let frame_main_start = reservoir.len() as isize - si.main_data_begin as isize;
        if frame_main_start < 0 {
            underflow += 1;
        }
        if md_start < md_end {
            reservoir.extend_from_slice(&data[md_start..md_end]);
        }
        let total_bits: u64 = si.granules.iter().flatten().map(|g| g.part2_3_length as u64).sum();
        if frame_main_start >= 0 && (frame_main_start as u64) * 8 + total_bits > (reservoir.len() as u64) * 8 {
            overflow += 1;
        }
        frames += 1;
        if p + len + 4 > data.len() {
            break;
        }
        p += len;
    }
    Ok(format!(
        "reservoir: {frames} frames assembled, {underflow} underflow, {overflow} overflow ({} B main-data)",
        reservoir.len()
    ))
}

// ─── step 3a: Huffman code reader + round-trip validation ─────────────────────

/// Read one Huffman codeword from `br` against (codes, lens), returning the
/// entry index. Prefix-free, so accumulate bits until a (length, code) matches. -/
fn read_huffman(br: &mut Br, codes: &[u32], lens: &[u8]) -> Option<usize> {
    if codes.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    let mut l = 0u8;
    loop {
        acc = (acc << 1) | br.bits(1);
        l += 1;
        for i in 0..codes.len() {
            if lens[i] == l && codes[i] == acc {
                return Some(i);
            }
        }
        if l > 24 {
            return None;
        }
    }
}

fn pack_code(code: u32, len: u8) -> Vec<u8> {
    let mut v = vec![0u8; (len as usize + 7) / 8 + 1];
    for b in 0..len {
        if (code >> (len - 1 - b)) & 1 == 1 {
            v[b as usize / 8] |= 1 << (7 - (b % 8));
        }
    }
    v
}

/// Validate the Huffman reader + extracted ISO tables: every entry's code must
/// decode back to its own index (round-trip), across all tables + count1 quads. -/
pub fn huffman_selftest() -> i32 {
    let mut fails = 0u32;
    let mut total = 0u32;
    let mut check = |codes: &[u32], lens: &[u8]| {
        for i in 0..codes.len() {
            if lens[i] == 0 {
                continue;
            }
            total += 1;
            let buf = pack_code(codes[i], lens[i]);
            let mut br = Br::new(&buf);
            if read_huffman(&mut br, codes, lens) != Some(i) {
                fails += 1;
            }
        }
    };
    for ts in [1usize, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 24] {
        let (c, l, _) = crate::mp3_tables::table(ts);
        check(c, l);
    }
    for q in 0u8..2 {
        let (c, l) = crate::mp3_tables::quad_table(q);
        check(c, l);
    }
    println!("huffman round-trip: {} entries, {} failures [{}]", total, fails, if fails == 0 { "ok" } else { "FAIL" });
    if fails == 0 { 0 } else { 1 }
}

// ─── step 3b: scalefactor bit accounting + spectral (part3) decode ────────────

// Long-block scalefactor-band boundaries (sample indices), MPEG-1, per samplerate.
const SFB_44100: [usize; 23] = [0,4,8,12,16,20,24,30,36,44,52,62,74,90,110,134,162,196,238,288,342,418,576];
const SFB_48000: [usize; 23] = [0,4,8,12,16,20,24,30,36,42,50,60,72,88,106,128,156,190,230,276,330,384,576];
const SFB_32000: [usize; 23] = [0,4,8,12,16,20,24,30,36,44,54,66,82,102,126,156,194,240,296,364,448,550,576];

fn sfb_long(samplerate: u32) -> &'static [usize; 23] {
    match samplerate {
        48000 | 24000 => &SFB_48000,
        32000 | 16000 => &SFB_32000,
        _ => &SFB_44100,
    }
}

// scalefac_compress → (slen1, slen2)
const SLEN: [(u32, u32); 16] = [
    (0,0),(0,1),(0,2),(0,3),(3,0),(1,1),(1,2),(1,3),(2,1),(2,2),(2,3),(3,1),(3,2),(3,3),(4,2),(4,3),
];

/// Exact number of part2 (scalefactor) bits for a granule. -/
fn part2_bits(g: &Granule, gr: usize, scfsi: u32) -> u32 {
    let (s1, s2) = SLEN[g.scalefac_compress as usize & 15];
    if g.block_type == 2 {
        if g.mixed_block { 17 * s1 + 18 * s2 } else { 18 * s1 + 18 * s2 }
    } else if gr == 0 {
        11 * s1 + 10 * s2
    } else {
        // scfsi groups (gr1): bit3=sfb0-5, bit2=sfb6-10, bit1=sfb11-15, bit0=sfb16-20
        let mut b = 0;
        if scfsi & 0b1000 == 0 { b += 6 * s1; }
        if scfsi & 0b0100 == 0 { b += 5 * s1; }
        if scfsi & 0b0010 == 0 { b += 5 * s2; }
        if scfsi & 0b0001 == 0 { b += 5 * s2; }
        b
    }
}

/// Decode the part3 spectral data (big_values 3 regions + count1), advancing
/// `br`. Returns false on Huffman desync / invalid table. -/
fn decode_part3(br: &mut Br, g: &Granule, sfb: &[usize; 23], boundary: usize) -> bool {
    let bvl = (2 * g.big_values as usize).min(576);
    let (r1, r2) = if g.block_type == 2 {
        // short/mixed (MPEG-1): region1_start = 36, region2_start = 576
        (36.min(bvl), bvl)
    } else {
        let i1 = (g.region0_count as usize + 1).min(22);
        let i2 = (g.region0_count as usize + g.region1_count as usize + 2).min(22);
        (sfb[i1].min(bvl), sfb[i2].min(bvl))
    };
    for (s, e, ts) in [(0usize, r1, g.table_select[0]), (r1, r2, g.table_select[1]), (r2, bvl, g.table_select[2])] {
        if s >= e {
            continue;
        }
        let (codes, lens, wrap) = crate::mp3_tables::table(ts as usize);
        let lin = crate::mp3_tables::LINBITS[ts as usize & 31];
        if codes.is_empty() || wrap == 0 {
            continue; // empty Huffman table ⇒ region is zero, consumes no bits
        }
        let mut i = s;
        while i < e {
            let idx = match read_huffman(br, codes, lens) { Some(v) => v as u32, None => return false };
            let mut x = idx / wrap as u32;
            let mut y = idx % wrap as u32;
            if x == 15 && lin > 0 { x += br.bits(lin); }
            if x > 0 { br.bits(1); }
            if y == 15 && lin > 0 { y += br.bits(lin); }
            if y > 0 { br.bits(1); }
            i += 2;
        }
    }
    // count1: quads until the bit boundary or 576 samples
    let (qc, ql) = crate::mp3_tables::quad_table(g.count1table_select as u8);
    let mut idx = bvl;
    while br.pos < boundary && idx < 576 {
        let qi = match read_huffman(br, qc, ql) { Some(v) => v as u32, None => return false };
        for b in 0..4 {
            if (qi >> (3 - b)) & 1 == 1 { br.bits(1); }
        }
        idx += 4;
    }
    true
}

/// Validate the full spectral decode: for every granule, skip part2 (computed
/// bits) and decode part3; the bit position must land on the granule's
/// `part2_3_length` boundary. A wrong table/region desyncs and misses it. -/
pub fn spectral_check(data: &[u8]) -> Result<String, String> {
    let info = parse(data)?;
    let nch = info.channels as usize;
    let si_bytes = if nch == 1 { 17 } else { 32 };
    let sfb = sfb_long(info.samplerate);
    // pass 1: build reservoir + record per-frame (main_start, side_info)
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" { p = 10 + syncsafe(&data[6..10]); }
    let mut reservoir: Vec<u8> = Vec::new();
    let mut frames: Vec<(usize, SideInfo)> = Vec::new();
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != info.channels { p += 1; continue; }
        let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes > data.len() { break; }
        let si = parse_side_info(&data[si_start..si_start + si_bytes], nch);
        let start = reservoir.len() as isize - si.main_data_begin as isize;
        let md_end = (p + len).min(data.len());
        if si_start + si_bytes < md_end {
            reservoir.extend_from_slice(&data[si_start + si_bytes..md_end]);
        }
        if start >= 0 { frames.push((start as usize, si)); }
        if p + len + 4 > data.len() { break; }
        p += len;
    }
    // pass 2: decode each granule's part3, check bit landing
    let (mut total, mut pass, mut maxdev) = (0u64, 0u64, 0i64);
    let mut fail_bt = [0u64; 4]; // failures by block_type
    let mut example = String::new();
    for (mstart, si) in &frames {
        let mut gpos = mstart * 8;
        for gr in 0..2 {
            for ch in 0..nch {
                let g = &si.granules[gr][ch];
                if g.part2_3_length == 0 { gpos += 0; continue; }
                let start = gpos;
                let boundary = start + g.part2_3_length as usize;
                if boundary > reservoir.len() * 8 { continue; }
                let p2 = part2_bits(g, gr, si.scfsi[ch]) as usize;
                let mut br = Br::new(&reservoir);
                br.pos = start + p2;
                let ok = decode_part3(&mut br, g, sfb, boundary);
                let dev = br.pos as i64 - boundary as i64;
                total += 1;
                if ok && dev.abs() <= 16 {
                    pass += 1;
                } else {
                    fail_bt[g.block_type as usize & 3] += 1;
                    if example.is_empty() {
                        example = format!(
                            "(ex: gr{gr} bt={} mixed={} ws={} bv={} p23={} ts={:?} r0={} r1={} sc={} dev={dev} ok={ok})",
                            g.block_type, g.mixed_block, g.window_switching, g.big_values,
                            g.part2_3_length, g.table_select, g.region0_count, g.region1_count, g.scalefac_compress
                        );
                    }
                }
                if dev.abs() > maxdev.abs() { maxdev = dev; }
                gpos = boundary;
            }
        }
    }
    Ok(format!(
        "spectral: {pass}/{total} land on boundary (±16), maxdev {maxdev}; fails by block_type {fail_bt:?} {example}"
    ))
}

// ─── step 4: scalefactor values + spectral values + requantization ────────────

/// Read the part2 scalefactors (values) for a long-block granule 0. Returns the
/// 21 long scalefactors and the number of bits consumed. (gr1/scfsi sharing and
/// short-block layout handled in the full granule decoder; this covers the
/// common long-block case for the requant sanity check.) -/
fn read_scalefac_long(br: &mut Br, g: &Granule) -> [u32; 21] {
    let (s1, s2) = SLEN[g.scalefac_compress as usize & 15];
    let mut sf = [0u32; 21];
    for i in 0..11 {
        sf[i] = if s1 > 0 { br.bits(s1) } else { 0 };
    }
    for i in 11..21 {
        sf[i] = if s2 > 0 { br.bits(s2) } else { 0 };
    }
    sf
}

/// Decode the signed spectral values is[0..576] (big_values 3 regions + count1),
/// advancing `br`. Long/short region rules per the validated spectral decoder. -/
fn decode_is(br: &mut Br, g: &Granule, sfb: &[usize; 23], boundary: usize) -> [i32; 576] {
    let mut is = [0i32; 576];
    let bvl = (2 * g.big_values as usize).min(576);
    let (r1, r2) = if g.block_type == 2 {
        (36.min(bvl), bvl)
    } else {
        let i1 = (g.region0_count as usize + 1).min(22);
        let i2 = (g.region0_count as usize + g.region1_count as usize + 2).min(22);
        (sfb[i1].min(bvl), sfb[i2].min(bvl))
    };
    let mut idx: usize;
    for (s, e, ts) in [(0usize, r1, g.table_select[0]), (r1, r2, g.table_select[1]), (r2, bvl, g.table_select[2])] {
        if s >= e { continue; }
        let (codes, lens, wrap) = crate::mp3_tables::table(ts as usize);
        let lin = crate::mp3_tables::LINBITS[ts as usize & 31];
        if codes.is_empty() || wrap == 0 { continue; }
        idx = s;
        while idx < e {
            let hi = match read_huffman(br, codes, lens) { Some(v) => v as u32, None => return is };
            let mut x = (hi / wrap as u32) as i32;
            let mut y = (hi % wrap as u32) as i32;
            if x == 15 && lin > 0 { x += br.bits(lin) as i32; }
            if x != 0 && br.bits(1) == 1 { x = -x; }
            if y == 15 && lin > 0 { y += br.bits(lin) as i32; }
            if y != 0 && br.bits(1) == 1 { y = -y; }
            is[idx] = x;
            if idx + 1 < 576 { is[idx + 1] = y; }
            idx += 2;
        }
    }
    // count1
    let (qc, ql) = crate::mp3_tables::quad_table(g.count1table_select as u8);
    idx = bvl;
    while br.pos < boundary && idx + 4 <= 576 {
        let qi = match read_huffman(br, qc, ql) { Some(v) => v as u32, None => return is };
        for b in 0..4 {
            if (qi >> (3 - b)) & 1 == 1 {
                let v = if br.bits(1) == 1 { -1 } else { 1 };
                is[idx + b] = v;
            }
        }
        idx += 4;
    }
    is
}

/// Requantize a long-block granule: is[i] → xr[i] via the MP3 power law.
///   xr = sign(is)·|is|^(4/3) · 2^(0.25·(global_gain−210)) · 2^(−mult·scalefac[sfb])
/// with mult = scalefac_scale ? 1 : 0.5, plus the preflag pretab. -/
fn requantize_long(is: &[i32; 576], sf: &[u32; 21], g: &Granule, sfb: &[usize; 23]) -> [f64; 576] {
    const PRETAB: [u32; 22] = [0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,3,3,3,2,0];
    let mult = if g.scalefac_scale { 1.0 } else { 0.5 };
    let gain = 0.25 * (g.global_gain as f64 - 210.0);
    let mut xr = [0.0f64; 576];
    for band in 0..21 {
        let pre = if g.preflag { PRETAB[band] } else { 0 };
        let exp = gain - mult * (sf[band] + pre) as f64;
        let scale = exp.exp2();
        for i in sfb[band]..sfb[band + 1] {
            let v = is[i];
            if v != 0 {
                xr[i] = v.signum() as f64 * (v.unsigned_abs() as f64).powf(4.0 / 3.0) * scale;
            }
        }
    }
    xr
}

/// Sanity check: decode + requantize the first long-block granule and report
/// that the spectrum is finite with plausible energy. -/
pub fn requant_check(data: &[u8]) -> Result<String, String> {
    let info = parse(data)?;
    let nch = info.channels as usize;
    let si_bytes = if nch == 1 { 17 } else { 32 };
    let sfb = sfb_long(info.samplerate);
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" { p = 10 + syncsafe(&data[6..10]); }
    let mut reservoir: Vec<u8> = Vec::new();
    let mut first: Option<(usize, SideInfo)> = None;
    let mut nlong = 0u64;
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != info.channels { p += 1; continue; }
        let h = u32::from_be_bytes([data[p], data[p+1], data[p+2], data[p+3]]);
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes > data.len() { break; }
        let si = parse_side_info(&data[si_start..si_start + si_bytes], nch);
        let start = reservoir.len() as isize - si.main_data_begin as isize;
        let md_end = (p + len).min(data.len());
        if si_start + si_bytes < md_end { reservoir.extend_from_slice(&data[si_start + si_bytes..md_end]); }
        if first.is_none() && start >= 0 && si.granules[0][0].block_type != 2 && si.granules[0][0].big_values > 30 {
            first = Some((start as usize, si.clone_min()));
        }
        if si.granules[0][0].block_type != 2 { nlong += 1; }
        if p + len + 4 > data.len() { break; }
        p += len;
    }
    let (mstart, si) = first.ok_or("no long-block granule found")?;
    let g = &si.granules[0][0];
    let mut br = Br::new(&reservoir);
    br.pos = mstart * 8;
    let sf = read_scalefac_long(&mut br, g);
    let boundary = mstart * 8 + g.part2_3_length as usize;
    let is = decode_is(&mut br, g, sfb, boundary);
    let xr = requantize_long(&is, &sf, g, sfb);
    let finite = xr.iter().all(|v| v.is_finite());
    let nonzero = is.iter().filter(|&&v| v != 0).count();
    let energy: f64 = xr.iter().map(|v| v * v).sum();
    let peak = xr.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
    Ok(format!(
        "requant: {nlong} long-block frames; first granule: gg={} nonzero_lines={nonzero} finite={finite} energy={energy:.3e} peak={peak:.3e}",
        g.global_gain
    ))
}

impl SideInfo {
    fn clone_min(&self) -> SideInfo {
        SideInfo { main_data_begin: self.main_data_begin, scfsi: self.scfsi.clone(), granules: self.granules.clone() }
    }
}

// ─── step 5: alias reduction + IMDCT + synthesis filterbank → PCM (mono, long) ─

use std::f64::consts::PI;

/// Per-channel time-domain state carried across granules.
struct ChanState {
    overlap: [[f32; 18]; 32], // IMDCT overlap (prev granule's 2nd half)
    v: [f32; 1024],           // synthesis filterbank FIFO
    prev_sf: [u32; 21],
}
impl ChanState {
    fn new() -> Self {
        ChanState { overlap: [[0.0; 18]; 32], v: [0.0; 1024], prev_sf: [0; 21] }
    }
}

/// Precomputed cosine matrices (computed once, not tabled).
struct Dsp {
    imdct: [[f32; 18]; 36],  // imdct[i][k] = cos(PI/72 * (2i+1+18)(2k+1))
    imdct6: [[f32; 6]; 12],  // 6-point IMDCT for short blocks
    win_long: [f32; 36],     // sin window (block_type 0)
    win_short: [f32; 12],    // sin window for short blocks
    nmat: Vec<f32>,          // 64x32 synthesis matrixing cos
}
impl Dsp {
    fn new() -> Self {
        let mut imdct = [[0.0f32; 18]; 36];
        for i in 0..36 {
            for k in 0..18 {
                imdct[i][k] = ((PI / 72.0) * ((2 * i + 1 + 18) as f64) * ((2 * k + 1) as f64)).cos() as f32;
            }
        }
        let mut imdct6 = [[0.0f32; 6]; 12];
        for i in 0..12 {
            for k in 0..6 {
                imdct6[i][k] = ((PI / 24.0) * ((2 * i + 1 + 6) as f64) * ((2 * k + 1) as f64)).cos() as f32;
            }
        }
        let mut win_long = [0.0f32; 36];
        for i in 0..36 {
            win_long[i] = ((PI / 36.0) * (i as f64 + 0.5)).sin() as f32;
        }
        let mut win_short = [0.0f32; 12];
        for i in 0..12 {
            win_short[i] = ((PI / 12.0) * (i as f64 + 0.5)).sin() as f32;
        }
        let mut nmat = vec![0.0f32; 64 * 32];
        for i in 0..64 {
            for k in 0..32 {
                nmat[i * 32 + k] = ((PI / 64.0) * ((2 * k + 1) as f64) * ((i + 16) as f64)).cos() as f32;
            }
        }
        Dsp { imdct, imdct6, win_long, win_short, nmat }
    }
}

fn alias_reduce(xr: &mut [f32; 576]) {
    const CS: [f32; 8] = [0.857493, 0.881742, 0.949629, 0.983315, 0.995518, 0.999161, 0.999899, 0.999993];
    const CA: [f32; 8] = [-0.514496, -0.471732, -0.313377, -0.181913, -0.094574, -0.040966, -0.014199, -0.003700];
    for sb in 1..32 {
        let base = sb * 18;
        for i in 0..8 {
            let lo = base - 1 - i;
            let hi = base + i;
            let a = xr[lo];
            let b = xr[hi];
            xr[lo] = a * CS[i] - b * CA[i];
            xr[hi] = b * CS[i] + a * CA[i];
        }
    }
}

/// One synthesis filterbank step: 32 subband samples → 32 PCM, updating V FIFO. -/
fn synth_step(st: &mut ChanState, dsp: &Dsp, s: &[f32; 32], out: &mut [f32; 32]) {
    for i in (64..1024).rev() {
        st.v[i] = st.v[i - 64];
    }
    for i in 0..64 {
        let mut acc = 0.0f32;
        let row = &dsp.nmat[i * 32..i * 32 + 32];
        for k in 0..32 {
            acc += s[k] * row[k];
        }
        st.v[i] = acc;
    }
    let mut u = [0.0f32; 512];
    for i in 0..8 {
        for j in 0..32 {
            u[64 * i + j] = st.v[128 * i + j];
            u[64 * i + 32 + j] = st.v[128 * i + 96 + j];
        }
    }
    for j in 0..32 {
        let mut acc = 0.0f32;
        for i in 0..16 {
            acc += u[32 * i + j] * crate::mp3_tables::SYNTHESIS_D[32 * i + j];
        }
        out[j] = acc;
    }
}

/// Decode one long-block granule's requantized spectrum → 576 PCM samples. -/
fn synth_granule(st: &mut ChanState, dsp: &Dsp, mut xr: [f32; 576], short: bool) -> [f32; 576] {
    if !short {
        alias_reduce(&mut xr); // alias reduction is long-block only
    }
    let mut sub = [[0.0f32; 18]; 32]; // [subband][time]
    for sb in 0..32 {
        let mut t = [0.0f32; 36];
        if short {
            // 3 windows × 6-point IMDCT, windowed, placed at offsets 6+6w
            for win in 0..3 {
                for i in 0..12 {
                    let mut acc = 0.0f32;
                    for k in 0..6 {
                        acc += xr[sb * 18 + win * 6 + k] * dsp.imdct6[i][k];
                    }
                    t[6 + 6 * win + i] += acc * dsp.win_short[i];
                }
            }
        } else {
            for i in 0..36 {
                let mut acc = 0.0f32;
                for k in 0..18 {
                    acc += xr[sb * 18 + k] * dsp.imdct[i][k];
                }
                t[i] = acc * dsp.win_long[i]; // IMDCT basis TDAC-verified; spec uses it unscaled
            }
        }
        let mut o = [0.0f32; 18];
        for i in 0..18 {
            o[i] = t[i] + st.overlap[sb][i];
            st.overlap[sb][i] = t[18 + i];
        }
        if sb & 1 == 1 {
            let mut i = 1;
            while i < 18 { o[i] = -o[i]; i += 2; }
        }
        sub[sb] = o;
    }
    let mut pcm = [0.0f32; 576];
    let mut tmp = [0.0f32; 32];
    for ts in 0..18 {
        let mut s = [0.0f32; 32];
        for sb in 0..32 { s[sb] = sub[sb][ts]; }
        synth_step(st, dsp, &s, &mut tmp);
        for j in 0..32 { pcm[ts * 32 + j] = tmp[j]; }
    }
    pcm
}

fn read_scalefac(br: &mut Br, g: &Granule, gr: usize, scfsi: u32, prev: &mut [u32; 21]) -> [u32; 21] {
    let (s1, s2) = SLEN[g.scalefac_compress as usize & 15];
    let mut sf = [0u32; 21];
    let groups = [(0usize, 6, s1, 0b1000u32), (6, 11, s1, 0b0100), (11, 16, s2, 0b0010), (16, 21, s2, 0b0001)];
    for (lo, hi, slen, bit) in groups {
        if gr == 1 && scfsi & bit != 0 {
            for i in lo..hi { sf[i] = prev[i]; }
        } else {
            for i in lo..hi { sf[i] = if slen > 0 { br.bits(slen) } else { 0 }; }
        }
    }
    *prev = sf;
    sf
}

/// Decode a mono MP3 → PCM (long blocks; short blocks emitted as silence for
/// now). Returns (samples i16, samplerate). First-cut, validated by SNR. -/
pub fn decode_pcm_mono(data: &[u8]) -> Result<(Vec<i16>, u32), String> {
    let info = parse(data)?;
    if info.channels != 1 { return Err("mono-only decode (first cut)".into()); }
    let si_bytes = 17;
    let sfb = sfb_long(info.samplerate);
    let sfb_s = sfb_short(info.samplerate);
    let dsp = Dsp::new();
    // build reservoir + per-frame side info
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" { p = 10 + syncsafe(&data[6..10]); }
    let mut res: Vec<u8> = Vec::new();
    let mut frames: Vec<(usize, SideInfo)> = Vec::new();
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != 1 { p += 1; continue; }
        let h = u32::from_be_bytes([data[p], data[p+1], data[p+2], data[p+3]]);
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes > data.len() { break; }
        let si = parse_side_info(&data[si_start..si_start + si_bytes], 1);
        let start = res.len() as isize - si.main_data_begin as isize;
        let md_end = (p + len).min(data.len());
        if si_start + si_bytes < md_end { res.extend_from_slice(&data[si_start + si_bytes..md_end]); }
        if start >= 0 { frames.push((start as usize, si)); }
        if p + len + 4 > data.len() { break; }
        p += len;
    }
    let mut st = ChanState::new();
    let mut pcm: Vec<i16> = Vec::new();
    for (mstart, si) in &frames {
        let mut br = Br::new(&res);
        br.pos = mstart * 8;
        for gr in 0..2 {
            let g = &si.granules[gr][0];
            let start = br.pos;
            let boundary = start + g.part2_3_length as usize;
            if boundary > res.len() * 8 { return Err("reservoir underrun".into()); }
            let short = g.block_type == 2;
            let xr = if short {
                let sf = read_scalefac_short(&mut br, g);
                let is = decode_is(&mut br, g, sfb, boundary);
                br.pos = boundary;
                let xr64 = requantize_short(&is, &sf, g, sfb_s);
                reorder_short(&xr64, sfb_s)
            } else {
                let sf = read_scalefac(&mut br, g, gr, si.scfsi[0], &mut st.prev_sf);
                let is = decode_is(&mut br, g, sfb, boundary);
                br.pos = boundary;
                let xr64 = requantize_long(&is, &sf, g, sfb);
                let mut xr = [0.0f32; 576];
                for i in 0..576 { xr[i] = xr64[i] as f32; }
                xr
            };
            let gp = synth_granule(&mut st, &dsp, xr, short);
            for &v in gp.iter() {
                pcm.push((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16);
            }
        }
    }
    Ok((pcm, info.samplerate))
}

/// Verify the IMDCT by its defining property — perfect reconstruction (TDAC):
/// analysis (windowed MDCT) → synthesis (my IMDCT + window + overlap-add) must
/// return the input. This is a provable identity, not an SNR comparison; it
/// localizes any convention/scale error to the transform itself. -/
pub fn imdct_tdac_test() -> i32 {
    let dsp = Dsp::new();
    let n = 18 * 12;
    let x: Vec<f32> = (0..n).map(|i| ((i as f64 * 0.31).sin() + 0.5 * (i as f64 * 0.07).cos()) as f32).collect();
    for &scale in &[1.0f64, 2.0 / 18.0, 1.0 / 9.0, 1.0 / 18.0, 2.0 / 36.0] {
        let mut recon = vec![0f32; n];
        let mut overlap = [0f32; 18];
        for f in 0..(n / 18 - 1) {
            let s = f * 18;
            let mut z = [0f32; 36];
            for i in 0..36 {
                z[i] = if s + i < n { x[s + i] } else { 0.0 } * dsp.win_long[i];
            }
            let mut coef = [0f32; 18];
            for k in 0..18 {
                let mut a = 0.0f32;
                for nn in 0..36 { a += z[nn] * dsp.imdct[nn][k]; }
                coef[k] = a;
            }
            let mut y = [0f32; 36];
            for nn in 0..36 {
                let mut a = 0.0f32;
                for k in 0..18 { a += coef[k] * dsp.imdct[nn][k]; }
                y[nn] = a * scale as f32 * dsp.win_long[nn];
            }
            for i in 0..18 {
                recon[s + i] = y[i] + overlap[i];
                overlap[i] = y[18 + i];
            }
        }
        let (mut num, mut err) = (0f64, 0f64);
        for i in 54..(n - 54) {
            num += (x[i] as f64).powi(2);
            err += ((x[i] - recon[i]) as f64).powi(2);
        }
        let snr = if err > 0.0 { 10.0 * (num / err).log10() } else { 999.0 };
        println!("  18-pt scale={scale:.5}  reconstruction SNR = {snr:.1} dB");
    }
    // 6-point IMDCT (short blocks): 12-sample frames, hop 6, scale 2/6
    {
        let m = 6 * 16;
        let xs: Vec<f32> = (0..m).map(|i| ((i as f64 * 0.5).sin()) as f32).collect();
        let mut recon = vec![0f32; m];
        let mut ov = [0f32; 6];
        let scale = 2.0f32 / 6.0;
        for f in 0..(m / 6 - 1) {
            let s = f * 6;
            let mut z = [0f32; 12];
            for i in 0..12 { z[i] = if s + i < m { xs[s + i] } else { 0.0 } * dsp.win_short[i]; }
            let mut coef = [0f32; 6];
            for k in 0..6 { let mut a = 0.0f32; for nn in 0..12 { a += z[nn] * dsp.imdct6[nn][k]; } coef[k] = a; }
            let mut y = [0f32; 12];
            for nn in 0..12 { let mut a = 0.0f32; for k in 0..6 { a += coef[k] * dsp.imdct6[nn][k]; } y[nn] = a * scale * dsp.win_short[nn]; }
            for i in 0..6 { recon[s + i] = y[i] + ov[i]; ov[i] = y[6 + i]; }
        }
        let (mut num, mut err) = (0f64, 0f64);
        for i in 18..(m - 18) { num += (xs[i] as f64).powi(2); err += ((xs[i] - recon[i]) as f64).powi(2); }
        let snr = if err > 0.0 { 10.0 * (num / err).log10() } else { 999.0 };
        println!("  6-pt  scale={scale:.5}  reconstruction SNR = {snr:.1} dB");
    }
    0
}

/// Localize the filterbank by its defining property: a sustained unit in
/// subband `sb` must synthesize a tone at the subband centre `(sb+0.5)/64`
/// cycles/sample. Energy concentrated there ⇒ matrixing/window correct. -/
pub fn filterbank_test() -> i32 {
    let dsp = Dsp::new();
    let goertzel = |x: &[f32], f: f64| -> f64 {
        let w = 2.0 * PI * f;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1; s1 = s0;
        }
        ((s1 * s1 + s2 * s2 - c * s1 * s2).sqrt()) / x.len() as f64
    };
    for sb in [1usize, 5, 10, 20] {
        let mut st = ChanState::new();
        let mut out: Vec<f32> = Vec::new();
        let mut tmp = [0.0f32; 32];
        for step in 0..20 {
            let mut s = [0.0f32; 32];
            if step == 0 { s[sb] = 1.0; } // impulse in this subband
            synth_step(&mut st, &dsp, &s, &mut tmp);
            out.extend_from_slice(&tmp);
        }
        let tail = &out[..]; // the impulse response itself (modulated prototype)
        let fc = (sb as f64 + 0.5) / 64.0;
        let e_c = goertzel(tail, fc);
        let f_wrong = ((sb as f64 + 9.5) % 32.0 + 0.5) / 64.0;
        let e_w = goertzel(tail, f_wrong);
        println!("  sb={sb:>2}: centre f={fc:.4} energy={e_c:>8.3}   off-bin f={f_wrong:.4} energy={e_w:>8.3}   ratio={:.1}",
            e_c / e_w.max(1e-9));
    }
    0
}

// ─── short blocks: SFB-short tables, scalefactors, subblock_gain requant, reorder ─

// Short-block scalefactor-band boundaries (per-window freq index, 0..192), MPEG-1.
const SFBS_44100: [usize; 14] = [0, 4, 8, 12, 16, 22, 30, 40, 52, 66, 84, 106, 136, 192];
const SFBS_48000: [usize; 14] = [0, 4, 8, 12, 16, 22, 28, 38, 50, 64, 80, 100, 126, 192];
const SFBS_32000: [usize; 14] = [0, 4, 8, 12, 16, 22, 30, 42, 58, 78, 104, 138, 180, 192];

fn sfb_short(samplerate: u32) -> &'static [usize; 14] {
    match samplerate {
        48000 | 24000 => &SFBS_48000,
        32000 | 16000 => &SFBS_32000,
        _ => &SFBS_44100,
    }
}

/// Read short-block scalefactors scalefac_s[window][sfb], advancing `br`. -/
fn read_scalefac_short(br: &mut Br, g: &Granule) -> [[u32; 13]; 3] {
    let (s1, s2) = SLEN[g.scalefac_compress as usize & 15];
    let mut sf = [[0u32; 13]; 3];
    for sfb in 0..6 {
        for w in 0..3 {
            sf[w][sfb] = if s1 > 0 { br.bits(s1) } else { 0 };
        }
    }
    for sfb in 6..12 {
        for w in 0..3 {
            sf[w][sfb] = if s2 > 0 { br.bits(s2) } else { 0 };
        }
    }
    sf
}

/// Requantize short-block spectral values (stored sfb-window order). -/
fn requantize_short(is: &[i32; 576], sf: &[[u32; 13]; 3], g: &Granule, sfb_s: &[usize; 14]) -> [f64; 576] {
    let mult = if g.scalefac_scale { 1.0 } else { 0.5 };
    let gain = 0.25 * (g.global_gain as f64 - 210.0);
    let mut xr = [0.0f64; 576];
    let mut idx = 0usize;
    for sfb in 0..13 {
        let width = sfb_s[sfb + 1] - sfb_s[sfb];
        for win in 0..3 {
            let exp = gain - 2.0 * g.subblock_gain[win] as f64 - mult * sf[win][sfb.min(11)] as f64;
            let scale = exp.exp2();
            for _ in 0..width {
                if idx >= 576 { break; }
                let v = is[idx];
                if v != 0 {
                    xr[idx] = v.signum() as f64 * (v.unsigned_abs() as f64).powf(4.0 / 3.0) * scale;
                }
                idx += 1;
            }
        }
    }
    xr
}

/// Reorder short-block spectrum from stored (sfb,window,freq) order into
/// per-subband window-grouped order for the 3×6-point IMDCT. -/
fn reorder_short(stored: &[f64; 576], sfb_s: &[usize; 14]) -> [f32; 576] {
    let mut out = [0.0f32; 576];
    let mut idx = 0usize;
    for sfb in 0..13 {
        let width = sfb_s[sfb + 1] - sfb_s[sfb];
        for win in 0..3 {
            for k in 0..width {
                let p = sfb_s[sfb] + k; // freq position within a window (0..192)
                let sb = p / 6;
                let pos = p % 6;
                if sb < 32 {
                    out[sb * 18 + win * 6 + pos] = stored[idx] as f32;
                }
                idx += 1;
            }
        }
    }
    out
}

/// Full decode (mono or stereo), with MS joint-stereo decorrelation. -/
pub fn decode_pcm(data: &[u8]) -> Result<(Vec<i16>, u32, u16), String> {
    let info = parse(data)?;
    let nch = info.channels as usize;
    let si_bytes = if nch == 1 { 17 } else { 32 };
    let sfb = sfb_long(info.samplerate);
    let sfb_s = sfb_short(info.samplerate);
    let dsp = Dsp::new();
    let mut p = 0usize;
    if data.len() > 10 && &data[0..3] == b"ID3" { p = 10 + syncsafe(&data[6..10]); }
    let mut res: Vec<u8> = Vec::new();
    let mut frames: Vec<(usize, SideInfo, bool)> = Vec::new(); // (main_start, side_info, ms_on)
    while let Some((len, _, sr, ch, _)) = parse_header(data, p) {
        if sr != info.samplerate || ch != info.channels { p += 1; continue; }
        let h = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let mode = (h >> 6) & 0b11;
        let mode_ext = (h >> 4) & 0b11;
        let ms_on = mode == 0b01 && (mode_ext & 0b10) != 0;
        let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
        let si_start = p + 4 + crc;
        if si_start + si_bytes > data.len() { break; }
        let si = parse_side_info(&data[si_start..si_start + si_bytes], nch);
        let start = res.len() as isize - si.main_data_begin as isize;
        let md_end = (p + len).min(data.len());
        if si_start + si_bytes < md_end { res.extend_from_slice(&data[si_start + si_bytes..md_end]); }
        if start >= 0 { frames.push((start as usize, si, ms_on)); }
        if p + len + 4 > data.len() { break; }
        p += len;
    }
    let mut states: Vec<ChanState> = (0..nch).map(|_| ChanState::new()).collect();
    let mut pcm: Vec<i16> = Vec::new();
    let r2 = std::f32::consts::FRAC_1_SQRT_2;
    for (mstart, si, ms) in &frames {
        let mut br = Br::new(&res);
        br.pos = mstart * 8;
        for gr in 0..2 {
            let mut xrc = [[0.0f32; 576]; 2];
            let mut shortc = [false; 2];
            for ch in 0..nch {
                let g = &si.granules[gr][ch];
                let boundary = br.pos + g.part2_3_length as usize;
                if boundary > res.len() * 8 { return Err("reservoir underrun".into()); }
                shortc[ch] = g.block_type == 2;
                if shortc[ch] {
                    let sfs = read_scalefac_short(&mut br, g);
                    let is = decode_is(&mut br, g, sfb, boundary);
                    br.pos = boundary;
                    xrc[ch] = reorder_short(&requantize_short(&is, &sfs, g, sfb_s), sfb_s);
                } else {
                    let mut prev = states[ch].prev_sf;
                    let sf = read_scalefac(&mut br, g, gr, si.scfsi[ch], &mut prev);
                    states[ch].prev_sf = prev;
                    let is = decode_is(&mut br, g, sfb, boundary);
                    br.pos = boundary;
                    let xr64 = requantize_long(&is, &sf, g, sfb);
                    for i in 0..576 { xrc[ch][i] = xr64[i] as f32; }
                }
            }
            if nch == 2 && *ms {
                for i in 0..576 {
                    let (m, s) = (xrc[0][i], xrc[1][i]);
                    xrc[0][i] = (m + s) * r2;
                    xrc[1][i] = (m - s) * r2;
                }
            }
            let mut gp = [[0.0f32; 576]; 2];
            for ch in 0..nch {
                gp[ch] = synth_granule(&mut states[ch], &dsp, xrc[ch], shortc[ch]);
            }
            for i in 0..576 {
                for ch in 0..nch {
                    pcm.push((gp[ch][i] * 32768.0).round().clamp(-32768.0, 32767.0) as i16);
                }
            }
        }
    }
    Ok((pcm, info.samplerate, info.channels))
}
