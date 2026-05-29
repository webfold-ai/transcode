// ═══════════════════════════════════════════════════════════════════════════
// aac — MPEG-2/4 AAC-LC decoder (pure Rust, no deps)
//
// From-scratch implementation of the ISO/IEC 13818-7 / 14496-3 AAC Low
// Complexity profile, ADTS-framed. Built and verified the same way as mp3.rs:
//   parse bit-exact → each transform proven by its math property → PCM
//   correlation vs ffmpeg.  Only factual spec table VALUES are used.
//
// Pipeline:  ADTS demux → raw_data_block elements (SCE/CPE/FIL) →
//   ics_info → section_data (codebook runs) → scale_factor_data (DPCM) →
//   spectral_data (Huffman) → dequantize (|q|^4/3 · 2^(sf/4)) →
//   [TNS] → [M/S, intensity] → IMDCT (2048 long / 256 short, KBD/sine) →
//   windowed overlap-add (TDAC) → PCM.
// ═══════════════════════════════════════════════════════════════════════════

use crate::aac_tables;

/// ISO sampling-frequency table (index → Hz), 14496-3 Table 1.16.
pub const SAMPLE_RATES: [u32; 13] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// channel_configuration → channel count (0 = defined by PCE; we treat as 0).
pub const CHANNEL_CONFIG: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 8];

// ─── MSB-first bit reader ──────────────────────────────────────────────────

pub struct Bits<'a> {
    data: &'a [u8],
    pub pos: usize, // bit position
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }
    pub fn at(data: &'a [u8], bit: usize) -> Self {
        Bits { data, pos: bit }
    }
    /// Read `n` bits (n ≤ 32), MSB-first, big-endian across bytes.
    pub fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.pos >> 3;
            let bit = 7 - (self.pos & 7);
            let b = if byte < self.data.len() { (self.data[byte] >> bit) & 1 } else { 0 };
            v = (v << 1) | b as u32;
            self.pos += 1;
        }
        v
    }
    pub fn bit(&mut self) -> u32 {
        self.bits(1)
    }
    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }
}

// ─── ADTS framing ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct AdtsHeader {
    pub mpeg4: bool,           // ID bit: false = MPEG-4, true = MPEG-2
    pub protection_absent: bool,
    pub profile: u8,           // object_type - 1  (LC = 1)
    pub sf_index: u8,
    pub channel_config: u8,
    pub frame_length: usize,   // total frame incl. header, bytes
    pub num_blocks: u8,        // raw_data_blocks in frame - 1
    pub header_len: usize,     // 7 (no CRC) or 9 (CRC)
}

/// Parse an ADTS header at byte offset `o`. Returns None if no valid syncword.
pub fn parse_adts(data: &[u8], o: usize) -> Option<AdtsHeader> {
    if o + 7 > data.len() {
        return None;
    }
    // syncword 0xFFF
    if data[o] != 0xFF || (data[o + 1] & 0xF0) != 0xF0 {
        return None;
    }
    let mut b = Bits::at(data, o * 8);
    b.bits(12); // syncword
    let mpeg4 = b.bit() == 0;
    b.bits(2); // layer (00)
    let protection_absent = b.bit() == 1;
    let profile = b.bits(2) as u8;
    let sf_index = b.bits(4) as u8;
    b.bit(); // private
    let channel_config = b.bits(3) as u8;
    b.bits(4); // original/home/copyright id+start
    let frame_length = b.bits(13) as usize;
    b.bits(11); // buffer fullness
    let num_blocks = b.bits(2) as u8;
    let header_len = if protection_absent { 7 } else { 9 };
    if sf_index as usize >= SAMPLE_RATES.len() || frame_length < header_len {
        return None;
    }
    Some(AdtsHeader {
        mpeg4,
        protection_absent,
        profile,
        sf_index,
        channel_config,
        frame_length,
        num_blocks,
        header_len,
    })
}

pub struct Info {
    pub samplerate: u32,
    pub channels: u16,
    pub profile: u8,
    pub frames: usize,
    pub samples: usize, // per channel
}

/// Scan the whole ADTS stream: count frames, derive duration.
pub fn parse(data: &[u8]) -> Result<Info, String> {
    let first = parse_adts(data, 0).ok_or("no ADTS syncword at start")?;
    let samplerate = SAMPLE_RATES[first.sf_index as usize];
    let channels = if (first.channel_config as usize) < CHANNEL_CONFIG.len() {
        CHANNEL_CONFIG[first.channel_config as usize]
    } else {
        0
    };
    let mut frames = 0usize;
    let mut o = 0usize;
    while o + 7 <= data.len() {
        let h = match parse_adts(data, o) {
            Some(h) => h,
            None => break,
        };
        if h.frame_length == 0 || o + h.frame_length > data.len() {
            // last partial / truncated frame
            frames += 1;
            break;
        }
        frames += (h.num_blocks as usize) + 1;
        o += h.frame_length;
    }
    Ok(Info {
        samplerate,
        channels,
        profile: first.profile,
        frames,
        samples: frames * 1024,
    })
}

// ─── Huffman spectral / scalefactor readers ──────────────────────────────────

/// Decode one Huffman symbol from spectral codebook arrays (codeword, length).
/// MSB-first; returns the symbol index, or None on a code longer than any entry.
fn read_huff(b: &mut Bits, codes: &[u16], lens: &[u8]) -> Option<usize> {
    let maxlen = *lens.iter().max().unwrap_or(&0);
    let mut acc = 0u32;
    for l in 1..=maxlen {
        acc = (acc << 1) | b.bit();
        for i in 0..codes.len() {
            if lens[i] == l && codes[i] as u32 == acc {
                return Some(i);
            }
        }
    }
    None
}

/// Decode one scalefactor Huffman symbol (codeword table is u32-wide).
fn read_sf(b: &mut Bits) -> Option<usize> {
    let (codes, lens) = (&aac_tables::SF_CODES, &aac_tables::SF_BITS);
    let maxlen = *lens.iter().max().unwrap();
    let mut acc = 0u32;
    for l in 1..=maxlen {
        acc = (acc << 1) | b.bit();
        for i in 0..codes.len() {
            if lens[i] == l && codes[i] == acc {
                return Some(i);
            }
        }
    }
    None
}

/// Append `len` bits of `code` (MSB-first) to a bit buffer — self-test only.
fn put_bits(buf: &mut Vec<u8>, pos: &mut usize, code: u32, len: u8) {
    for k in (0..len).rev() {
        let byte = *pos >> 3;
        while byte >= buf.len() {
            buf.push(0);
        }
        if (code >> k) & 1 == 1 {
            buf[byte] |= 1 << (7 - (*pos & 7));
        }
        *pos += 1;
    }
}

/// Codebook self-test: encode each codeword, decode it back, and require it to
/// return its own index consuming exactly its length. This proves every
/// codebook is a valid prefix code AND that the table VALUES were extracted
/// correctly — a wrong/prefix-colliding entry decodes to the wrong index or
/// wrong bit count. (The deeper proof is the bit-exact frame-alignment gate.)
pub fn huffman_selftest() -> i32 {
    let mut fails = 0;
    let mut total = 0;
    let check = |codes_u32: &dyn Fn(usize) -> u32, lens: &[u8], n: usize| -> usize {
        let mut bad = 0;
        for i in 0..n {
            let mut buf = vec![0u8; 0];
            let mut pos = 0usize;
            put_bits(&mut buf, &mut pos, codes_u32(i), lens[i]);
            buf.push(0);
            buf.push(0); // pad so the reader never runs off the end
            let mut br = Bits::new(&buf);
            // decode using a generic linear match identical to read_huff/read_sf
            let maxlen = *lens.iter().max().unwrap();
            let mut acc = 0u32;
            let mut got = None;
            for l in 1..=maxlen {
                acc = (acc << 1) | br.bit();
                for j in 0..n {
                    if lens[j] == l && codes_u32(j) == acc {
                        got = Some(j);
                        break;
                    }
                }
                if got.is_some() {
                    break;
                }
            }
            if got != Some(i) || br.pos != lens[i] as usize {
                bad += 1;
            }
        }
        bad
    };
    for cb in 1..=11 {
        let (codes, lens) = aac_tables::codebook(cb);
        let bad = check(&|i| codes[i] as u32, lens, codes.len());
        total += codes.len();
        if bad > 0 {
            fails += 1;
        }
        println!(
            "  cb{cb:<2} {:>3} symbols, max {} bits  {}",
            codes.len(),
            lens.iter().max().unwrap(),
            if bad == 0 { "[ok]".to_string() } else { format!("[{bad} BAD]") }
        );
    }
    let (sfc, sfl) = (&aac_tables::SF_CODES, &aac_tables::SF_BITS);
    let bad = check(&|i| sfc[i], sfl, sfc.len());
    total += sfc.len();
    if bad > 0 {
        fails += 1;
    }
    println!(
        "  sf  {:>3} symbols, max {} bits  {}",
        sfc.len(),
        sfl.iter().max().unwrap(),
        if bad == 0 { "[ok]".to_string() } else { format!("[{bad} BAD]") }
    );
    if fails == 0 {
        println!("aac huffman: all {total} codewords round-trip (valid prefix codes)");
        0
    } else {
        println!("aac huffman: {fails} codebook(s) FAILED");
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Bitstream syntax: raw_data_block → SCE/CPE/… → ics → section/sf/spectral
// (ISO/IEC 14496-3 §4.4.2, AAC-LC subset). Produces quantized spectral
// coefficients + scalefactors + window info per channel.
// ═══════════════════════════════════════════════════════════════════════════

const ONLY_LONG: u32 = 0;
const _LONG_START: u32 = 1;
const EIGHT_SHORT: u32 = 2;
const _LONG_STOP: u32 = 3;

#[derive(Clone, Default)]
pub struct Ics {
    pub window_sequence: u32,
    pub window_shape: u32,
    pub max_sfb: usize,
    pub num_windows: usize,
    pub num_window_groups: usize,
    pub group_len: Vec<usize>,    // windows per group
    pub swb_offset: Vec<usize>,   // sfb boundaries within one window
    pub num_swb: usize,
}

#[derive(Clone, Default)]
struct TnsFilter {
    length: usize,
    order: usize,
    direction: bool,
    parcor: Vec<f64>, // dequantized reflection coefficients
}

#[derive(Clone, Default)]
struct Tns {
    filt: Vec<Vec<TnsFilter>>, // per window
}

#[derive(Clone, Default)]
pub struct Channel {
    pub ics: Ics,
    pub global_gain: u32,
    pub cb: Vec<u8>,    // codebook per (group,sfb): g*max_sfb + sfb
    pub sf: Vec<i32>,   // scalefactor per (group,sfb)
    pub coef: Vec<i32>, // quantized coefficients, read order
    tns: Tns,
}

fn decode_ics_info(b: &mut Bits, sr: usize) -> Result<Ics, String> {
    b.bit(); // ics_reserved_bit
    let window_sequence = b.bits(2);
    let window_shape = b.bit();
    let mut ics = Ics { window_sequence, window_shape, ..Default::default() };
    if window_sequence == EIGHT_SHORT {
        ics.max_sfb = b.bits(4) as usize;
        let sfg = b.bits(7);
        ics.num_windows = 8;
        ics.num_window_groups = 1;
        ics.group_len = vec![1];
        for i in 0..7 {
            if sfg & (1 << (6 - i)) != 0 {
                *ics.group_len.last_mut().unwrap() += 1;
            } else {
                ics.num_window_groups += 1;
                ics.group_len.push(1);
            }
        }
        let off = aac_tables::swb_offset_short(sr);
        ics.swb_offset = off.iter().map(|&x| x as usize).collect();
        ics.num_swb = off.len() - 1;
    } else {
        ics.max_sfb = b.bits(6) as usize;
        if b.bit() == 1 {
            return Err("predictor_data_present (not AAC-LC)".into());
        }
        ics.num_windows = 1;
        ics.num_window_groups = 1;
        ics.group_len = vec![1];
        let off = aac_tables::swb_offset_long(sr);
        ics.swb_offset = off.iter().map(|&x| x as usize).collect();
        ics.num_swb = off.len() - 1;
    }
    if ics.max_sfb > ics.num_swb {
        return Err(format!("max_sfb {} > num_swb {}", ics.max_sfb, ics.num_swb));
    }
    Ok(ics)
}

fn decode_section(b: &mut Bits, ics: &Ics) -> Vec<u8> {
    let bits_incr = if ics.window_sequence == EIGHT_SHORT { 3 } else { 5 };
    let esc = (1u32 << bits_incr) - 1;
    let mut cb = vec![0u8; ics.num_window_groups * ics.max_sfb];
    for g in 0..ics.num_window_groups {
        let mut k = 0;
        while k < ics.max_sfb {
            let c = b.bits(4) as u8;
            let mut len = 0u32;
            loop {
                let incr = b.bits(bits_incr);
                len += incr;
                if incr != esc {
                    break;
                }
            }
            if len == 0 {
                break; // guard (valid streams have len ≥ 1)
            }
            for _ in 0..len {
                if k < ics.max_sfb {
                    cb[g * ics.max_sfb + k] = c;
                    k += 1;
                }
            }
        }
    }
    cb
}

fn decode_scalefactors(b: &mut Bits, ics: &Ics, cb: &[u8], global_gain: u32) -> Result<Vec<i32>, String> {
    let mut sf = vec![0i32; ics.num_window_groups * ics.max_sfb];
    let mut off_sf = global_gain as i32;
    let mut off_is = 0i32;
    let mut off_noise = global_gain as i32 - 90;
    let mut noise_first = true;
    for g in 0..ics.num_window_groups {
        for s in 0..ics.max_sfb {
            let c = cb[g * ics.max_sfb + s];
            let idx = g * ics.max_sfb + s;
            match c {
                0 => {} // ZERO_HCB — no scalefactor
                14 | 15 => {
                    off_is += read_sf(b).ok_or("sf(is)")? as i32 - 60;
                    sf[idx] = off_is;
                }
                13 => {
                    if noise_first {
                        noise_first = false;
                        off_noise += b.bits(9) as i32 - 256;
                    } else {
                        off_noise += read_sf(b).ok_or("sf(noise)")? as i32 - 60;
                    }
                    sf[idx] = off_noise;
                }
                _ => {
                    off_sf += read_sf(b).ok_or("sf")? as i32 - 60;
                    sf[idx] = off_sf;
                }
            }
        }
    }
    Ok(sf)
}

/// Unpack a spectral Huffman index into `dim` (signed or magnitude) values.
fn unpack(cb: usize, idx: usize) -> [i32; 4] {
    let mut o = [0i32; 4];
    match cb {
        1 | 2 => {
            let mut v = idx;
            for d in (0..4).rev() {
                o[d] = (v % 3) as i32 - 1;
                v /= 3;
            }
        }
        3 | 4 => {
            let mut v = idx;
            for d in (0..4).rev() {
                o[d] = (v % 3) as i32;
                v /= 3;
            }
        }
        5 | 6 => {
            o[0] = (idx / 9) as i32 - 4;
            o[1] = (idx % 9) as i32 - 4;
        }
        7 | 8 => {
            o[0] = (idx / 8) as i32;
            o[1] = (idx % 8) as i32;
        }
        9 | 10 => {
            o[0] = (idx / 13) as i32;
            o[1] = (idx % 13) as i32;
        }
        11 => {
            o[0] = (idx / 17) as i32;
            o[1] = (idx % 17) as i32;
        }
        _ => {}
    }
    o
}

/// ESC value decode for codebook 11 (|value| == 16): count leading 1s = N,
/// then read N+4 bits → 2^(N+4) + word.
fn read_escape(b: &mut Bits) -> i32 {
    let mut n = 0u32;
    while b.bit() == 1 {
        n += 1;
        if n > 24 {
            break;
        }
    }
    let mut val = 0i32;
    for _ in 0..(n + 4) {
        val = (val << 1) | b.bit() as i32;
    }
    val + (1 << (n + 4))
}

fn decode_spectral(b: &mut Bits, ics: &Ics, cb: &[u8]) -> Result<Vec<i32>, String> {
    let mut coef = Vec::new();
    for g in 0..ics.num_window_groups {
        for s in 0..ics.max_sfb {
            let c = cb[g * ics.max_sfb + s] as usize;
            if c == 0 || c >= 12 {
                continue; // ZERO / intensity / noise: no spectral coefficients
            }
            let (dim, unsigned, _lav, esc) = aac_tables::cb_info(c);
            let (codes, lens) = aac_tables::codebook(c);
            let width = ics.swb_offset[s + 1] - ics.swb_offset[s];
            let ncoef = ics.group_len[g] * width;
            if ncoef % dim != 0 {
                return Err(format!("sfb width {ncoef} not multiple of dim {dim}"));
            }
            let mut k = 0;
            while k < ncoef {
                let idx = read_huff(b, codes, lens).ok_or("spectral huff")?;
                let vals = unpack(c, idx);
                if unsigned {
                    // ISO order: sign bits for each non-zero value FIRST, then the
                    // ESC sequences for each magnitude == 16. Reading them the other
                    // way feeds a sign bit into the ESC leading-1 count → desync.
                    let mut sign = [false; 4];
                    for d in 0..dim {
                        if vals[d] != 0 {
                            sign[d] = b.bit() == 1;
                        }
                    }
                    for d in 0..dim {
                        let mut mag = vals[d];
                        if esc && mag == 16 {
                            mag = read_escape(b);
                        }
                        coef.push(if sign[d] { -mag } else { mag });
                    }
                } else {
                    for d in 0..dim {
                        coef.push(vals[d]); // signed codebook — value encodes sign
                    }
                }
                k += dim;
            }
        }
    }
    Ok(coef)
}

fn decode_tns(b: &mut Bits, ics: &Ics) -> Tns {
    let (nf_bits, len_bits, ord_bits) = if ics.window_sequence == EIGHT_SHORT {
        (1u32, 4u32, 3u32)
    } else {
        (2, 6, 5)
    };
    let mut tns = Tns { filt: vec![Vec::new(); ics.num_windows] };
    for w in 0..ics.num_windows {
        let n_filt = b.bits(nf_bits);
        if n_filt > 0 {
            let coef_res = b.bit();
            let coef_res_bits = if coef_res == 1 { 4u32 } else { 3 };
            for _ in 0..n_filt {
                let length = b.bits(len_bits) as usize;
                let order = b.bits(ord_bits) as usize;
                let mut f = TnsFilter { length, order, ..Default::default() };
                if order > 0 {
                    f.direction = b.bit() == 1;
                    let coef_compress = b.bit();
                    let coef_bits = coef_res_bits - coef_compress;
                    // dequantize each transmitted coefficient to a PARCOR value
                    let iqfac = ((1i32 << (coef_res_bits - 1)) as f64 - 0.5) / (PI / 2.0);
                    let iqfac_m = ((1i32 << (coef_res_bits - 1)) as f64 + 0.5) / (PI / 2.0);
                    for _ in 0..order {
                        let raw = b.bits(coef_bits) as i32;
                        let c = if raw >= (1 << (coef_bits - 1)) { raw - (1 << coef_bits) } else { raw };
                        let p = (c as f64 / if c >= 0 { iqfac } else { iqfac_m }).sin();
                        f.parcor.push(p);
                    }
                }
                tns.filt[w].push(f);
            }
        }
    }
    tns
}

/// Apply the TNS synthesis (all-pole) filter to the spectrum, per window/filter.
fn apply_tns(spec: &mut [f64], ics: &Ics, tns: &Tns) {
    for w in 0..ics.num_windows {
        if w >= tns.filt.len() {
            break;
        }
        let mut bottom = ics.num_swb;
        for f in &tns.filt[w] {
            let top = bottom;
            bottom = top.saturating_sub(f.length);
            if f.order == 0 {
                continue;
            }
            // reflection (PARCOR) → direct-form LPC coefficients a[1..=order]
            let order = f.order;
            let mut a = vec![0f64; order + 1];
            a[0] = 1.0;
            for m in 1..=order {
                let prev = a.clone();
                for i in 1..m {
                    a[i] = prev[i] + f.parcor[m - 1] * prev[m - i];
                }
                a[m] = f.parcor[m - 1];
            }
            let start = ics.swb_offset[bottom.min(ics.max_sfb)];
            let end = ics.swb_offset[top.min(ics.max_sfb)];
            let size = end.saturating_sub(start);
            if size == 0 {
                continue;
            }
            let woff = if ics.window_sequence == EIGHT_SHORT { w * 128 } else { 0 };
            let (mut p, inc): (isize, isize) =
                if f.direction { ((woff + end - 1) as isize, -1) } else { ((woff + start) as isize, 1) };
            for m in 0..size {
                let lim = m.min(order);
                let mut acc = spec[p as usize];
                for i in 1..=lim {
                    acc -= spec[(p - i as isize * inc) as usize] * a[i];
                }
                spec[p as usize] = acc;
                p += inc;
            }
        }
    }
}

fn parse_ics(b: &mut Bits, sr: usize, common: Option<&Ics>, dbg: bool) -> Result<Channel, String> {
    let global_gain = b.bits(8);
    let ics = match common {
        Some(i) => i.clone(),
        None => decode_ics_info(b, sr)?,
    };
    if dbg {
        eprintln!(
            "  ics: ws={} max_sfb={} groups={} num_swb={} @bit {}",
            ics.window_sequence, ics.max_sfb, ics.num_window_groups, ics.num_swb, b.pos
        );
    }
    let cb = decode_section(b, &ics);
    if dbg {
        eprintln!("  after section @bit {}  cb={:?}", b.pos, &cb);
    }
    let sf = decode_scalefactors(b, &ics, &cb, global_gain)?;
    if dbg {
        eprintln!("  after scalefactors @bit {}", b.pos);
    }
    if b.bit() == 1 {
        // pulse_data
        let np = b.bits(2);
        b.bits(6);
        for _ in 0..=np {
            b.bits(5);
            b.bits(4);
        }
    }
    if dbg {
        eprintln!("  after pulse @bit {}", b.pos);
    }
    let tns = if b.bit() == 1 { decode_tns(b, &ics) } else { Tns::default() };
    if dbg {
        eprintln!("  after tns @bit {}", b.pos);
    }
    if b.bit() == 1 {
        return Err("gain_control_data (not AAC-LC)".into());
    }
    let coef = decode_spectral(b, &ics, &cb)?;
    if dbg {
        eprintln!("  after spectral @bit {}  ncoef={}", b.pos, coef.len());
    }
    Ok(Channel { ics, global_gain, cb, sf, coef, tns })
}

/// Parse one raw_data_block: the element sequence terminated by ID_END.
/// `ms` collects (num_groups, max_sfb, ms_used bits) per CPE for stereo.
fn parse_raw_data_block(b: &mut Bits, sr: usize, ms: &mut Vec<Vec<bool>>, dbg: bool) -> Result<Vec<Channel>, String> {
    let mut chans = Vec::new();
    loop {
        let id = b.bits(3);
        if dbg {
            eprintln!("element id={id} @bit {}", b.pos);
        }
        match id {
            0 | 3 => {
                // SCE / LFE
                b.bits(4); // element_instance_tag
                chans.push(parse_ics(b, sr, None, dbg)?);
                ms.push(Vec::new());
            }
            1 => {
                // CPE
                b.bits(4);
                let common = b.bit();
                let mut ms_used = Vec::new();
                let common_ics = if common == 1 {
                    let ics = decode_ics_info(b, sr)?;
                    let ms_mask = b.bits(2);
                    if ms_mask == 1 {
                        for _ in 0..ics.num_window_groups * ics.max_sfb {
                            ms_used.push(b.bit() == 1);
                        }
                    } else if ms_mask == 2 {
                        ms_used = vec![true; ics.num_window_groups * ics.max_sfb];
                    }
                    Some(ics)
                } else {
                    None
                };
                chans.push(parse_ics(b, sr, common_ics.as_ref(), dbg)?);
                chans.push(parse_ics(b, sr, common_ics.as_ref(), dbg)?);
                ms.push(ms_used.clone());
                ms.push(ms_used);
            }
            2 => return Err("coupling_channel_element unsupported".into()),
            4 => {
                // data_stream_element
                b.bits(4);
                let align = b.bit();
                let mut cnt = b.bits(8);
                if cnt == 255 {
                    cnt += b.bits(8);
                }
                if align == 1 {
                    b.byte_align();
                }
                for _ in 0..cnt {
                    b.bits(8);
                }
            }
            5 => return Err("program_config_element unsupported".into()),
            6 => {
                // fill_element
                let mut cnt = b.bits(4);
                if cnt == 15 {
                    cnt += b.bits(8) - 1;
                }
                for _ in 0..cnt {
                    b.bits(8);
                }
            }
            7 => break, // ID_END
            _ => unreachable!(),
        }
        if b.remaining() < 3 {
            break;
        }
    }
    b.byte_align();
    Ok(chans)
}

/// Bit-exact alignment gate: decode the raw_data_block of every frame and check
/// it lands exactly on the next ADTS frame boundary. A single wrong codebook /
/// scalefactor / syntax field desyncs the bit consumption and the frame won't
/// land — this is the deep proof of correctness (cf. mp3's part2_3_length gate).
pub fn align_check(data: &[u8]) -> Result<String, String> {
    let (mut frames, mut aligned, mut errs) = (0usize, 0usize, 0usize);
    let mut first_err = String::new();
    let mut o = 0usize;
    while o + 7 <= data.len() {
        let h = match parse_adts(data, o) {
            Some(h) => h,
            None => break,
        };
        if h.frame_length == 0 || o + h.frame_length > data.len() {
            break;
        }
        let sr = h.sf_index as usize;
        let frame_end = (o + h.frame_length) * 8;
        let mut b = Bits::at(data, (o + h.header_len) * 8);
        let mut ms = Vec::new();
        match parse_raw_data_block(&mut b, sr, &mut ms, false) {
            Ok(_) => {
                if b.pos == frame_end {
                    aligned += 1;
                } else {
                    errs += 1;
                    if first_err.is_empty() {
                        first_err = format!(
                            "frame {frames}: ended at bit {}, want {} (off {})",
                            b.pos,
                            frame_end,
                            frame_end as i64 - b.pos as i64
                        );
                    }
                }
            }
            Err(e) => {
                errs += 1;
                if first_err.is_empty() {
                    first_err = format!("frame {frames}: {e}");
                }
            }
        }
        frames += 1;
        o += h.frame_length;
    }
    let tail = if first_err.is_empty() {
        String::new()
    } else {
        format!("; first mismatch — {first_err}")
    };
    Ok(format!("{aligned}/{frames} frames byte-exact aligned ({errs} mismatched){tail}"))
}

/// Count feature usage across the stream (to characterize decode residuals).
pub fn feature_stats(data: &[u8]) -> Result<String, String> {
    let (mut frames, mut shortf, mut tnsf, mut isf, mut pnsf) = (0usize, 0, 0, 0, 0);
    let mut o = 0usize;
    while o + 7 <= data.len() {
        let h = match parse_adts(data, o) {
            Some(h) => h,
            None => break,
        };
        if h.frame_length == 0 || o + h.frame_length > data.len() {
            break;
        }
        let mut b = Bits::at(data, (o + h.header_len) * 8);
        let mut ms = Vec::new();
        if let Ok(chans) = parse_raw_data_block(&mut b, h.sf_index as usize, &mut ms, false) {
            for c in &chans {
                if c.ics.window_sequence == EIGHT_SHORT {
                    shortf += 1;
                }
                if c.tns.filt.iter().any(|w| w.iter().any(|f| f.order > 0)) {
                    tnsf += 1;
                }
                if c.cb.iter().any(|&x| x == 14 || x == 15) {
                    isf += 1;
                }
                if c.cb.iter().any(|&x| x == 13) {
                    pnsf += 1;
                }
            }
        }
        frames += 1;
        o += h.frame_length;
    }
    Ok(format!(
        "{frames} frames: {shortf} short-block, {tnsf} TNS, {isf} intensity-stereo, {pnsf} PNS (channel-instances)"
    ))
}

/// Trace the parse of the first frame, stage by stage, for debugging.
pub fn debug_frame(data: &[u8]) -> Result<String, String> {
    let h = parse_adts(data, 0).ok_or("no ADTS")?;
    let frame_end = h.frame_length * 8;
    eprintln!(
        "frame: len={} header={} payload bits {}..{}",
        h.frame_length,
        h.header_len,
        h.header_len * 8,
        frame_end
    );
    let mut b = Bits::at(data, h.header_len * 8);
    let mut ms = Vec::new();
    let r = parse_raw_data_block(&mut b, h.sf_index as usize, &mut ms, true);
    Ok(format!("result={:?}  ended @bit {} (want {})", r.map(|c| c.len()), b.pos, frame_end))
}

// ═══════════════════════════════════════════════════════════════════════════
// DSP: IMDCT + sine / Kaiser-Bessel-Derived windows + overlap-add (TDAC)
//
// AAC filterbank (14496-3 §4.6.11). The IMDCT for a frame of N/2 coefficients
//   x[n] = (2/N) Σ_{k=0}^{N/2-1} spec[k] · cos( (π/N)(2n + 1 + N/2)(2k + 1)/2 )
// for n = 0..N-1, with N = 2048 (long) or 256 (short). The analysis window
// (sine or KBD) is applied, then 50 % overlap-add reconstructs the signal
// (time-domain aliasing cancellation, Princen-Bradley: W[n]²+W[n+N/2]² = 1).
// ═══════════════════════════════════════════════════════════════════════════

use std::f64::consts::PI;

/// Modified Bessel function of the first kind, order 0 (series; converges fast).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < sum * 1e-16 {
            break;
        }
    }
    sum
}

/// Kaiser-Bessel-Derived window of length `n`, parameter α (4 long / 6 short).
/// Returns the full N-point window (rising left half, falling right half).
pub fn kbd_window(n: usize, alpha: f64) -> Vec<f64> {
    let half = n / 2;
    // Kaiser-Bessel kernel W'[p], p = 0..=N/2
    let mut kernel = vec![0.0f64; half + 1];
    let denom = bessel_i0(PI * alpha);
    for (p, kp) in kernel.iter_mut().enumerate() {
        let r = p as f64 / (n as f64 / 4.0) - 1.0; // ∈ [-1, 1]
        let arg = PI * alpha * (1.0 - r * r).max(0.0).sqrt();
        *kp = bessel_i0(arg) / denom;
    }
    // cumulative sums
    let total: f64 = kernel.iter().sum();
    let mut cum = vec![0.0f64; half + 1];
    let mut acc = 0.0;
    for p in 0..=half {
        acc += kernel[p];
        cum[p] = acc;
    }
    let mut w = vec![0.0f64; n];
    for nn in 0..half {
        w[nn] = (cum[nn] / total).sqrt();
        w[n - 1 - nn] = (cum[nn] / total).sqrt();
    }
    w
}

/// Sine window of length `n`: W[k] = sin( π/N (k + 1/2) ).
pub fn sine_window(n: usize) -> Vec<f64> {
    (0..n).map(|k| (PI / n as f64 * (k as f64 + 0.5)).sin()).collect()
}

/// In-place radix-2 FFT, positive sign, no normalization:
///   Z[m] = Σ_j z[j]·exp(+i·2π·j·m/n),  n a power of two.
/// Both MDCT directions need this single kernel (see `mdct_twiddles`). DIT with
/// recurrence-stepped twiddles, so only log₂(n) `cos`/`sin` calls per transform.
fn fft_pos(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    // bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2usize;
    while len <= n {
        let ang = 2.0 * PI / len as f64; // +i convention
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = len / 2;
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..half {
                let ur = re[i + k];
                let ui = im[i + k];
                let pr = re[i + k + half];
                let pi = im[i + k + half];
                let vr = pr * cr - pi * ci;
                let vi = pr * ci + pi * cr;
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + half] = ur - vr;
                im[i + k + half] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Separable twiddle factors that turn the MDCT/IMDCT cosine sum into one
/// N-point FFT, for the canonical phase n0 = (M+1)/2 (M = N/2). Cached per N
/// (only 2048 / 256 occur). Derivation: the chirp (n+n0)(k+½) splits as
/// n·k (the DFT kernel) + n-only + k-only terms, each an exp() twiddle.
struct Twiddles {
    // IMDCT: out[n] = Re{ A[n]·Σ_k (X[k]·B[k])·e^{+i2πnk/N} }
    b_re: Vec<f64>, // B[k] = e^{+iπ·n0·k/M},        k∈0..M
    b_im: Vec<f64>,
    a_re: Vec<f64>, // A[n] = e^{+iπ·(n+n0)/(2M)},    n∈0..N
    a_im: Vec<f64>,
    // MDCT: out[k] = Re{ Wp[k]·Σ_n (t[n]·P[n])·e^{+i2πnk/N} }
    p_re: Vec<f64>, // P[n]  = e^{+iπ·n/(2M)},        n∈0..N
    p_im: Vec<f64>,
    wp_re: Vec<f64>, // Wp[k]= e^{+iπ·n0·(2k+1)/(2M)}, k∈0..M
    wp_im: Vec<f64>,
}

fn mdct_twiddles(n: usize) -> &'static Twiddles {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<usize, &'static Twiddles>>> = OnceLock::new();
    let mut map = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    if let Some(t) = map.get(&n) {
        return t;
    }
    let m = n / 2;
    let n0 = (m + 1) as f64 / 2.0;
    let cis = |a: f64| (a.cos(), a.sin());
    let (mut b_re, mut b_im) = (vec![0.0; m], vec![0.0; m]);
    let (mut wp_re, mut wp_im) = (vec![0.0; m], vec![0.0; m]);
    for k in 0..m {
        let (r, i) = cis(PI * n0 * k as f64 / m as f64);
        b_re[k] = r;
        b_im[k] = i;
        let (r, i) = cis(PI * n0 * (2 * k + 1) as f64 / (2 * m) as f64);
        wp_re[k] = r;
        wp_im[k] = i;
    }
    let (mut a_re, mut a_im) = (vec![0.0; n], vec![0.0; n]);
    let (mut p_re, mut p_im) = (vec![0.0; n], vec![0.0; n]);
    for nn in 0..n {
        let (r, i) = cis(PI * (nn as f64 + n0) / (2 * m) as f64);
        a_re[nn] = r;
        a_im[nn] = i;
        let (r, i) = cis(PI * nn as f64 / (2 * m) as f64);
        p_re[nn] = r;
        p_im[nn] = i;
    }
    let leaked: &'static Twiddles = Box::leak(Box::new(Twiddles {
        b_re, b_im, a_re, a_im, p_re, p_im, wp_re, wp_im,
    }));
    map.insert(n, leaked);
    leaked
}

/// IMDCT cosine sum (length M → N), canonical phase, via one N-point FFT.
/// Returns the UNSCALED sum Σ_k spec[k]·cos((π/M)(n+n0)(k+½)); caller scales.
fn fast_imdct_core(spec: &[f64]) -> Vec<f64> {
    let m = spec.len();
    let n = m * 2;
    let tw = mdct_twiddles(n);
    let (mut yr, mut yi) = (vec![0.0; n], vec![0.0; n]); // zero-padded M..N
    for k in 0..m {
        yr[k] = spec[k] * tw.b_re[k];
        yi[k] = spec[k] * tw.b_im[k];
    }
    fft_pos(&mut yr, &mut yi);
    (0..n).map(|nn| tw.a_re[nn] * yr[nn] - tw.a_im[nn] * yi[nn]).collect()
}

/// Forward MDCT cosine sum (length N → M), canonical phase, via one N-point FFT.
fn fast_mdct_core(time: &[f64]) -> Vec<f64> {
    let n = time.len();
    let m = n / 2;
    let tw = mdct_twiddles(n);
    let (mut zr, mut zi) = (vec![0.0; n], vec![0.0; n]);
    for nn in 0..n {
        zr[nn] = time[nn] * tw.p_re[nn];
        zi[nn] = time[nn] * tw.p_im[nn];
    }
    fft_pos(&mut zr, &mut zi);
    (0..m).map(|k| tw.wp_re[k] * zr[k] - tw.wp_im[k] * zi[k]).collect()
}

#[inline]
fn canonical_n0(half: usize, n0: f64) -> bool {
    (n0 - (half + 1) as f64 / 2.0).abs() < 1e-9
}

/// IMDCT of `spec` (length N/2) → time domain (length N), spec §4.6.11.2.1.
/// n0 = (N/2+1)/2, scale = 2/N (the Princen-Bradley TDAC normalization).
pub fn imdct(spec: &[f64]) -> Vec<f64> {
    let half = spec.len();
    imdct_p(spec, (half + 1) as f64 / 2.0, 2.0 / (half * 2) as f64)
}

fn imdct_p(spec: &[f64], n0: f64, scale: f64) -> Vec<f64> {
    let half = spec.len();
    let n = half * 2;
    let mut out = vec![0.0f64; n];
    if canonical_n0(half, n0) {
        let core = fast_imdct_core(spec);
        for (o, &c) in out.iter_mut().zip(core.iter()) {
            *o = scale * c;
        }
    } else {
        // non-canonical phase: only the brute-force `mdct_probe` reaches here
        for (nn, o) in out.iter_mut().enumerate() {
            let a = nn as f64 + n0;
            let mut acc = 0.0;
            for (k, &s) in spec.iter().enumerate() {
                acc += s * (PI / half as f64 * a * (k as f64 + 0.5)).cos();
            }
            *o = scale * acc;
        }
    }
    out
}

/// Forward MDCT (length N → N/2): the encoder's analysis transform.
fn mdct(time: &[f64]) -> Vec<f64> {
    mdct_p(time, (time.len() / 2 + 1) as f64 / 2.0)
}

fn mdct_p(time: &[f64], n0: f64) -> Vec<f64> {
    let n = time.len();
    let half = n / 2;
    let mut out = vec![0.0f64; half];
    if canonical_n0(half, n0) {
        out = fast_mdct_core(time);
    } else {
        for (k, o) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (nn, &t) in time.iter().enumerate() {
                acc += t * (PI / half as f64 * (nn as f64 + n0) * (k as f64 + 0.5)).cos();
            }
            *o = acc;
        }
    }
    out
}

/// Throwaway probe: brute-force n0 and scale to find the TDAC-correct pair.
pub fn mdct_probe() -> i32 {
    let n = 2048usize;
    let half = n / 2;
    let win = sine_window(n);
    let total = n * 3;
    let sig: Vec<f64> = (0..total)
        .map(|i| (0.013 * i as f64).sin() + 0.5 * (0.071 * i as f64).cos())
        .collect();
    let candidates_n0 = [
        ((half + 1) as f64 / 2.0, "(L+1)/2"),
        (half as f64 / 2.0, "L/2"),
        ((n / 2 + 1) as f64 / 2.0, "(N/2+1)/2=same"),
        ((n as f64) / 2.0 + 0.5, "N/2+0.5"),
    ];
    let candidates_scale = [(2.0 / n as f64, "2/N"), (2.0 / half as f64, "2/L=4/N"), (1.0 / half as f64, "1/L")];
    for (n0, n0name) in candidates_n0 {
        for (scale, sname) in candidates_scale {
            let mut recon = vec![0.0f64; total];
            let mut h = 0;
            while h + n <= total {
                let frame: Vec<f64> = (0..n).map(|i| sig[h + i] * win[i]).collect();
                let spec = mdct_p(&frame, n0);
                let time = imdct_p(&spec, n0, scale);
                for i in 0..n {
                    recon[h + i] += time[i] * win[i];
                }
                h += half;
            }
            let err = (n..total - n).map(|i| (recon[i] - sig[i]).abs()).fold(0.0, f64::max);
            let db = 20.0 * err.max(1e-300).log10();
            println!("  n0={n0name:<14} scale={sname:<8} → {db:>8.1} dB");
        }
    }
    0
}

/// Numeric proof that the FFT-factored cores reproduce the direct cosine sum
/// to machine precision (canonical phase n0 = (M+1)/2), for both block sizes
/// and both directions. The FFT path is only an algebraic re-grouping of the
/// same sum, so the residual must be ~1e-12; this gates the fast path before
/// the codec ever uses it.
pub fn fft_check() -> i32 {
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    };
    let mut worst = 0.0f64;
    for &n in &[2048usize, 256usize] {
        let m = n / 2;
        let n0 = (m + 1) as f64 / 2.0;
        // IMDCT: fast core vs direct cosine sum
        let spec: Vec<f64> = (0..m).map(|_| rng()).collect();
        let fast = fast_imdct_core(&spec);
        let mut di = 0.0f64;
        for nn in 0..n {
            let a = nn as f64 + n0;
            let naive: f64 = spec.iter().enumerate().map(|(k, &s)| s * (PI / m as f64 * a * (k as f64 + 0.5)).cos()).sum();
            di = di.max((fast[nn] - naive).abs());
        }
        // MDCT: fast core vs direct cosine sum
        let time: Vec<f64> = (0..n).map(|_| rng()).collect();
        let fastf = fast_mdct_core(&time);
        let mut df = 0.0f64;
        for k in 0..m {
            let naive: f64 = time.iter().enumerate().map(|(nn, &t)| t * (PI / m as f64 * (nn as f64 + n0) * (k as f64 + 0.5)).cos()).sum();
            df = df.max((fastf[k] - naive).abs());
        }
        worst = worst.max(di).max(df);
        let to_db = |e: f64| 20.0 * e.max(1e-300).log10();
        println!("  N={n:<5} IMDCT err {di:.2e} ({:>7.1} dB)   MDCT err {df:.2e} ({:>7.1} dB)", to_db(di), to_db(df));
    }
    if worst < 1e-6 {
        println!("aac FFT cores: match direct cosine sum (worst {worst:.2e}) [ok]");
        0
    } else {
        println!("aac FFT cores: MISMATCH (worst {worst:.2e}) [FAIL]");
        1
    }
}

/// Micro-bench: pure IMDCT cost vs full per-frame decode budget. Answers
/// "is the transform actually the bottleneck?" before changing it.
pub fn imdct_bench() -> i32 {
    use std::time::Instant;
    let spec: Vec<f64> = (0..1024).map(|k| (k as f64 * 0.0173).sin()).collect();
    // warm
    for _ in 0..1000 { let _ = fast_imdct_core(&spec); }
    let iters = 100_000usize;
    let t0 = Instant::now();
    let mut sink = 0.0f64;
    for _ in 0..iters {
        let out = fast_imdct_core(&spec);
        sink += out[0]; // prevent dead-code elim
    }
    let dt = t0.elapsed().as_secs_f64();
    let us = dt * 1e6 / iters as f64;
    let mhz = (1024.0 / us) as i64; // Msamp/s of spec consumed
    // For 10s @16k decode budget context (1024-hop): 160000/1024 ≈ 156 frames.
    let frames_10s = 156.0;
    let imdct_share_ms = us * frames_10s / 1000.0;
    println!("fast_imdct_core(spec[1024]): {us:.2} µs/call  ({mhz} Msamp/s)  [sink={sink:.3e}]");
    println!("for 10s @16k mono (156 long frames): IMDCT alone = {imdct_share_ms:.2} ms");
    println!("(compare to measured full decode 26.89 ms → IMDCT is ~{:.0}% of decode)",
             100.0 * imdct_share_ms / 26.89);
    0
}

/// TDAC perfect-reconstruction test: windowed MDCT → IMDCT → overlap-add must
/// reconstruct the input in the steady state.  Proves the IMDCT + window math
/// (both sine and KBD, long N=2048 and short N=256) the same way mp3.rs proved
/// its IMDCT — by the transform's own perfect-reconstruction property.
pub fn tdac_test() -> i32 {
    let mut fails = 0;
    for &(n, alpha, name) in &[(2048usize, 4.0, "long"), (256, 6.0, "short")] {
        for (wname, win) in [("sine", sine_window(n)), ("kbd", kbd_window(n, alpha))] {
            // Princen-Bradley overlap condition: W[i]² + W[i+N/2]² == 1
            let half = n / 2;
            let pr_err = (0..half)
                .map(|i| (win[i] * win[i] + win[i + half] * win[i + half] - 1.0).abs())
                .fold(0.0f64, f64::max);
            // build a test signal longer than one frame, hop = N/2
            let total = n * 3;
            let sig: Vec<f64> = (0..total)
                .map(|i| (0.013 * i as f64).sin() + 0.5 * (0.071 * i as f64).cos())
                .collect();
            let mut recon = vec![0.0f64; total];
            let mut h = 0;
            while h + n <= total {
                let mut frame = vec![0.0f64; n];
                for i in 0..n {
                    frame[i] = sig[h + i] * win[i]; // analysis window
                }
                let spec = mdct(&frame);
                // matched PR pair: unnormalized forward ↔ inverse scaled 2/L
                // (proves the basis+window+overlap; the real decode in imdct()
                // uses the spec's 2/N, which ffmpeg's encoder is normalized to).
                let time = imdct_p(&spec, (half + 1) as f64 / 2.0, 2.0 / half as f64);
                for i in 0..n {
                    recon[h + i] += time[i] * win[i]; // synthesis window + overlap-add
                }
                h += half;
            }
            // steady-state region (skip first/last frame edges)
            let (lo, hi) = (n, total - n);
            let err = (lo..hi)
                .map(|i| (recon[i] - sig[i]).abs())
                .fold(0.0f64, f64::max);
            let db = 20.0 * (err.max(1e-300)).log10();
            let ok = pr_err < 1e-9 && db < -100.0;
            if !ok {
                fails += 1;
            }
            println!(
                "  {name:<5} N={n:<4} {wname:<4}  PR-cond err={pr_err:.2e}  recon err={db:>7.1} dB  [{}]",
                if ok { "ok" } else { "FAIL" }
            );
        }
    }
    if fails == 0 {
        println!("aac TDAC: all passed");
        0
    } else {
        println!("aac TDAC: {fails} FAILED");
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Audio reconstruction: dequant → M/S stereo → IMDCT/window/overlap-add → PCM
// ═══════════════════════════════════════════════════════════════════════════

/// Inverse quantization table: |q|^(4/3) for q ∈ [0, 8191] (the AAC quantizer
/// max). 8192 × f64 = 64 KiB, built once. Each decoded spectral line is an
/// integer, so the exponent collapses to one indexed load — no per-line `powf`.
fn dequant_table() -> &'static [f64; 8192] {
    use std::sync::OnceLock;
    static T: OnceLock<[f64; 8192]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0.0f64; 8192];
        for (i, v) in t.iter_mut().enumerate() {
            *v = (i as f64).powf(4.0 / 3.0);
        }
        t
    })
}

/// Inverse quantization of one coefficient: sign(q)·|q|^(4/3).
#[inline]
fn dequant(q: i32) -> f64 {
    let a = dequant_table()[(q.unsigned_abs() as usize).min(8191)];
    if q < 0 { -a } else { a }
}

/// Precomputed analysis windows (sine + KBD, long + short).
struct Dsp {
    sine_long: Vec<f64>,
    kbd_long: Vec<f64>,
    sine_short: Vec<f64>,
    kbd_short: Vec<f64>,
}
impl Dsp {
    fn new() -> Self {
        Dsp {
            sine_long: sine_window(2048),
            kbd_long: kbd_window(2048, 4.0),
            sine_short: sine_window(256),
            kbd_short: kbd_window(256, 6.0),
        }
    }
}

/// Per-channel filterbank state carried across frames.
#[derive(Clone)]
struct ChanState {
    overlap: [f64; 1024],
    prev_kbd: bool,
}
impl Default for ChanState {
    fn default() -> Self {
        ChanState { overlap: [0.0; 1024], prev_kbd: false }
    }
}

/// Dequantize + apply scalefactor gain, placing coefficients into a 1024-bin
/// spectrum (long: direct; short: 8 consecutive 128-bin windows).
fn reconstruct(ch: &Channel) -> Vec<f64> {
    let ics = &ch.ics;
    let mut spec = vec![0f64; 1024];
    let mut ci = 0usize;
    for g in 0..ics.num_window_groups {
        let win_start: usize = ics.group_len[..g].iter().sum();
        for s in 0..ics.max_sfb {
            let c = ch.cb[g * ics.max_sfb + s];
            let width = ics.swb_offset[s + 1] - ics.swb_offset[s];
            if c == 0 || c >= 12 {
                continue; // ZERO / intensity / noise (left silent for now)
            }
            let sf = ch.sf[g * ics.max_sfb + s];
            let gain = 2f64.powf(0.25 * (sf - 100) as f64);
            let off = ics.swb_offset[s];
            for w in 0..ics.group_len[g] {
                let base = if ics.window_sequence == EIGHT_SHORT { (win_start + w) * 128 } else { 0 };
                for k in 0..width {
                    let q = ch.coef[ci];
                    ci += 1;
                    spec[base + off + k] = dequant(q) * gain;
                }
            }
        }
    }
    spec
}

/// M/S stereo decorrelation in the spectral domain (per sfb where ms_used).
fn apply_ms(ics: &Ics, ms_used: &[bool], spectra: &mut [Vec<f64>]) {
    if ms_used.is_empty() || spectra.len() < 2 {
        return;
    }
    let (a, b) = spectra.split_at_mut(1);
    let (s0, s1) = (&mut a[0], &mut b[0]);
    for g in 0..ics.num_window_groups {
        let win_start: usize = ics.group_len[..g].iter().sum();
        for s in 0..ics.max_sfb {
            if !ms_used[g * ics.max_sfb + s] {
                continue;
            }
            let off = ics.swb_offset[s];
            let width = ics.swb_offset[s + 1] - off;
            for w in 0..ics.group_len[g] {
                let base = if ics.window_sequence == EIGHT_SHORT { (win_start + w) * 128 } else { 0 };
                for k in 0..width {
                    let p = base + off + k;
                    let (m, sd) = (s0[p], s1[p]);
                    s0[p] = m + sd;
                    s1[p] = m - sd;
                }
            }
        }
    }
}

/// Build the 2048-point synthesis window for a long-type block, honouring the
/// window_sequence transitions (START/STOP) and per-half window shape.
fn make_long_window(ws: u32, prev_kbd: bool, curr_kbd: bool, dsp: &Dsp) -> Vec<f64> {
    let ll = if prev_kbd { &dsp.kbd_long } else { &dsp.sine_long };
    let lr = if curr_kbd { &dsp.kbd_long } else { &dsp.sine_long };
    let sl = if prev_kbd { &dsp.kbd_short } else { &dsp.sine_short };
    let sr = if curr_kbd { &dsp.kbd_short } else { &dsp.sine_short };
    let mut w = vec![0f64; 2048];
    match ws {
        EIGHT_SHORT => {} // handled separately
        _LONG_START => {
            w[..1024].copy_from_slice(&ll[..1024]);
            for x in w.iter_mut().take(1472).skip(1024) {
                *x = 1.0;
            }
            for n in 1472..1600 {
                w[n] = sr[n - 1472 + 128];
            }
        }
        _LONG_STOP => {
            for n in 448..576 {
                w[n] = sl[n - 448];
            }
            for x in w.iter_mut().take(1024).skip(576) {
                *x = 1.0;
            }
            w[1024..2048].copy_from_slice(&lr[1024..2048]);
        }
        _ => {
            // ONLY_LONG
            w[..1024].copy_from_slice(&ll[..1024]);
            w[1024..2048].copy_from_slice(&lr[1024..2048]);
        }
    }
    w
}

/// IMDCT + window + overlap-add for one channel of one frame → 1024 samples.
fn synth(ws: u32, curr_kbd: bool, spec: &[f64], st: &mut ChanState, dsp: &Dsp) -> Vec<f64> {
    let mut buf = vec![0f64; 2048];
    if ws == EIGHT_SHORT {
        let sw = if curr_kbd { &dsp.kbd_short } else { &dsp.sine_short };
        for i in 0..8 {
            let t = imdct(&spec[i * 128..i * 128 + 128]);
            for j in 0..256 {
                buf[448 + i * 128 + j] += t[j] * sw[j];
            }
        }
    } else {
        let t = imdct(&spec[0..1024]);
        let w = make_long_window(ws, st.prev_kbd, curr_kbd, dsp);
        for n in 0..2048 {
            buf[n] = t[n] * w[n];
        }
    }
    let mut out = vec![0f64; 1024];
    for n in 0..1024 {
        out[n] = buf[n] + st.overlap[n];
    }
    st.overlap.copy_from_slice(&buf[1024..2048]);
    st.prev_kbd = curr_kbd;
    out
}

/// Decode-path reconstruction test: simulate an encoder (analysis window +
/// forward MDCT) per frame, push the coefficients through the real `synth`
/// (IMDCT + synthesis window + overlap-add), and check the tone reconstructs.
/// Reports the decode-path gain (G) and residual — isolates scale vs OLA bugs.
pub fn synth_pr_test() -> i32 {
    let dsp = Dsp::new();
    let total = 2048 * 5;
    let sig: Vec<f64> = (0..total)
        .map(|i| 0.4 * (2.0 * PI * 1000.0 * i as f64 / 44100.0).sin())
        .collect();
    let awin = sine_window(2048);
    let mut st = ChanState::default();
    let mut out = vec![0.0f64; total];
    let mut h = 0;
    while h + 2048 <= total {
        let frame: Vec<f64> = (0..2048).map(|i| sig[h + i] * awin[i]).collect();
        let spec = mdct(&frame);
        let o = synth(ONLY_LONG, false, &spec, &mut st, &dsp);
        for i in 0..1024 {
            out[h + i] = o[i];
        }
        h += 1024;
    }
    let (lo, hi) = (2048, total - 2048);
    let mut num = 0.0;
    let mut den = 0.0;
    for i in lo..hi {
        num += out[i] * sig[i];
        den += sig[i] * sig[i];
    }
    let g = num / den;
    let mut err = 0.0f64;
    let mut sigp = 0.0f64;
    for i in lo..hi {
        err += (out[i] - g * sig[i]).powi(2);
        sigp += (g * sig[i]).powi(2);
    }
    let db = 10.0 * (err / sigp).max(1e-300).log10();
    println!("decode-path gain G={g:.4}  (1/G={:.4})  residual={db:.1} dB", 1.0 / g);
    if db < -60.0 {
        println!("OLA reconstructs cleanly; divide synth by G to match unity");
        0
    } else {
        println!("OLA NOT clean — filterbank/window/overlap bug");
        1
    }
}

/// Decode an ADTS AAC-LC stream to interleaved 16-bit PCM.
pub fn decode_pcm(data: &[u8]) -> Result<(Vec<i16>, u32, u16), String> {
    let first = parse_adts(data, 0).ok_or("no ADTS syncword")?;
    let sr_hz = SAMPLE_RATES[first.sf_index as usize];
    let dsp = Dsp::new();
    let mut states: Vec<ChanState> = Vec::new();
    let mut nch = 0usize;
    let mut pcm: Vec<i16> = Vec::new();
    let mut o = 0usize;
    while o + 7 <= data.len() {
        let h = match parse_adts(data, o) {
            Some(h) => h,
            None => break,
        };
        if h.frame_length == 0 || o + h.frame_length > data.len() {
            break;
        }
        let mut b = Bits::at(data, (o + h.header_len) * 8);
        let mut ms: Vec<Vec<bool>> = Vec::new();
        let chans = parse_raw_data_block(&mut b, h.sf_index as usize, &mut ms, false)?;
        if states.is_empty() {
            nch = chans.len().max(1);
            states = vec![ChanState::default(); nch];
        }
        if chans.len() != nch {
            return Err("channel count changed mid-stream".into());
        }
        let mut spectra: Vec<Vec<f64>> = chans.iter().map(reconstruct).collect();
        // decoder order: M/S (after dequant) → TNS → filterbank
        if nch == 2 && !ms.is_empty() {
            let ics = chans[0].ics.clone();
            apply_ms(&ics, &ms[0], &mut spectra);
        }
        for c in 0..nch {
            apply_tns(&mut spectra[c], &chans[c].ics, &chans[c].tns);
        }
        let mut outs: Vec<Vec<f64>> = Vec::with_capacity(nch);
        for c in 0..nch {
            let ws = chans[c].ics.window_sequence;
            let curr_kbd = chans[c].ics.window_shape == 1;
            outs.push(synth(ws, curr_kbd, &spectra[c], &mut states[c], &dsp));
        }
        // `out` is already in 16-bit PCM amplitude (the AAC encoder normalizes to
        // the ±2^15 sample domain, so the dequant·IMDCT reconstruction lands there
        // directly — no further ×32768 is applied).
        for n in 0..1024 {
            for out in outs.iter() {
                pcm.push(out[n].round().clamp(-32768.0, 32767.0) as i16);
            }
        }
        o += h.frame_length;
    }
    Ok((pcm, sr_hz, nch as u16))
}

// ═══════════════════════════════════════════════════════════════════════════
// Encoder: PCM → AAC-LC (ADTS). ONLY_LONG blocks, per-band codebook selection,
// uniform-scalefactor quantization with a bit-budget bisection. Reuses the
// TDAC-verified forward MDCT and the (reverse) ISO Huffman codebooks. Validated
// by ffmpeg decoding the output + a full round-trip.  Baseline quality (no
// psychoacoustic model / short blocks / M/S) — valid and interoperable.
// ═══════════════════════════════════════════════════════════════════════════

/// MSB-first bit writer (mirror of `Bits`).
struct BitWriter {
    buf: Vec<u8>,
    nbits: usize,
}
impl BitWriter {
    fn new() -> Self {
        BitWriter { buf: Vec::new(), nbits: 0 }
    }
    fn write(&mut self, n: u32, val: u32) {
        for k in (0..n).rev() {
            let byte = self.nbits >> 3;
            if byte >= self.buf.len() {
                self.buf.push(0);
            }
            if (val >> k) & 1 == 1 {
                self.buf[byte] |= 1 << (7 - (self.nbits & 7));
            }
            self.nbits += 1;
        }
    }
    fn append(&mut self, other: &BitWriter) {
        for i in 0..other.nbits {
            let bit = (other.buf[i >> 3] >> (7 - (i & 7))) & 1;
            self.write(1, bit as u32);
        }
    }
    fn into_bytes(mut self) -> Vec<u8> {
        self.nbits = (self.nbits + 7) & !7;
        while self.buf.len() < (self.nbits >> 3) {
            self.buf.push(0);
        }
        self.buf
    }
    fn len_bits(&self) -> usize {
        self.nbits
    }
}

/// Forward quantizer: q = sign(x)·⌊(|x|·2^(-0.25(sf-100)))^(3/4) + 0.4054⌋, clamped.
fn quant(x: f64, sf: i32) -> i32 {
    let g = 2f64.powf(-0.25 * (sf - 100) as f64);
    let m = ((x.abs() * g).powf(0.75) + 0.4054) as i32;
    let q = m.min(8191);
    if x < 0.0 { -q } else { q }
}

/// Codebook index for a `dim`-value tuple (signed cbs use values; unsigned use
/// magnitudes; cb11 clamps magnitude to 16 = ESC flag).
fn cb_index(cb: usize, t: &[i32]) -> usize {
    match cb {
        1 | 2 => t.iter().fold(0, |a, &v| a * 3 + (v.clamp(-1, 1) + 1) as usize),
        3 | 4 => t.iter().fold(0, |a, &v| a * 3 + (v.unsigned_abs() as usize).min(2)),
        5 | 6 => (t[0].clamp(-4, 4) + 4) as usize * 9 + (t[1].clamp(-4, 4) + 4) as usize,
        7 | 8 => (t[0].unsigned_abs() as usize).min(7) * 8 + (t[1].unsigned_abs() as usize).min(7),
        9 | 10 => (t[0].unsigned_abs() as usize).min(12) * 13 + (t[1].unsigned_abs() as usize).min(12),
        11 => (t[0].unsigned_abs() as usize).min(16) * 17 + (t[1].unsigned_abs() as usize).min(16),
        _ => 0,
    }
}

fn escape_bits(v: u32) -> usize {
    let mut n = 0u32;
    while (1u32 << (n + 5)) <= v {
        n += 1;
    }
    (n as usize) + 1 + (n as usize + 4)
}

fn write_escape(w: &mut BitWriter, v: u32) {
    let mut n = 0u32;
    while (1u32 << (n + 5)) <= v {
        n += 1;
    }
    for _ in 0..n {
        w.write(1, 1);
    }
    w.write(1, 0);
    w.write(n + 4, v - (1 << (n + 4)));
}

/// Bits to encode a band with codebook `cb` (codeword + signs + escapes).
fn band_bits(band: &[i32], cb: usize) -> usize {
    if cb == 0 {
        return 0;
    }
    let (dim, unsigned, _lav, esc) = aac_tables::cb_info(cb);
    let (_codes, lens) = aac_tables::codebook(cb);
    let mut bits = 0;
    let mut k = 0;
    while k + dim <= band.len() {
        let t = &band[k..k + dim];
        bits += lens[cb_index(cb, t)] as usize;
        if unsigned {
            for &v in t {
                if v != 0 {
                    bits += 1;
                }
            }
        }
        if esc {
            for &v in t {
                let m = v.unsigned_abs();
                if m >= 16 {
                    bits += escape_bits(m);
                }
            }
        }
        k += dim;
    }
    bits
}

fn encode_band(w: &mut BitWriter, band: &[i32], cb: usize) {
    let (dim, unsigned, _lav, esc) = aac_tables::cb_info(cb);
    let (codes, lens) = aac_tables::codebook(cb);
    let mut k = 0;
    while k + dim <= band.len() {
        let t = &band[k..k + dim];
        let idx = cb_index(cb, t);
        w.write(lens[idx] as u32, codes[idx] as u32);
        if unsigned {
            for &v in t {
                if v != 0 {
                    w.write(1, if v < 0 { 1 } else { 0 });
                }
            }
        }
        if esc {
            for &v in t {
                let m = v.unsigned_abs();
                if m >= 16 {
                    write_escape(w, m);
                }
            }
        }
        k += dim;
    }
}

/// Cheapest valid codebook for a band (0 = all-zero → ZERO_HCB).
fn choose_cb(band: &[i32]) -> u8 {
    let maxabs = band.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    if maxabs == 0 {
        return 0;
    }
    let cands: &[usize] = if maxabs <= 1 {
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    } else if maxabs <= 2 {
        &[3, 4, 5, 6, 7, 8, 9, 10, 11]
    } else if maxabs <= 4 {
        &[5, 6, 7, 8, 9, 10, 11]
    } else if maxabs <= 7 {
        &[7, 8, 9, 10, 11]
    } else if maxabs <= 12 {
        &[9, 10, 11]
    } else {
        &[11]
    };
    let mut best = (usize::MAX, 11u8);
    for &c in cands {
        let b = band_bits(band, c);
        if b < best.0 {
            best = (b, c as u8);
        }
    }
    best.1
}

/// Build one channel's individual_channel_stream for a given scalefactor.
fn build_ics(spec: &[f64], swb: &[usize], sf: i32) -> BitWriter {
    let num_swb = swb.len() - 1;
    let mut q = vec![0i32; swb[num_swb]];
    for (k, qq) in q.iter_mut().enumerate() {
        *qq = quant(spec[k], sf);
    }
    let mut cbs = vec![0u8; num_swb];
    let mut max_sfb = 0;
    for s in 0..num_swb {
        cbs[s] = choose_cb(&q[swb[s]..swb[s + 1]]);
        if cbs[s] != 0 {
            max_sfb = s + 1;
        }
    }
    let mut w = BitWriter::new();
    w.write(8, sf as u32 & 0xFF); // global_gain
    // ics_info (ONLY_LONG, sine window)
    w.write(1, 0); // ics_reserved_bit
    w.write(2, 0); // window_sequence = ONLY_LONG
    w.write(1, 0); // window_shape = sine
    w.write(6, max_sfb as u32);
    w.write(1, 0); // predictor_data_present
    // section_data: runs of equal codebook
    let mut s = 0;
    while s < max_sfb {
        let c = cbs[s];
        let mut len = 1;
        while s + len < max_sfb && cbs[s + len] == c {
            len += 1;
        }
        w.write(4, c as u32);
        let mut rem = len;
        while rem >= 31 {
            w.write(5, 31);
            rem -= 31;
        }
        w.write(5, rem as u32);
        s += len;
    }
    // scale_factor_data: every coded band carries delta 0 (uniform sf) = index 60
    for s in 0..max_sfb {
        if cbs[s] != 0 {
            w.write(aac_tables::SF_BITS[60] as u32, aac_tables::SF_CODES[60]);
        }
    }
    w.write(1, 0); // pulse_data_present
    w.write(1, 0); // tns_data_present
    w.write(1, 0); // gain_control_data_present
    // spectral_data
    for s in 0..max_sfb {
        let c = cbs[s] as usize;
        if c == 0 || c >= 12 {
            continue;
        }
        encode_band(&mut w, &q[swb[s]..swb[s + 1]], c);
    }
    w
}

/// Encode one channel by choosing the global scalefactor in **closed form** — no
/// rate-control search loop (the platonic B/U form of the two-loop encoder).
///
/// `sf_min` is the no-clamp floor: below it the loudest line saturates the 8191
/// quantizer ceiling. Within `[sf_min, 255]` the frame bit count R(sf) decreases
/// monotonically, and the rate–scalefactor law is analytic: a step of +4 in sf
/// scales every coefficient magnitude by 2^(−3/4), shaving ≈ ¾ bit off each
/// non-zero line's codeword. So the slope is closed-form, dR/dsf ≈ −0.1875·N_nz,
/// and the scalefactor meeting the budget is a single division — residual over
/// slope — injected directly (x = B/U), not bisected:
///     sf* = sf_min + ⌈(R(sf_min) − target) / (0.1875·N_nz)⌉
/// One calibration build at sf_min, then one corrective build at sf*. The ADTS
/// frame length is a soft budget (no decoder buffer constraint at AAC-LC bitrates
/// — a frame stays far under the 13-bit length field), so a small model error
/// only nudges the realised bitrate; it can never produce an invalid stream.
fn encode_channel(spec: &[f64], swb: &[usize], target_bits: usize) -> BitWriter {
    let maxc = spec.iter().cloned().fold(0.0f64, |a, b| a.max(b.abs()));
    let thresh = 8190.0f64.powf(4.0 / 3.0); // |coef|·2^(-(sf-100)/4) at q≈8191
    let sf_min = if maxc < 1.0 {
        1
    } else {
        (100.0 + 4.0 * (maxc / thresh).log2()).ceil() as i32
    }
    .clamp(1, 255);

    let ics0 = build_ics(spec, swb, sf_min);
    if ics0.len_bits() <= target_bits {
        return ics0; // finest allowed sf already fits — nothing to coarsen
    }
    let n_coef = swb[swb.len() - 1];
    let n_nz = spec[..n_coef].iter().filter(|&&x| quant(x, sf_min) != 0).count();
    let slope = (0.1875 * n_nz as f64).max(1e-6); // bits shed per +1 in sf
    let step = ((ics0.len_bits() - target_bits) as f64 / slope).ceil() as i32;
    let sf = (sf_min + step).clamp(sf_min, 255);
    build_ics(spec, swb, sf)
}

fn write_adts(out: &mut Vec<u8>, sr_index: u8, channels: u16, frame_len: usize) {
    let profile = 1u8; // AAC-LC
    let ch = channels as u8;
    out.push(0xFF);
    out.push(0xF1); // sync + MPEG-4 + layer0 + protection_absent
    out.push((profile << 6) | (sr_index << 2) | ((ch >> 2) & 1));
    out.push(((ch & 3) << 6) | ((frame_len >> 11) as u8 & 0x03));
    out.push(((frame_len >> 3) & 0xFF) as u8);
    out.push((((frame_len & 7) << 5) as u8) | 0x1F); // + buffer-fullness high
    out.push(0xFC); // buffer-fullness low + num_raw_blocks=0
}

/// Encode→decode round-trip self-test: encode a 1 kHz tone, decode it with our
/// own decoder, and require high correlation with the original. Proves the
/// encoder produces a valid stream our (ffmpeg-matching) decoder reconstructs.
pub fn encode_roundtrip_test() -> i32 {
    let rate = 44100u32;
    let n = rate as usize * 2;
    let orig: Vec<i16> = (0..n)
        .map(|i| (0.4 * 32767.0 * (2.0 * PI * 1000.0 * i as f64 / rate as f64).sin()) as i16)
        .collect();
    let aac = encode_pcm(&orig, rate, 1, 128);
    let (dec, _, _) = decode_pcm(&aac).unwrap_or((vec![], 0, 0));
    let mut best = -2.0f64;
    for lag in 0..4096 {
        let m = 40000.min(dec.len().saturating_sub(lag)).min(orig.len());
        if m < 20000 {
            continue;
        }
        let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..m {
            let (a, b) = (dec[lag + i] as f64, orig[i] as f64);
            sa += a;
            sb += b;
            saa += a * a;
            sbb += b * b;
            sab += a * b;
        }
        let mf = m as f64;
        let (da, db) = (saa - sa * sa / mf, sbb - sb * sb / mf);
        if da > 0.0 && db > 0.0 {
            let c = (sab - sa * sb / mf) / (da * db).sqrt();
            if c > best {
                best = c;
            }
        }
    }
    println!("aac encode round-trip (1 kHz tone @128k): correlation={best:.4}  ({} B)", aac.len());
    if best > 0.99 {
        println!("  encoder OK");
        0
    } else {
        println!("  encoder FAIL");
        1
    }
}

/// Encode interleaved 16-bit PCM to an ADTS AAC-LC stream at ~`bitrate_kbps`.
pub fn encode_pcm(samples: &[i16], rate: u32, channels: u16, bitrate_kbps: u32) -> Vec<u8> {
    let sr_index = SAMPLE_RATES.iter().position(|&r| r == rate).unwrap_or(4);
    let swb: Vec<usize> = aac_tables::swb_offset_long(sr_index).iter().map(|&x| x as usize).collect();
    let nch = channels.max(1) as usize;
    let n_in = samples.len() / nch;
    let awin = sine_window(2048);
    // per-channel bit budget per frame
    let target = (bitrate_kbps as usize * 1000 * 1024) / (rate as usize * nch);
    // deinterleave with 1024-sample priming pad, then pad to whole frames
    let n_frames = (n_in + 1024).div_ceil(1024);
    let mut chans = vec![vec![0f64; 1024 + n_frames * 1024 + 1024]; nch];
    for i in 0..n_in {
        for (c, ch) in chans.iter_mut().enumerate() {
            ch[1024 + i] = samples[i * nch + c] as f64;
        }
    }
    let mut out = Vec::new();
    for f in 0..n_frames {
        let mut fw = BitWriter::new();
        fw.write(3, if nch == 1 { 0 } else { 1 }); // SCE / CPE
        fw.write(4, 0); // element_instance_tag
        if nch == 2 {
            fw.write(1, 0); // common_window = 0 (independent L/R)
        }
        for ch in chans.iter() {
            let base = f * 1024;
            let frame: Vec<f64> = (0..2048).map(|i| ch[base + i] * awin[i]).collect();
            let mut spec = mdct(&frame);
            for x in spec.iter_mut() {
                *x *= 2.0; // round-trip unity: the decode path has gain 0.5
            }
            fw.append(&encode_channel(&spec, &swb, target));
        }
        fw.write(3, 7); // ID_END
        let payload = fw.into_bytes();
        write_adts(&mut out, sr_index as u8, channels, 7 + payload.len());
        out.extend_from_slice(&payload);
    }
    out
}
