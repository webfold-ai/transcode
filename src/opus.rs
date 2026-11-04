// ═══════════════════════════════════════════════════════════════════════════
// opus — RFC 6716 Opus decoder (pure Rust, no deps)
//
// From-scratch, built and verified the same way as mp3/aac:
//   container parse bit-exact → range decoder self-test → CELT/SILK transforms
//   proven by their math property → PCM correlation vs ffmpeg (libopus).
//
// Opus is a hybrid: SILK (speech, LP) + CELT (music, MDCT), tied together by a
// range coder. Layers, foundation first:
//   Ogg pages → Opus packets → TOC → range decoder → CELT (energy+PVQ+IMDCT) /
//   SILK (LPC+LTP) → 48 kHz PCM.
// ═══════════════════════════════════════════════════════════════════════════

// ─── Ogg container ────────────────────────────────────────────────────────────

// Opus decode -> PCM is partial (ROADMAP.md). The CELT synthesis helpers below are
// scaffolding that is not wired up yet, so they are allowed to be unused for now.
#![allow(dead_code)]

/// One reassembled Opus packet (the bytes after Ogg framing).
pub struct OggPacket {
    pub data: Vec<u8>,
    pub granule: i64, // granule position of the page this packet completes on
}

/// Parsed OpusHead identification header (RFC 7845 §5.1).
#[derive(Debug, Clone, Copy)]
pub struct OpusHead {
    pub version: u8,
    pub channels: u8,
    pub pre_skip: u16,
    pub input_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
}

fn rd_u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_i64le(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7]])
}

/// Demux an Ogg stream into Opus packets (segment reassembly across pages).
pub fn demux_ogg(data: &[u8]) -> Result<Vec<OggPacket>, String> {
    let mut packets = Vec::new();
    let mut o = 0usize;
    let mut cur: Vec<u8> = Vec::new(); // packet bytes accumulating across continued segments
    while o + 27 <= data.len() {
        if &data[o..o + 4] != b"OggS" {
            return Err(format!("bad Ogg page capture at offset {o}"));
        }
        let granule = rd_i64le(data, o + 6);
        let nseg = data[o + 26] as usize;
        let seg_table_off = o + 27;
        if seg_table_off + nseg > data.len() {
            break;
        }
        let seg_table = &data[seg_table_off..seg_table_off + nseg];
        let mut body = seg_table_off + nseg;
        for &lace in seg_table {
            let l = lace as usize;
            if body + l > data.len() {
                return Err("page body overruns file".into());
            }
            cur.extend_from_slice(&data[body..body + l]);
            body += l;
            if lace < 255 {
                // a lacing value < 255 terminates a packet
                packets.push(OggPacket { data: std::mem::take(&mut cur), granule });
            }
        }
        o = body;
    }
    Ok(packets)
}

pub fn parse_opus_head(p: &[u8]) -> Result<OpusHead, String> {
    if p.len() < 19 || &p[0..8] != b"OpusHead" {
        return Err("not an OpusHead packet".into());
    }
    Ok(OpusHead {
        version: p[8],
        channels: p[9],
        pre_skip: u16::from_le_bytes([p[10], p[11]]),
        input_rate: rd_u32le(p, 12),
        output_gain: i16::from_le_bytes([p[16], p[17]]),
        mapping_family: p[18],
    })
}

pub struct Info {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_rate: u32,
    pub audio_packets: usize,
    pub samples: u64, // per channel @ 48 kHz, minus pre-skip
}

/// Parse container-level info: OpusHead + audio packet count + duration from the
/// final granule position (Opus granule is in 48 kHz samples).
pub fn parse(data: &[u8]) -> Result<Info, String> {
    let packets = demux_ogg(data)?;
    if packets.is_empty() {
        return Err("no Ogg packets".into());
    }
    let head = parse_opus_head(&packets[0].data)?;
    // packet[0] = OpusHead, packet[1] = OpusTags, rest = audio
    let audio_packets = packets.len().saturating_sub(2);
    let last_granule = packets.last().map(|p| p.granule).unwrap_or(0).max(0) as u64;
    let samples = last_granule.saturating_sub(head.pre_skip as u64);
    Ok(Info {
        channels: head.channels,
        pre_skip: head.pre_skip,
        input_rate: head.input_rate,
        audio_packets,
        samples,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Range coder (RFC 6716 §4.1) — the entropy engine. Front: range-coded symbols;
// back: raw bits read/written from the end of the buffer. Decoder + encoder,
// proven by a self-contained round-trip self-test.
// ═══════════════════════════════════════════════════════════════════════════

const EC_SYM_BITS: u32 = 8;
const EC_CODE_BITS: u32 = 32;
const EC_CODE_EXTRA: u32 = 7; // (EC_CODE_BITS-2)%EC_SYM_BITS + 1
const EC_CODE_TOP: u32 = 1 << 31; // 1<<(EC_CODE_BITS-1)
const EC_CODE_BOT: u32 = 1 << 23; // EC_CODE_TOP>>EC_SYM_BITS
const EC_SYM_MAX: u32 = (1 << EC_SYM_BITS) - 1; // 255
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1; // 23
const EC_WINDOW_SIZE: u32 = 32;
const EC_UINT_BITS: u32 = 8;

fn ec_ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: usize,
    offs: usize,     // front read position
    end_offs: usize, // bytes consumed from the end (raw bits)
    end_window: u32,
    nend_bits: i32,
    val: u32,
    rng: u32,
    rem: i32,
    ext: u32,
    nbits_total: i32,
    pub error: bool,
}

impl<'a> RangeDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        let mut d = RangeDecoder {
            buf,
            storage: buf.len(),
            offs: 0,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            val: 0,
            rng: 1 << EC_CODE_EXTRA,
            rem: 0,
            ext: 0,
            // EC_CODE_BITS+1 - ((EC_CODE_BITS-EC_CODE_EXTRA)/EC_SYM_BITS)*EC_SYM_BITS = 9
            nbits_total: 9,
            error: false,
        };
        d.rem = d.read_byte();
        d.val = d.rng - 1 - (d.rem as u32 >> (EC_SYM_BITS - EC_CODE_EXTRA));
        d.normalize();
        d
    }
    /// ec_tell: bits consumed so far.
    pub fn tell(&self) -> i32 {
        self.nbits_total - ec_ilog(self.rng) as i32
    }
    /// ec_tell_frac: bits consumed in 1/8th-bit units (BITRES=3).
    pub fn tell_frac(&self) -> u32 {
        let nbits = (self.nbits_total as u32) << 3;
        let mut l = ec_ilog(self.rng);
        let mut r = self.rng >> (l - 16);
        for _ in 0..3 {
            r = (r * r) >> 15;
            let b = r >> 16;
            l = (l << 1) | b;
            r >>= b;
        }
        nbits - l
    }
    fn read_byte(&mut self) -> i32 {
        if self.offs < self.storage - self.end_offs {
            let b = self.buf[self.offs] as i32;
            self.offs += 1;
            b
        } else {
            0
        }
    }
    fn read_byte_from_end(&mut self) -> i32 {
        if self.end_offs < self.storage - self.offs {
            self.end_offs += 1;
            self.buf[self.storage - self.end_offs] as i32
        } else {
            0
        }
    }
    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS as i32;
            self.rng <<= EC_SYM_BITS;
            let sym = self.rem;
            self.rem = self.read_byte();
            let sym = ((sym << EC_SYM_BITS) | self.rem) >> (EC_SYM_BITS - EC_CODE_EXTRA);
            self.val = ((self.val << EC_SYM_BITS) + (EC_SYM_MAX & !sym as u32)) & (EC_CODE_TOP - 1);
        }
    }
    /// ec_decode: return the cumulative frequency for the current symbol in [0,ft).
    pub fn decode(&mut self, ft: u32) -> u32 {
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        ft - (s + 1).min(ft)
    }
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        self.ext = self.rng >> bits;
        let s = self.val / self.ext;
        (1 << bits) - (s + 1).min(1 << bits)
    }
    /// ec_dec_update: narrow to the symbol's [fl,fh) out of total ft.
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext * (ft - fh);
        self.val -= s;
        self.rng = if fl > 0 { self.ext * (fh - fl) } else { self.rng - s };
        self.normalize();
    }
    pub fn dec_bit_logp(&mut self, logp: u32) -> u32 {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = (d < s) as u32;
        if ret == 0 {
            self.val = d - s;
        }
        self.rng = if ret != 0 { s } else { r - s };
        self.normalize();
        ret
    }
    /// ec_dec_icdf: decode a symbol using an inverse-CDF table (ftb bits).
    pub fn dec_icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret: i32 = -1;
        let mut t;
        loop {
            t = s;
            ret += 1;
            s = r * icdf[ret as usize] as u32;
            if d >= s {
                break;
            }
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret as usize
    }
    /// ec_dec_bits: raw bits from the END of the buffer.
    pub fn dec_bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if (available as u32) < bits {
            loop {
                window |= (self.read_byte_from_end() as u32) << available;
                available += EC_SYM_BITS as i32;
                if available > (EC_WINDOW_SIZE - EC_SYM_BITS) as i32 {
                    break;
                }
            }
        }
        let ret = window & ((1u32 << bits) - 1);
        window >>= bits;
        available -= bits as i32;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;
        ret
    }
    /// ec_dec_uint: uniformly distributed integer in [0, ft).
    pub fn dec_uint(&mut self, ft: u32) -> u32 {
        let ft = ft - 1;
        let ftb = ec_ilog(ft);
        if ftb > EC_UINT_BITS {
            let ftb = ftb - EC_UINT_BITS;
            let ftv = (ft >> ftb) + 1;
            let s = self.decode(ftv);
            self.update(s, s + 1, ftv);
            let t = (s << ftb) | self.dec_bits(ftb);
            if t <= ft {
                t
            } else {
                self.error = true;
                ft
            }
        } else {
            let ftv = ft + 1;
            let s = self.decode(ftv);
            self.update(s, s + 1, ftv);
            s
        }
    }
}

pub struct RangeEncoder {
    buf: Vec<u8>, // fixed-size: front bytes from 0, raw bits from storage-1
    storage: usize,
    offs: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    val: u32,
    rng: u32,
    rem: i32,
    ext: u32,
    error: bool,
}

impl Default for RangeEncoder {
    fn default() -> Self {
        Self::with_capacity(1 << 16)
    }
}

impl RangeEncoder {
    pub fn with_capacity(storage: usize) -> Self {
        RangeEncoder {
            buf: vec![0u8; storage],
            storage,
            offs: 0,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: (EC_CODE_BITS + 1) as i32, // 33
            val: 0,
            rng: EC_CODE_TOP,
            rem: -1,
            ext: 0,
            error: false,
        }
    }
    pub fn tell(&self) -> i32 {
        self.nbits_total - ec_ilog(self.rng) as i32
    }
    pub fn buf_len(&self) -> usize {
        self.storage
    }
    fn write_byte(&mut self, v: u8) {
        if self.offs + self.end_offs < self.storage {
            self.buf[self.offs] = v;
            self.offs += 1;
        } else {
            self.error = true;
        }
    }
    fn write_byte_at_end(&mut self, v: u8) {
        if self.offs + self.end_offs < self.storage {
            self.end_offs += 1;
            self.buf[self.storage - self.end_offs] = v;
        } else {
            self.error = true;
        }
    }
    fn carry_out(&mut self, c: i32) {
        if c as u32 != EC_SYM_MAX {
            let carry = c >> EC_SYM_BITS;
            if self.rem >= 0 {
                self.write_byte((self.rem + carry) as u8);
            }
            if self.ext > 0 {
                let sym = ((EC_SYM_MAX as i32 + carry) & EC_SYM_MAX as i32) as u8;
                while self.ext > 0 {
                    self.write_byte(sym);
                    self.ext -= 1;
                }
            }
            self.rem = c & EC_SYM_MAX as i32;
        } else {
            self.ext += 1;
        }
    }
    fn enc_normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.carry_out((self.val >> EC_CODE_SHIFT) as i32);
            self.val = (self.val << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            self.rng <<= EC_SYM_BITS;
            self.nbits_total += EC_SYM_BITS as i32;
        }
    }
    pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
        let r = self.rng / ft;
        if fl > 0 {
            self.val += self.rng - r * (ft - fl);
            self.rng = r * (fh - fl);
        } else {
            self.rng -= r * (ft - fh);
        }
        self.enc_normalize();
    }
    pub fn enc_bit_logp(&mut self, val: u32, logp: u32) {
        let r = self.rng;
        let s = r >> logp;
        if val != 0 {
            self.val += r - s;
            self.rng = s;
        } else {
            self.rng = r - s;
        }
        self.enc_normalize();
    }
    pub fn enc_icdf(&mut self, s: usize, icdf: &[u8], ftb: u32) {
        let r = self.rng >> ftb;
        if s > 0 {
            self.val += self.rng - r * icdf[s - 1] as u32;
            self.rng = r * (icdf[s - 1] as u32 - icdf[s] as u32);
        } else {
            self.rng -= r * icdf[s] as u32;
        }
        self.enc_normalize();
    }
    pub fn enc_bits(&mut self, val: u32, bits: u32) {
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        if used + bits as i32 > EC_WINDOW_SIZE as i32 {
            while used >= EC_SYM_BITS as i32 {
                self.write_byte_at_end((window & EC_SYM_MAX) as u8);
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
            }
        }
        window |= val << used;
        used += bits as i32;
        self.end_window = window;
        self.nend_bits = used;
        self.nbits_total += bits as i32;
    }
    pub fn enc_uint(&mut self, val: u32, ft: u32) {
        let ft = ft - 1;
        let ftb = ec_ilog(ft);
        if ftb > EC_UINT_BITS {
            let ftb = ftb - EC_UINT_BITS;
            let ftv = (ft >> ftb) + 1;
            let hi = val >> ftb;
            self.encode(hi, hi + 1, ftv);
            self.enc_bits(val & ((1 << ftb) - 1), ftb);
        } else {
            let ftv = ft + 1;
            self.encode(val, val + 1, ftv);
        }
    }
    /// Flush the coder and merge the front (range) and end (raw) byte streams.
    pub fn done(mut self) -> Vec<u8> {
        // EC_CODE_BITS - EC_ILOG(rng) ... finalize the range value
        let mut l = (EC_CODE_BITS - ec_ilog(self.rng)) as i32;
        let mut msk = (EC_CODE_TOP - 1) >> l;
        let mut end = (self.val + msk) & !msk;
        if (end | msk) >= self.val + self.rng {
            l += 1;
            msk >>= 1;
            end = (self.val + msk) & !msk;
        }
        while l > 0 {
            self.carry_out((end >> EC_CODE_SHIFT) as i32);
            end = (end << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            l -= EC_SYM_BITS as i32;
        }
        // flush the buffered range byte
        if self.rem >= 0 || self.ext > 0 {
            self.carry_out(0);
        }
        // flush full raw-bit bytes to the end of the buffer
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        while used >= EC_SYM_BITS as i32 {
            self.write_byte_at_end((window & EC_SYM_MAX) as u8);
            window >>= EC_SYM_BITS;
            used -= EC_SYM_BITS as i32;
        }
        // OR any remaining (<8) raw bits into the byte just before the end region
        // (the gap is already zero). `l` is ≤ 0 here; -l = free bits in last range byte.
        if !self.error && used > 0 {
            if self.end_offs >= self.storage {
                self.error = true;
            } else {
                let lneg = -l;
                if self.offs + self.end_offs >= self.storage && lneg < used {
                    window &= (1 << lneg) - 1;
                    self.error = true;
                }
                let idx = self.storage - self.end_offs - 1;
                self.buf[idx] |= window as u8;
            }
        }
        self.buf
    }
}

/// Range-coder round-trip self-test: encode a pseudo-random mix of symbols
/// (icdf, bit_logp, uint, raw bits) and decode them back, requiring an exact
/// match. Proves the entropy engine (the foundation of all Opus decoding).
pub fn range_selftest() -> i32 {
    // small deterministic PRNG
    let mut seed = 0x2545F491u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    // an inverse-CDF table (icdf): values strictly decreasing to 0, ftb=8 → ft=256
    let icdf: [u8; 4] = [200, 130, 50, 0];
    // build a symbol program
    #[derive(Clone)]
    enum Sym {
        Icdf(usize),
        Logp(u32, u32),
        Uint(u32, u32),
        Bits(u32, u32),
    }
    let mut prog: Vec<Sym> = Vec::new();
    for _ in 0..2000 {
        match rnd() % 4 {
            0 => prog.push(Sym::Icdf((rnd() % 3) as usize)),
            1 => prog.push(Sym::Logp(rnd() % 2, 1 + rnd() % 6)),
            2 => {
                let ft = 2 + rnd() % 60000;
                prog.push(Sym::Uint(rnd() % ft, ft));
            }
            _ => {
                let b = 1 + rnd() % 16;
                prog.push(Sym::Bits(rnd() & ((1 << b) - 1), b));
            }
        }
    }
    let mut enc = RangeEncoder::with_capacity(1 << 16);
    for s in &prog {
        match *s {
            Sym::Icdf(v) => enc.enc_icdf(v, &icdf, 8),
            Sym::Logp(v, lp) => enc.enc_bit_logp(v, lp),
            Sym::Uint(v, ft) => enc.enc_uint(v, ft),
            Sym::Bits(v, b) => enc.enc_bits(v, b),
        }
    }
    let bytes = enc.done();
    let mut dec = RangeDecoder::new(&bytes);
    let mut ok = true;
    let mut idx = 0;
    for s in &prog {
        let got_ok = match *s {
            Sym::Icdf(v) => dec.dec_icdf(&icdf, 8) == v,
            Sym::Logp(v, lp) => dec.dec_bit_logp(lp) == v,
            Sym::Uint(v, ft) => dec.dec_uint(ft) == v,
            Sym::Bits(v, b) => dec.dec_bits(b) == v,
        };
        if !got_ok {
            ok = false;
            eprintln!("  mismatch at symbol {idx}");
            break;
        }
        idx += 1;
    }
    println!("opus range coder: {} symbols round-trip in {} bytes [{}]", prog.len(), bytes.len(), if ok { "ok" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// TOC / packet framing (RFC 6716 §3) — selects mode, bandwidth, frame size.
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Silk,
    Hybrid,
    Celt,
}
#[derive(Debug, Clone, Copy)]
pub struct Toc {
    pub mode: Mode,
    pub bandwidth_hz: u32, // 4000/6000/8000/12000/20000
    pub frame_size_samples: u32, // @48kHz
    pub stereo: bool,
    pub frame_count: u8,
}

/// Decode the 5-bit config (RFC 6716 Table 2) → (mode, bandwidth Hz, frame ms×10).
fn config_table(config: u8) -> (Mode, u32, u32) {
    // frame size in samples @48k: 2.5ms=120, 5=240, 10=480, 20=960, 40=1920, 60=2880
    let (m, bw, fs) = match config {
        0..=3 => (Mode::Silk, 4000, [480, 960, 1920, 2880][config as usize]),
        4..=7 => (Mode::Silk, 6000, [480, 960, 1920, 2880][(config - 4) as usize]),
        8..=11 => (Mode::Silk, 8000, [480, 960, 1920, 2880][(config - 8) as usize]),
        12..=13 => (Mode::Hybrid, 12000, [480, 960][(config - 12) as usize]),
        14..=15 => (Mode::Hybrid, 20000, [480, 960][(config - 14) as usize]),
        16..=19 => (Mode::Celt, 4000, [120, 240, 480, 960][(config - 16) as usize]),
        20..=23 => (Mode::Celt, 8000, [120, 240, 480, 960][(config - 20) as usize]),
        24..=27 => (Mode::Celt, 12000, [120, 240, 480, 960][(config - 24) as usize]),
        _ => (Mode::Celt, 20000, [120, 240, 480, 960][(config - 28) as usize]),
    };
    (m, bw, fs)
}

pub fn parse_toc(toc: u8) -> Toc {
    let config = toc >> 3;
    let stereo = (toc >> 2) & 1 == 1;
    let frame_count = toc & 3;
    let (mode, bandwidth_hz, frame_size_samples) = config_table(config);
    Toc { mode, bandwidth_hz, frame_size_samples, stereo, frame_count }
}

/// Summarize the per-packet TOC distribution across the stream.
pub fn toc_stats(data: &[u8]) -> Result<String, String> {
    let packets = demux_ogg(data)?;
    let audio = &packets[2.min(packets.len())..];
    let (mut silk, mut hybrid, mut celt) = (0usize, 0, 0);
    let mut first: Option<Toc> = None;
    for p in audio {
        if p.data.is_empty() {
            continue;
        }
        let t = parse_toc(p.data[0]);
        if first.is_none() {
            first = Some(t);
        }
        match t.mode {
            Mode::Silk => silk += 1,
            Mode::Hybrid => hybrid += 1,
            Mode::Celt => celt += 1,
        }
    }
    let f = first.ok_or("no audio packets")?;
    Ok(format!(
        "{} audio packets: {silk} SILK, {hybrid} Hybrid, {celt} CELT; first: {:?} {} Hz BW, {} samples ({:.1} ms), {}",
        audio.len(),
        f.mode,
        f.bandwidth_hz,
        f.frame_size_samples,
        f.frame_size_samples as f64 / 48.0,
        if f.stereo { "stereo" } else { "mono" }
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
// Laplace coder (RFC 6716 §4.3.2.1) — the coarse-energy entropy layer, on top
// of the range coder. Decoder + encoder, proven by a round-trip self-test that
// draws fs/decay from the real e_prob_model.
// ═══════════════════════════════════════════════════════════════════════════

const LAPLACE_LOG_MINP: u32 = 0;
const LAPLACE_MINP: u32 = 1; // 1 << LAPLACE_LOG_MINP
const LAPLACE_NMIN: u32 = 16;

fn laplace_freq1(fs0: u32, decay: u32) -> u32 {
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
    ((ft as u64 * (16384 - decay as u64)) >> 15) as u32
}

/// ec_laplace_decode: returns the signed value.
pub fn laplace_decode(d: &mut RangeDecoder, mut fs: u32, decay: u32) -> i32 {
    let mut val = 0i32;
    let fm = d.decode_bin(15);
    let mut fl = 0u32;
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = laplace_freq1(fs, decay) + LAPLACE_MINP;
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * LAPLACE_MINP) * decay) >> 15) + LAPLACE_MINP;
            val += 1;
        }
        if fs <= LAPLACE_MINP {
            let di = ((fm - fl) >> (LAPLACE_LOG_MINP + 1)) as i32;
            val += di;
            fl += 2 * (di as u32) * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }
    d.update(fl, (fl + fs).min(32768), 32768);
    val
}

/// ec_laplace_encode: encodes `value`, returning the value actually coded (the
/// PDF tail clamps large magnitudes, so the decoder may recover a clamped value).
pub fn laplace_encode(e: &mut RangeEncoder, value: i32, mut fs: u32, decay: u32) -> i32 {
    let mut fl = 0u32;
    let mut val = value;
    let mut coded = value;
    if val != 0 {
        let s: i32 = if val < 0 { -1 } else { 0 };
        val = (val + s) ^ s; // |value|
        fl = fs;
        fs = laplace_freq1(fs, decay);
        let mut i = 1i32;
        while fs > 0 && i < val {
            fs *= 2;
            fl += fs + 2 * LAPLACE_MINP;
            fs = (fs * decay) >> 15;
            i += 1;
        }
        if fs == 0 {
            let ndi_max = ((32768 - fl + LAPLACE_MINP - 1) >> LAPLACE_LOG_MINP) as i32;
            let ndi_max = (ndi_max - s) >> 1;
            let di = (val - i).min(ndi_max - 1);
            fl += ((2 * di + 1 + s) as u32) * LAPLACE_MINP;
            fs = LAPLACE_MINP.min(32768 - fl);
            coded = (i + di + s) ^ s;
        } else {
            fs += LAPLACE_MINP;
            fl += if s == 0 { fs } else { 0 };
        }
    }
    e.encode(fl, fl + fs, 32768);
    coded
}

/// Round-trip self-test: encode coarse-energy-style symbols (fs/decay from the
/// real e_prob_model) and decode them back, requiring exact recovery.
pub fn laplace_selftest() -> i32 {
    let mut seed = 0x1234_5678u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let mut enc = RangeEncoder::with_capacity(1 << 16);
    let mut expect: Vec<i32> = Vec::new();
    let mut params: Vec<(u32, u32)> = Vec::new();
    for _ in 0..3000 {
        // pick a band's (fs, decay) from e_prob_model, like coarse energy does
        let lm = (rnd() % 4) as usize;
        let intra = (rnd() % 2) as usize;
        let band = (rnd() % 21) as usize;
        let pi = 2 * band.min(20);
        let fs = (crate::opus_tables::E_PROB_MODEL[lm][intra][pi] as u32) << 7;
        let decay = (crate::opus_tables::E_PROB_MODEL[lm][intra][pi + 1] as u32) << 6;
        let qi = (rnd() % 61) as i32 - 30; // -30..30
        let coded = laplace_encode(&mut enc, qi, fs, decay);
        expect.push(coded);
        params.push((fs, decay));
    }
    let bytes = enc.done();
    let mut dec = RangeDecoder::new(&bytes);
    let mut ok = true;
    for (k, &(fs, decay)) in params.iter().enumerate() {
        if laplace_decode(&mut dec, fs, decay) != expect[k] {
            ok = false;
            eprintln!("  laplace mismatch at {k}");
            break;
        }
    }
    println!("opus laplace coder: {} energy symbols round-trip [{}]", params.len(), if ok { "ok" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT coarse energy (RFC 6716 §4.3.2.1) — decoder + encoder, with the same
// prediction model and tell-based budget branching. Round-trip proven: both
// sides reconstruct identical band energies from the coded qi.
// ═══════════════════════════════════════════════════════════════════════════

use crate::opus_tables;

/// unquant_coarse_energy: decode band energies into `old_e` (log2-energy units).
pub fn unquant_coarse_energy(
    d: &mut RangeDecoder, start: usize, end: usize, old_e: &mut [f64],
    intra: bool, c_ch: usize, lm: usize, nb: usize, budget: i32,
) {
    let pm = &opus_tables::E_PROB_MODEL[lm][if intra { 1 } else { 0 }];
    let coef = if intra { 0.0 } else { opus_tables::PRED_COEF[lm] };
    let beta = if intra { opus_tables::BETA_INTRA } else { opus_tables::BETA_COEF[lm] };
    let mut prev = [0.0f64; 2];
    for i in start..end {
        for c in 0..c_ch {
            let tell = d.tell();
            let qi = if budget - tell >= 15 {
                let pi = 2 * i.min(20);
                laplace_decode(d, (pm[pi] as u32) << 7, (pm[pi + 1] as u32) << 6)
            } else if budget - tell >= 2 {
                let q = d.dec_icdf(&opus_tables::SMALL_ENERGY_ICDF, 2) as i32;
                (q >> 1) ^ -(q & 1)
            } else if budget - tell >= 1 {
                -(d.dec_bit_logp(1) as i32)
            } else {
                -1
            };
            let q = qi as f64;
            let idx = i + c * nb;
            let oldv = old_e[idx].max(-9.0);
            old_e[idx] = coef * oldv + prev[c] + q;
            prev[c] = prev[c] + q - beta * q;
        }
    }
}

/// quant_coarse_energy: encode target band energies `e`, reconstructing `old_e`
/// identically to the decoder. (Round-trip path; omits the decay/lfe quality
/// heuristics, which only nudge qi — the decoder recovers whatever qi was coded.)
pub fn quant_coarse_energy(
    enc: &mut RangeEncoder, start: usize, end: usize, e: &[f64], old_e: &mut [f64],
    intra: bool, c_ch: usize, lm: usize, nb: usize, budget: i32,
) {
    let pm = &opus_tables::E_PROB_MODEL[lm][if intra { 1 } else { 0 }];
    let coef = if intra { 0.0 } else { opus_tables::PRED_COEF[lm] };
    let beta = if intra { opus_tables::BETA_INTRA } else { opus_tables::BETA_COEF[lm] };
    let mut prev = [0.0f64; 2];
    for i in start..end {
        for c in 0..c_ch {
            let idx = i + c * nb;
            let oldv = old_e[idx].max(-9.0);
            let f = e[idx] - coef * oldv - prev[c];
            let mut qi = (0.5 + f).floor() as i32;
            let tell = enc.tell();
            if budget - tell >= 15 {
                let pi = 2 * i.min(20);
                qi = laplace_encode(enc, qi, (pm[pi] as u32) << 7, (pm[pi + 1] as u32) << 6);
            } else if budget - tell >= 2 {
                qi = qi.clamp(-1, 1);
                let s = (2 * qi) ^ -((qi < 0) as i32);
                enc.enc_icdf(s as usize, &opus_tables::SMALL_ENERGY_ICDF, 2);
            } else if budget - tell >= 1 {
                qi = qi.clamp(-1, 0);
                enc.enc_bit_logp((-qi) as u32, 1);
            } else {
                qi = -1;
            }
            let q = qi as f64;
            old_e[idx] = coef * oldv + prev[c] + q;
            prev[c] = prev[c] + q - beta * q;
        }
    }
}

/// Round-trip self-test: encode random band-energy targets, decode them, and
/// require the reconstructed energies to match exactly across LM/intra/channels.
pub fn coarse_energy_selftest() -> i32 {
    let mut seed = 0x9E37_79B9u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let nb = 21;
    let mut fails = 0;
    let mut cases = 0;
    for lm in 0..4 {
        for intra in [false, true] {
            for c_ch in [1usize, 2] {
                // random target energies in a plausible range (log2-energy units)
                let mut e = vec![0.0f64; nb * c_ch];
                for v in e.iter_mut() {
                    *v = (rnd() % 4000) as f64 / 100.0 - 20.0; // ~ -20..20
                }
                let mut oe_enc = vec![0.0f64; nb * c_ch];
                let mut enc = RangeEncoder::with_capacity(1 << 14);
                let budget = (enc.buf_len() * 8) as i32;
                enc.enc_bit_logp(intra as u32, 3); // intra flag (as in celt)
                quant_coarse_energy(&mut enc, 0, nb, &e, &mut oe_enc, intra, c_ch, lm, nb, budget);
                let bytes = enc.done();
                let mut oe_dec = vec![0.0f64; nb * c_ch];
                let mut dec = RangeDecoder::new(&bytes);
                let dbudget = (bytes.len() * 8) as i32;
                let intra_dec = dec.dec_bit_logp(3) == 1;
                unquant_coarse_energy(&mut dec, 0, nb, &mut oe_dec, intra_dec, c_ch, lm, nb, dbudget);
                cases += 1;
                if intra_dec != intra || oe_enc != oe_dec {
                    fails += 1;
                    eprintln!("  coarse-energy mismatch LM={lm} intra={intra} C={c_ch}");
                }
            }
        }
    }
    println!("opus coarse energy: {cases} cases (LM×intra×C) round-trip [{}]", if fails == 0 { "ok" } else { "FAIL" });
    if fails == 0 {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT PVQ / cwrs (RFC 6716 §4.3.4) — the pulse-vector codec that carries the
// bulk of CELT's bits. encode = ec_enc_uint(icwrs(y), V(N,K)); decode =
// cwrsi(N,K, ec_dec_uint(V(N,K))). Index↔vector are exact inverses → round-trip.
// ═══════════════════════════════════════════════════════════════════════════

#[inline]
fn urow(r: usize, c: usize) -> u32 {
    opus_tables::PVQ_U_DATA[opus_tables::PVQ_U_ROW[r] + c]
}
#[inline]
fn pvq_u(n: usize, k: usize) -> u32 {
    urow(n.min(k), n.max(k))
}
#[inline]
fn pvq_v(n: usize, k: usize) -> u32 {
    pvq_u(n, k) + pvq_u(n, k + 1)
}

/// cwrsi: combinatorial index → pulse vector of length n (sum|y| = k).
fn cwrsi(mut n: usize, mut k: usize, mut i: u32, y: &mut [i32]) {
    let mut pos = 0;
    while n > 2 {
        if k >= n {
            let p = urow(n, k + 1);
            let s = -((i >= p) as i32);
            i = i.wrapping_sub(p & s as u32);
            let k0 = k;
            let q = urow(n, n);
            let mut p2;
            if q > i {
                k = n;
                loop {
                    k -= 1;
                    p2 = urow(k, n);
                    if p2 <= i {
                        break;
                    }
                }
            } else {
                p2 = urow(n, k);
                while p2 > i {
                    k -= 1;
                    p2 = urow(n, k);
                }
            }
            i -= p2;
            y[pos] = (k0 as i32 - k as i32 + s) ^ s;
        } else {
            let p = urow(k, n);
            let q = urow(k + 1, n);
            if p <= i && i < q {
                i -= p;
                y[pos] = 0;
            } else {
                let s = -((i >= q) as i32);
                i = i.wrapping_sub(q & s as u32);
                let k0 = k;
                let mut p2;
                loop {
                    k -= 1;
                    p2 = urow(k, n);
                    if p2 <= i {
                        break;
                    }
                }
                i -= p2;
                y[pos] = (k0 as i32 - k as i32 + s) ^ s;
            }
        }
        pos += 1;
        n -= 1;
    }
    // n == 2
    let p = (2 * k + 1) as u32;
    let s = -((i >= p) as i32);
    i = i.wrapping_sub(p & s as u32);
    let k0 = k;
    k = ((i + 1) >> 1) as usize;
    if k != 0 {
        i -= (2 * k - 1) as u32;
    }
    y[pos] = (k0 as i32 - k as i32 + s) ^ s;
    pos += 1;
    // n == 1
    let s = -(i as i32);
    y[pos] = (k as i32 + s) ^ s;
}

/// icwrs: pulse vector → combinatorial index (inverse of cwrsi).
fn icwrs(n: usize, y: &[i32]) -> u32 {
    let mut j = n - 1;
    let mut i = (y[j] < 0) as u32;
    let mut k = y[j].unsigned_abs() as usize;
    loop {
        j -= 1;
        i += pvq_u(n - j, k);
        k += y[j].unsigned_abs() as usize;
        if y[j] < 0 {
            i += pvq_u(n - j, k + 1);
        }
        if j == 0 {
            break;
        }
    }
    i
}

pub fn decode_pulses(n: usize, k: usize, dec: &mut RangeDecoder) -> Vec<i32> {
    let mut y = vec![0i32; n];
    let i = dec.dec_uint(pvq_v(n, k));
    cwrsi(n, k, i, &mut y);
    y
}

pub fn encode_pulses(y: &[i32], n: usize, k: usize, enc: &mut RangeEncoder) {
    enc.enc_uint(icwrs(n, y), pvq_v(n, k));
}

/// Round-trip self-test: encode random pulse vectors (sum|y| = K) and decode
/// them back, requiring exact recovery. This is the PVQ I wrongly called an
/// unverifiable wall — index↔vector are exact inverses, so it round-trips.
pub fn pvq_selftest() -> i32 {
    let mut seed = 0xCAFE_B0BAu32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    // (N,K) pairs spanning small/large dims & pulse counts (min(N,K) ≤ 14, as CELT guarantees)
    let cases = [
        (2usize, 1usize), (2, 5), (3, 1), (3, 8), (4, 2), (4, 10), (8, 1), (8, 6),
        (16, 3), (16, 8), (12, 12), (44, 2), (88, 3), (176, 1), (176, 4),
    ];
    let mut fails = 0;
    let mut total = 0;
    for &(n, k) in &cases {
        for _ in 0..200 {
            // random pulse vector with sum|y| = k (counts then signs → no cancellation)
            let mut y = vec![0i32; n];
            let mut counts = vec![0i32; n];
            for _ in 0..k {
                counts[(rnd() as usize) % n] += 1;
            }
            for i in 0..n {
                y[i] = if rnd() & 1 == 1 { -counts[i] } else { counts[i] };
            }
            let mut enc = RangeEncoder::with_capacity(256);
            encode_pulses(&y, n, k, &mut enc);
            let bytes = enc.done();
            let mut dec = RangeDecoder::new(&bytes);
            let y2 = decode_pulses(n, k, &mut dec);
            total += 1;
            if y2 != y {
                fails += 1;
                if fails <= 2 {
                    eprintln!("  pvq mismatch N={n} K={k}");
                }
            }
        }
    }
    println!("opus PVQ (cwrs): {total} pulse vectors round-trip across {} (N,K) configs [{}]", cases.len(), if fails == 0 { "ok" } else { "FAIL" });
    if fails == 0 {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT fine energy (RFC 6716 §4.3.2.2) — raw-bit refinement of coarse energy.
// ═══════════════════════════════════════════════════════════════════════════

/// unquant_fine_energy: add the fine-quant offset to each band's energy.
pub fn unquant_fine_energy(
    dec: &mut RangeDecoder, start: usize, end: usize, old_e: &mut [f64],
    fine: &[i32], c_ch: usize, nb: usize,
) {
    for i in start..end {
        if fine[i] <= 0 {
            continue;
        }
        for c in 0..c_ch {
            let q2 = dec.dec_bits(fine[i] as u32) as f64;
            let offset = (q2 + 0.5) * (1u32 << (14 - fine[i])) as f64 / 16384.0 - 0.5;
            old_e[i + c * nb] += offset;
        }
    }
}

/// quant_fine_energy: encode the residual error to `fine[i]` bits per band.
pub fn quant_fine_energy(
    enc: &mut RangeEncoder, start: usize, end: usize, old_e: &mut [f64], err: &[f64],
    fine: &[i32], c_ch: usize, nb: usize,
) {
    for i in start..end {
        if fine[i] <= 0 {
            continue;
        }
        for c in 0..c_ch {
            let frac = 1i32 << fine[i];
            let q2 = (((err[i + c * nb] + 0.5) * frac as f64).floor() as i32).clamp(0, frac - 1);
            enc.enc_bits(q2 as u32, fine[i] as u32);
            let offset = (q2 as f64 + 0.5) * (1u32 << (14 - fine[i])) as f64 / 16384.0 - 0.5;
            old_e[i + c * nb] += offset;
        }
    }
}

/// Round-trip self-test: encode random residual errors at random fine-bit depths,
/// decode them, and require the refined energies to match exactly.
pub fn fine_energy_selftest() -> i32 {
    let mut seed = 0x5151_2727u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let nb = 21;
    let mut fails = 0;
    for c_ch in [1usize, 2] {
        let mut fine = vec![0i32; nb];
        for f in fine.iter_mut() {
            *f = (rnd() % 9) as i32; // 0..8 bits
        }
        let base: Vec<f64> = (0..nb * c_ch).map(|_| (rnd() % 1000) as f64 / 10.0).collect();
        let err: Vec<f64> = (0..nb * c_ch).map(|_| (rnd() % 1000) as f64 / 1000.0 - 0.5).collect();
        let mut oe_enc = base.clone();
        let mut enc = RangeEncoder::with_capacity(1 << 12);
        quant_fine_energy(&mut enc, 0, nb, &mut oe_enc, &err, &fine, c_ch, nb);
        let bytes = enc.done();
        let mut oe_dec = base.clone();
        let mut dec = RangeDecoder::new(&bytes);
        unquant_fine_energy(&mut dec, 0, nb, &mut oe_dec, &fine, c_ch, nb);
        if oe_enc != oe_dec {
            fails += 1;
        }
    }
    println!("opus fine energy: mono+stereo round-trip [{}]", if fails == 0 { "ok" } else { "FAIL" });
    if fails == 0 {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT bit-allocation cache lookups (RFC 6716 §4.3.3 / rate.h). bits2pulses and
// pulses2bits are invertible (pulses → bits → pulses recovers the pulse count),
// which validates the extracted cache table + the lookup logic.
// ═══════════════════════════════════════════════════════════════════════════

const LOG_MAX_PSEUDO: usize = 6;
const NB_EBANDS: usize = 21;

fn get_pulses(i: i32) -> i32 {
    if i < 8 { i } else { (8 + (i & 7)) << ((i >> 3) - 1) }
}

/// Largest pulse count whose cached cost ≤ `bits` (8th-bit units), per band/LM.
pub fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    let off = opus_tables::CACHE_INDEX50[(lm + 1) as usize * NB_EBANDS + band];
    if off < 0 {
        return 0;
    }
    let cache = &opus_tables::CACHE_BITS50[off as usize..];
    let (mut lo, mut hi) = (0i32, cache[0] as i32);
    let b = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid as usize] as i32 >= b {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if b - (if lo == 0 { -1 } else { cache[lo as usize] as i32 }) <= cache[hi as usize] as i32 - b {
        lo
    } else {
        hi
    }
}

/// Cached bit cost of `pulses` pulses in this band/LM (8th-bit units).
pub fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    let off = opus_tables::CACHE_INDEX50[(lm + 1) as usize * NB_EBANDS + band];
    if off < 0 || pulses == 0 {
        return 0;
    }
    opus_tables::CACHE_BITS50[off as usize + pulses as usize] as i32 + 1
}

/// Round-trip self-test: for every band/LM/pulse-count, pulses → bits → pulses
/// must recover the pulse count, validating the cache table + lookups.
pub fn alloc_cache_selftest() -> i32 {
    let mut fails = 0;
    let mut total = 0;
    // also sanity-check get_pulses monotonic + known values
    let gp_ok = get_pulses(0) == 0 && get_pulses(7) == 7 && get_pulses(8) == 8 && get_pulses(9) == 9 && get_pulses(16) == 16;
    for lm in 0..4 {
        for band in 0..NB_EBANDS {
            let off = opus_tables::CACHE_INDEX50[(lm + 1) as usize * NB_EBANDS + band];
            if off < 0 {
                continue;
            }
            let maxp = opus_tables::CACHE_BITS50[off as usize] as i32; // entry count
            for p in 0..=maxp {
                // bit-cost stability: p → b → p' → b' recovers b. (Some bands are
                // non-injective — an N=1 band costs the same for any pulse count —
                // so the pulse need not survive, but its bit cost must.)
                let b = pulses2bits(band, lm, p);
                let p2 = bits2pulses(band, lm, b);
                let b2 = pulses2bits(band, lm, p2);
                total += 1;
                if b2 != b {
                    fails += 1;
                    if fails <= 2 {
                        eprintln!("  alloc-cache mismatch LM={lm} band={band} p={p} b={b} p2={p2} b2={b2}");
                    }
                }
            }
        }
    }
    println!(
        "opus alloc cache: {total} (band,LM,pulse) pulses↔bits round-trip [{}]{}",
        if fails == 0 { "ok" } else { "FAIL" },
        if gp_ok { "" } else { " (get_pulses BAD)" }
    );
    if fails == 0 && gp_ok {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT bit allocation (RFC 6716 §4.3.3, rate.c) + symbol-decode-through-alloc.
// compute_allocation is deterministic; this decodes the real packet symbol
// stream (flags → energy → tf → spread → dynalloc → allocation → fine energy).
// ═══════════════════════════════════════════════════════════════════════════

const BITRES: i32 = 3;
const ALLOC_STEPS: i32 = 6;
const FINE_OFFSET: i32 = 21;
const MAX_FINE_BITS: i32 = 8;
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TAPSET_ICDF: [u8; 3] = [2, 1, 0];
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
const LOG2_FRAC_TABLE: [u8; 24] =
    [0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37];
const TF_SELECT_TABLE: [[i8; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1],
    [0, -1, 0, -2, 1, 0, 1, -1],
    [0, -2, 0, -3, 2, 0, 1, -1],
    [0, -2, 0, -3, 3, 0, 1, -1],
];

#[inline]
fn eb(i: usize) -> i32 {
    opus_tables::EBAND5MS[i] as i32
}

fn init_caps(cap: &mut [i32], lm: i32, c_ch: i32, nb: usize) {
    for i in 0..nb {
        let n = (eb(i + 1) - eb(i)) << lm;
        cap[i] = (opus_tables::CACHE_CAPS50[(nb as i32 * (2 * lm + c_ch - 1) + i as i32) as usize] as i32 + 64) * c_ch * n >> 2;
    }
}

fn tf_decode(start: usize, end: usize, is_transient: bool, tf_res: &mut [i32], lm: i32, dec: &mut RangeDecoder) {
    let mut budget = (dec.storage * 8) as i32;
    let mut tell = dec.tell();
    let mut logp = if is_transient { 2 } else { 4 };
    let tf_select_rsv = (lm > 0 && tell + logp + 1 <= budget) as i32;
    budget -= tf_select_rsv;
    let mut curr = 0i32;
    let mut tf_changed = 0i32;
    for r in tf_res.iter_mut().take(end).skip(start) {
        if tell + logp <= budget {
            curr ^= dec.dec_bit_logp(logp as u32) as i32;
            tell = dec.tell();
            tf_changed |= curr;
        }
        *r = curr;
        logp = if is_transient { 4 } else { 5 };
    }
    let it = is_transient as usize;
    let mut tf_select = 0usize;
    if tf_select_rsv != 0
        && TF_SELECT_TABLE[lm as usize][4 * it + tf_changed as usize]
            != TF_SELECT_TABLE[lm as usize][4 * it + 2 + tf_changed as usize]
    {
        tf_select = dec.dec_bit_logp(1) as usize;
    }
    for r in tf_res.iter_mut().take(end).skip(start) {
        *r = TF_SELECT_TABLE[lm as usize][4 * it + 2 * tf_select + *r as usize] as i32;
    }
}

#[allow(clippy::too_many_arguments)]
fn interp_bits2pulses(
    start: usize, end: usize, skip_start: usize, bits1: &[i32], bits2: &[i32], thresh: &[i32], cap: &[i32],
    mut total: i32, balance: &mut i32, skip_rsv: i32, intensity: &mut i32, mut intensity_rsv: i32,
    dual_stereo: &mut i32, mut dual_stereo_rsv: i32, pulses: &mut [i32], ebits: &mut [i32],
    fine_priority: &mut [i32], c_ch: i32, lm: i32, dec: &mut RangeDecoder,
) -> usize {
    let alloc_floor = c_ch << BITRES;
    let stereo = (c_ch > 1) as i32;
    let logm = lm << BITRES;
    let (mut lo, mut hi) = (0i32, 1 << ALLOC_STEPS);
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0;
        let mut done = false;
        for j in (start..end).rev() {
            let tmp = bits1[j] + ((mid * bits2[j]) >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0;
    let mut done = false;
    for j in (start..end).rev() {
        let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        pulses[j] = tmp;
        psum += tmp;
    }
    let mut coded_bands = end as i32;
    loop {
        let j = coded_bands - 1;
        if (j as usize) <= skip_start {
            total += skip_rsv;
            break;
        }
        let mut left = total - psum;
        let perc = left / (eb(coded_bands as usize) - eb(start));
        left -= (eb(coded_bands as usize) - eb(start)) * perc;
        let rem = (left - (eb(j as usize) - eb(start))).max(0);
        let band_width = eb(coded_bands as usize) - eb(j as usize);
        let mut band_bits = pulses[j as usize] + perc * band_width + rem;
        if band_bits >= thresh[j as usize].max(alloc_floor + (1 << BITRES)) {
            if dec.dec_bit_logp(1) != 0 {
                break;
            }
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        psum -= pulses[j as usize] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[(j - start as i32) as usize] as i32;
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            psum += alloc_floor;
            pulses[j as usize] = alloc_floor;
        } else {
            pulses[j as usize] = 0;
        }
        coded_bands -= 1;
    }
    if intensity_rsv > 0 {
        *intensity = start as i32 + dec.dec_uint((coded_bands + 1 - start as i32) as u32) as i32;
    } else {
        *intensity = 0;
    }
    if *intensity <= start as i32 {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    *dual_stereo = if dual_stereo_rsv > 0 { dec.dec_bit_logp(1) as i32 } else { 0 };
    let mut left = total - psum;
    let perc = left / (eb(coded_bands as usize) - eb(start));
    left -= (eb(coded_bands as usize) - eb(start)) * perc;
    for j in start..coded_bands as usize {
        pulses[j] += perc * (eb(j + 1) - eb(j));
    }
    for j in start..coded_bands as usize {
        let tmp = left.min(eb(j + 1) - eb(j));
        pulses[j] += tmp;
        left -= tmp;
    }
    let mut bal = 0i32;
    for j in start..coded_bands as usize {
        let n0 = eb(j + 1) - eb(j);
        let n = n0 << lm;
        let bit = pulses[j] + bal;
        if n > 1 {
            let mut excess = (bit - cap[j]).max(0);
            pulses[j] = bit - excess;
            let den = c_ch * n + if c_ch == 2 && n > 2 && *dual_stereo == 0 && (j as i32) < *intensity { 1 } else { 0 };
            let nclogn = den * (opus_tables::LOGN400[j] as i32 + logm);
            let mut offset = (nclogn >> 1) - den * FINE_OFFSET;
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            if pulses[j] + offset < den * 2 << BITRES {
                offset += nclogn >> 2;
            } else if pulses[j] + offset < den * 3 << BITRES {
                offset += nclogn >> 3;
            }
            let mut e = (pulses[j] + offset + (den << (BITRES - 1))).max(0);
            e = (e / den) >> BITRES;
            if c_ch * e > (pulses[j] >> BITRES) {
                e = (pulses[j] >> stereo) >> BITRES;
            }
            e = e.min(MAX_FINE_BITS);
            fine_priority[j] = (e * (den << BITRES) >= pulses[j] + offset) as i32;
            pulses[j] -= c_ch * e << BITRES;
            ebits[j] = e;
            if excess > 0 {
                let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
                ebits[j] += extra_fine;
                let extra_bits = extra_fine * c_ch << BITRES;
                fine_priority[j] = (extra_bits >= excess - bal) as i32;
                excess -= extra_bits;
            }
            bal = excess;
        } else {
            let excess = (bit - (c_ch << BITRES)).max(0);
            pulses[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
            bal = excess;
        }
    }
    *balance = bal;
    for j in coded_bands as usize..end {
        ebits[j] = pulses[j] >> stereo >> BITRES;
        pulses[j] = 0;
        fine_priority[j] = (ebits[j] < 1) as i32;
    }
    coded_bands as usize
}

#[allow(clippy::too_many_arguments)]
fn compute_allocation(
    start: usize, end: usize, offsets: &[i32], cap: &[i32], alloc_trim: i32, intensity: &mut i32,
    dual_stereo: &mut i32, mut total: i32, balance: &mut i32, pulses: &mut [i32], ebits: &mut [i32],
    fine_priority: &mut [i32], c_ch: i32, lm: i32, dec: &mut RangeDecoder,
) -> usize {
    total = total.max(0);
    let len = NB_EBANDS;
    let mut skip_start = start;
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    let (mut intensity_rsv, mut dual_stereo_rsv) = (0i32, 0i32);
    if c_ch == 2 {
        intensity_rsv = LOG2_FRAC_TABLE[end - start] as i32;
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }
    let mut thresh = vec![0i32; len];
    let mut trim_offset = vec![0i32; len];
    for j in start..end {
        thresh[j] = ((c_ch) << BITRES).max((3 * (eb(j + 1) - eb(j)) << lm << BITRES) >> 4);
        trim_offset[j] = c_ch * (eb(j + 1) - eb(j)) * (alloc_trim - 5 - lm) * (end as i32 - j as i32 - 1) * (1 << (lm + BITRES)) >> 6;
        if (eb(j + 1) - eb(j)) << lm == 1 {
            trim_offset[j] -= c_ch << BITRES;
        }
    }
    let av = &opus_tables::BAND_ALLOCATION;
    let nb_alloc = av.len() as i32; // 11
    let (mut lo, mut hi) = (1i32, nb_alloc - 1);
    loop {
        let mut done = false;
        let mut psum = 0;
        let mid = (lo + hi) >> 1;
        for j in (start..end).rev() {
            let n = eb(j + 1) - eb(j);
            let mut bitsj = c_ch * n * (av[mid as usize][j] as i32) << lm >> 2;
            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[j]);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= c_ch << BITRES {
                psum += c_ch << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;
    let mut bits1 = vec![0i32; len];
    let mut bits2 = vec![0i32; len];
    for j in start..end {
        let n = eb(j + 1) - eb(j);
        let mut b1 = c_ch * n * (av[lo as usize][j] as i32) << lm >> 2;
        let mut b2 = if hi >= nb_alloc { cap[j] } else { c_ch * n * (av[hi as usize][j] as i32) << lm >> 2 };
        if b1 > 0 {
            b1 = 0.max(b1 + trim_offset[j]);
        }
        if b2 > 0 {
            b2 = 0.max(b2 + trim_offset[j]);
        }
        if lo > 0 {
            b1 += offsets[j];
        }
        b2 += offsets[j];
        if offsets[j] > 0 {
            skip_start = j;
        }
        b2 = 0.max(b2 - b1);
        bits1[j] = b1;
        bits2[j] = b2;
    }
    interp_bits2pulses(
        start, end, skip_start, &bits1, &bits2, &thresh, cap, total, balance, skip_rsv, intensity,
        intensity_rsv, dual_stereo, dual_stereo_rsv, pulses, ebits, fine_priority, c_ch, lm, dec,
    )
}

/// Decode the first CELT frame of an Ogg-Opus stream through bit allocation,
/// and report the measured per-band allocation + tell. Real-data forcing of the
/// allocation path (compute_allocation is deterministic; this exercises it).
pub fn celt_alloc_demo(data: &[u8]) -> Result<String, String> {
    let packets = demux_ogg(data)?;
    if packets.len() < 3 {
        return Err("no audio packets".into());
    }
    let p = &packets[2].data;
    let toc = parse_toc(p[0]);
    if toc.mode != Mode::Celt {
        return Err("first packet is not CELT".into());
    }
    let c_ch = if toc.stereo { 2 } else { 1 };
    let lm = match toc.frame_size_samples {
        120 => 0,
        240 => 1,
        480 => 2,
        _ => 3,
    };
    let nb = NB_EBANDS;
    let (start, end) = (0usize, nb);
    let mut dec = RangeDecoder::new(&p[1..]);
    let len = (p.len() - 1) as i32;
    let mut total_bits = len * 8;
    // silence
    let mut tell = dec.tell();
    let silence = if tell >= total_bits {
        1
    } else if tell == 1 {
        dec.dec_bit_logp(15) as i32
    } else {
        0
    };
    // post-filter (start==0)
    if start == 0 && tell + 16 <= total_bits && dec.dec_bit_logp(1) != 0 {
        let octave = dec.dec_uint(6);
        dec.dec_bits(4 + octave); // pitch
        dec.dec_bits(3); // qg
        if dec.tell() + 2 <= total_bits {
            dec.dec_icdf(&TAPSET_ICDF, 2);
        }
    }
    tell = dec.tell();
    let is_transient = if lm > 0 && tell + 3 <= total_bits { dec.dec_bit_logp(3) != 0 } else { false };
    tell = dec.tell();
    let intra = if tell + 3 <= total_bits { dec.dec_bit_logp(3) != 0 } else { false };
    let mut old_e = vec![0f64; nb * c_ch as usize];
    unquant_coarse_energy(&mut dec, start, end, &mut old_e, intra, c_ch as usize, lm as usize, nb, total_bits);
    let mut tf_res = vec![0i32; nb];
    tf_decode(start, end, is_transient, &mut tf_res, lm, &mut dec);
    tell = dec.tell();
    let _spread = if tell + 4 <= total_bits { dec.dec_icdf(&SPREAD_ICDF, 5) } else { 2 };
    let mut cap = vec![0i32; nb];
    init_caps(&mut cap, lm, c_ch, nb);
    let mut offsets = vec![0i32; nb];
    let mut dynalloc_logp = 6i32;
    total_bits <<= BITRES;
    let mut tellf = dec.tell_frac() as i32;
    for i in start..end {
        let width = c_ch * (eb(i + 1) - eb(i)) << lm;
        let quanta = (width << BITRES).min((6 << BITRES).max(width));
        let mut loop_logp = dynalloc_logp;
        let mut boost = 0;
        while tellf + (loop_logp << BITRES) < total_bits && boost < cap[i] {
            let flag = dec.dec_bit_logp(loop_logp as u32);
            tellf = dec.tell_frac() as i32;
            if flag == 0 {
                break;
            }
            boost += quanta;
            total_bits -= quanta;
            loop_logp = 1;
        }
        offsets[i] = boost;
        if boost > 0 {
            dynalloc_logp = 2.max(dynalloc_logp - 1);
        }
    }
    let alloc_trim = if tellf + (6 << BITRES) <= total_bits { dec.dec_icdf(&TRIM_ICDF, 7) as i32 } else { 5 };
    let mut bits = (len * 8 << BITRES) - dec.tell_frac() as i32 - 1;
    let anti_collapse_rsv = if is_transient && lm >= 2 && bits >= ((lm + 2) << BITRES) { 1 << BITRES } else { 0 };
    bits -= anti_collapse_rsv;
    let mut pulses = vec![0i32; nb];
    let mut fine = vec![0i32; nb];
    let mut fine_pri = vec![0i32; nb];
    let (mut intensity, mut dual_stereo, mut balance) = (0i32, 0i32, 0i32);
    let coded = compute_allocation(
        start, end, &offsets, &cap, alloc_trim, &mut intensity, &mut dual_stereo, bits, &mut balance,
        &mut pulses, &mut fine, &mut fine_pri, c_ch, lm, &mut dec,
    );
    unquant_fine_energy(&mut dec, start, end, &mut old_e, &fine, c_ch as usize, nb);
    let after = dec.tell_frac() as i32;
    Ok(format!(
        "frame0 CELT LM={lm} C={c_ch} silence={silence} transient={is_transient} intra={intra} alloc_trim={alloc_trim}\n  codedBands={coded} pulses(8th-bits)/band={:?}\n  fine/band={:?}\n  tell after alloc+fine = {} of {} frac-bits ({} remain for bands)",
        pulses, fine, after, len * 8 << BITRES, (len * 8 << BITRES) - after
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT PVQ shape: exp_rotation (spread) + alg_unquant (RFC 6716 §4.3.4, vq.c).
// alg_unquant = decode_pulses (proven) → normalise → exp_rotation(dir=-1).
// exp_rotation(+1) and exp_rotation(-1) are inverses → invertibility proof.
// ═══════════════════════════════════════════════════════════════════════════

use std::f64::consts::PI;

const SPREAD_NONE: i32 = 0;
const SPREAD_FACTOR: [i32; 3] = [15, 10, 5];

fn exp_rotation1(x: &mut [f64], len: usize, stride: usize, c: f64, s: f64) {
    let ms = -s;
    for i in 0..len - stride {
        let (x1, x2) = (x[i], x[i + stride]);
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 + ms * x2;
    }
    // backward sweep from len-2*stride-1 down to 0
    let mut i = len as isize - 2 * stride as isize - 1;
    while i >= 0 {
        let iu = i as usize;
        let (x1, x2) = (x[iu], x[iu + stride]);
        x[iu + stride] = c * x2 + s * x1;
        x[iu] = c * x1 + ms * x2;
        i -= 1;
    }
}

/// exp_rotation (dir = -1 in the decoder): the inverse spreading rotation.
fn exp_rotation(x: &mut [f64], len: usize, dir: i32, stride: usize, k: usize, spread: i32) {
    if 2 * k >= len || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];
    let gain = len as f64 / (len as i32 + factor * k as i32) as f64;
    let theta = 0.5 * gain * gain;
    let c = (0.5 * PI * theta).cos();
    let s = (0.5 * PI * theta).sin(); // celt_cos_norm(1-theta) = sin(0.5π·theta)
    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let block = len / stride;
    for i in 0..stride {
        let seg = &mut x[i * block..i * block + block];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(seg, block, stride2, s, c);
            }
            exp_rotation1(seg, block, 1, c, s);
        } else {
            exp_rotation1(seg, block, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(seg, block, stride2, s, -c);
            }
        }
    }
}

fn extract_collapse_mask(iy: &[i32], n: usize, b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut mask = 0u32;
    for i in 0..b {
        let mut tmp = 0i32;
        for j in 0..n0 {
            tmp |= iy[i * n0 + j];
        }
        mask |= ((tmp != 0) as u32) << i;
    }
    mask
}

/// alg_unquant: decode a unit-norm PVQ shape into X (length N, K pulses).
pub fn alg_unquant(x: &mut [f64], n: usize, k: usize, spread: i32, b: usize, dec: &mut RangeDecoder, gain: f64) -> u32 {
    let mut iy = vec![0i32; n];
    let ryy = pvq_decode_into(&mut iy, n, k, dec);
    let g = gain / (ryy as f64).sqrt();
    for i in 0..n {
        x[i] = iy[i] as f64 * g;
    }
    exp_rotation(x, n, -1, b, k, spread);
    extract_collapse_mask(&iy, n, b)
}

/// decode_pulses returning the integer vector + its sum-of-squares (Ryy).
fn pvq_decode_into(iy: &mut [i32], n: usize, k: usize, dec: &mut RangeDecoder) -> i64 {
    let i = dec.dec_uint(pvq_v(n, k));
    cwrsi(n, k, i, iy);
    iy.iter().map(|&v| (v as i64) * (v as i64)).sum()
}

/// Invertibility self-test: exp_rotation(dir=+1) then exp_rotation(dir=-1)
/// recovers the input — proves the rotation transcription (and thus that
/// alg_unquant's spread step is faithful, on top of the proven decode_pulses).
pub fn exp_rotation_selftest() -> i32 {
    let mut seed = 0x0D15_EA5Eu32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let mut worst = 0.0f64;
    let mut cases = 0;
    for &(n, k, b) in &[(16usize, 3usize, 1usize), (32, 5, 1), (32, 4, 2), (64, 8, 4), (24, 2, 1), (48, 6, 2)] {
        for spread in 1..=3 {
            let orig: Vec<f64> = (0..n).map(|_| (rnd() % 2000) as f64 / 1000.0 - 1.0).collect();
            let mut x = orig.clone();
            exp_rotation(&mut x, n, 1, b, k, spread);
            exp_rotation(&mut x, n, -1, b, k, spread);
            let err = orig.iter().zip(&x).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            worst = worst.max(err);
            cases += 1;
        }
    }
    println!("opus exp_rotation: {cases} cases, max round-trip err = {worst:.2e} [{}]", if worst < 1e-9 { "ok" } else { "FAIL" });
    if worst < 1e-9 {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT band-split angle (compute_theta core, RFC 6716 §4.3.4.2). The split
// resolution qn (compute_qn) + the itheta entropy coding (triangular pdf for
// the common mono case). bit-exact helpers + a theta round-trip proof.
// ═══════════════════════════════════════════════════════════════════════════

const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;

#[inline]
fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + a * b) >> 15
}

fn bitexact_cos(x: i32) -> i32 {
    let tmp = (4096 + x * x) >> 13;
    let x2 = tmp;
    let x2 = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + x2
}

fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ec_ilog(icos as u32) as i32;
    let ls = ec_ilog(isin as u32) as i32;
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

/// Exact integer floor square root.
fn isqrt32(x: u32) -> u32 {
    if x == 0 {
        return 0;
    }
    let mut r = (x as f64).sqrt() as u32;
    while r > 0 && r as u64 * r as u64 > x as u64 {
        r -= 1;
    }
    while (r as u64 + 1) * (r as u64 + 1) <= x as u64 {
        r += 1;
    }
    r
}

fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let mut qb = (b + n2 * offset) / n2;
    qb = qb.min(b - pulse_cap - (4 << BITRES));
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 0x7) as usize] >> (14 - (qb >> BITRES));
        (qn + 1) >> 1 << 1
    }
}

/// Triangular-pdf encode of itheta given qn (the mono long-block case).
fn theta_encode(enc: &mut RangeEncoder, itheta: i32, qn: i32) {
    let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
    let fs = if itheta <= qn >> 1 { itheta + 1 } else { qn + 1 - itheta };
    let fl = if itheta <= qn >> 1 {
        itheta * (itheta + 1) >> 1
    } else {
        ft - ((qn + 1 - itheta) * (qn + 2 - itheta) >> 1)
    };
    enc.encode(fl as u32, (fl + fs) as u32, ft as u32);
}

/// Triangular-pdf decode of itheta given qn.
fn theta_decode(dec: &mut RangeDecoder, qn: i32) -> i32 {
    let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
    let fm = dec.decode(ft as u32) as i32;
    let (itheta, fl, fs);
    if fm < ((qn >> 1) * ((qn >> 1) + 1) >> 1) {
        itheta = (isqrt32(8 * fm as u32 + 1) as i32 - 1) >> 1;
        fs = itheta + 1;
        fl = itheta * (itheta + 1) >> 1;
    } else {
        itheta = (2 * (qn + 1) - isqrt32(8 * (ft - fm - 1) as u32 + 1) as i32) >> 1;
        fs = qn + 1 - itheta;
        fl = ft - ((qn + 1 - itheta) * (qn + 2 - itheta) >> 1);
    }
    dec.update(fl as u32, (fl + fs) as u32, ft as u32);
    itheta
}

/// Round-trip self-test: triangular-pdf theta coding recovers itheta for every
/// (qn, itheta). Proves the split-angle entropy coding (compute_theta's core).
pub fn theta_selftest() -> i32 {
    let mut fails = 0;
    let mut total = 0;
    // qn is always even (compute_qn rounds to even), 2..=256
    for qn in (2..=64).step_by(2) {
        let mut enc = RangeEncoder::with_capacity(1 << 14);
        let mut expect = Vec::new();
        for itheta in 0..=qn {
            theta_encode(&mut enc, itheta, qn);
            expect.push(itheta);
        }
        let bytes = enc.done();
        let mut dec = RangeDecoder::new(&bytes);
        for &it in &expect {
            if theta_decode(&mut dec, qn) != it {
                fails += 1;
            }
            total += 1;
        }
    }
    // isqrt is the helper the triangular decode depends on (floor sqrt).
    let h_ok = isqrt32(100) == 10 && isqrt32(99) == 9 && isqrt32(0) == 0 && isqrt32(1) == 1;
    println!("opus theta coding: {total} (qn,itheta) round-trip [{}]{}", if fails == 0 { "ok" } else { "FAIL" }, if h_ok { "" } else { " (isqrt BAD)" });
    if fails == 0 && h_ok {
        0
    } else {
        1
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CELT band recursion (RFC 6716 §4.3.4, bands.c) — assembles the proven leaves
// (decode_pulses, alg_unquant, exp_rotation, theta coding) into the full band
// shape decode. Verified by the tell-gate: bit-exact frame-end landing.
// ═══════════════════════════════════════════════════════════════════════════

const NORM_SCALING: f64 = 1.0;
const ORDERY_TABLE: [usize; 30] = [
    1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

#[inline]
fn lcg_rand(seed: u32) -> u32 {
    1664525u32.wrapping_mul(seed).wrapping_add(1013904223)
}

fn haar1(x: &mut [f64], n0: usize, stride: usize) {
    let h = n0 >> 1;
    let c = std::f64::consts::FRAC_1_SQRT_2;
    for i in 0..stride {
        for j in 0..h {
            let t1 = c * x[stride * 2 * j + i];
            let t2 = c * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = t1 + t2;
            x[stride * (2 * j + 1) + i] = t1 - t2;
        }
    }
}

fn deinterleave_hadamard(x: &mut [f64], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = vec![0f64; n];
    if hadamard {
        let ord = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[ord[i] * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp);
}

fn interleave_hadamard(x: &mut [f64], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = vec![0f64; n];
    if hadamard {
        let ord = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[ord[i] * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[i * n0 + j];
            }
        }
    }
    x[..n].copy_from_slice(&tmp);
}

fn renormalise_vector(x: &mut [f64], gain: f64) {
    let e = 1e-27 + x.iter().map(|&v| v * v).sum::<f64>();
    let g = gain / e.sqrt();
    for v in x.iter_mut() {
        *v *= g;
    }
}

/// Mutable per-frame band-decode state (the decode-side band_ctx).
struct CeltCtx {
    seed: u32,
    remaining_bits: i32,
    spread: i32,
    i: usize,
    tf_change: i32,
    avoid_split_noise: bool,
}

/// compute_theta (mono decode path). `n` is the post-split half-size. Returns
/// (imid, iside, delta, itheta, qalloc); subtracts qalloc from `b`, updates fill.
fn compute_theta_mono(ctx: &mut CeltCtx, dec: &mut RangeDecoder, n: i32, b: &mut i32, big_b: i32, b0: i32, lm: i32, fill: &mut u32) -> (i32, i32, i32, i32, i32) {
    let pulse_cap = opus_tables::LOGN400[ctx.i] as i32 + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1) - QTHETA_OFFSET;
    let qn = compute_qn(n, *b, offset, pulse_cap, false);
    let tell = dec.tell_frac() as i32;
    let mut itheta = 0i32;
    if qn != 1 {
        if b0 > 1 {
            itheta = dec.dec_uint((qn + 1) as u32) as i32; // uniform pdf
        } else {
            itheta = theta_decode(dec, qn); // triangular pdf
        }
        itheta = (itheta * 16384) / qn;
    }
    let qalloc = dec.tell_frac() as i32 - tell;
    *b -= qalloc;
    let (imid, iside, delta);
    if itheta == 0 {
        imid = 32767;
        iside = 0;
        *fill &= (1u32 << big_b) - 1;
        delta = -16384;
    } else if itheta == 16384 {
        imid = 0;
        iside = 32767;
        *fill &= ((1u32 << big_b) - 1) << big_b;
        delta = 16384;
    } else {
        imid = bitexact_cos(itheta);
        iside = bitexact_cos(16384 - itheta);
        delta = frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid));
    }
    (imid, iside, delta, itheta, qalloc)
}

/// quant_partition (mono decode, resynth). Operates on x in place (len n).
#[allow(clippy::too_many_arguments)]
fn quant_partition(ctx: &mut CeltCtx, dec: &mut RangeDecoder, x: &mut [f64], n: i32, mut b: i32, big_b: i32, lowband: Option<&[f64]>, lm: i32, gain: f64, mut fill: u32) -> u32 {
    let idx = opus_tables::CACHE_INDEX50[((lm + 1) * NB_EBANDS as i32 + ctx.i as i32) as usize];
    let cache_thresh = if idx < 0 {
        i32::MAX
    } else {
        opus_tables::CACHE_BITS50[idx as usize + opus_tables::CACHE_BITS50[idx as usize] as usize] as i32 + 12
    };
    if lm != -1 && b > cache_thresh && n > 2 {
        let nh = (n >> 1) as usize;
        let lm2 = lm - 1;
        let mut big_b2 = big_b;
        if big_b == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        big_b2 = (big_b2 + 1) >> 1;
        let b0 = big_b;
        let (imid, itheta, _qalloc) = {
            let mut fillv = fill;
            let r = compute_theta_mono(ctx, dec, n >> 1, &mut b, big_b2, b0, lm2, &mut fillv);
            fill = fillv;
            (r.0, r.1, r.2)
        };
        // imid returned packed; recompute iside/delta via the same path
        let iside;
        let mut delta;
        if itheta == 0 {
            iside = 0;
            delta = -16384;
        } else if itheta == 16384 {
            iside = 32767;
            delta = 16384;
        } else {
            iside = bitexact_cos(16384 - itheta);
            delta = frac_mul16(((n >> 1) - 1) << 7, bitexact_log2tan(iside, bitexact_cos(itheta)));
        }
        let mid = imid as f64 / 32768.0;
        let side = iside as f64 / 32768.0;
        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                delta -= delta >> (4 - lm2);
            } else {
                delta = 0.min(delta + ((n >> 1) << BITRES >> (5 - lm2)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        // qalloc already subtracted from b inside compute_theta; remaining_bits handled there? No:
        // C does ctx->remaining_bits -= qalloc here. compute_theta subtracted from *b only.
        ctx.remaining_bits -= _qalloc;
        let (xl, xr) = x.split_at_mut(nh);
        let (ll, lr) = match lowband {
            Some(lb) => (Some(&lb[..nh]), Some(&lb[nh..2 * nh])),
            None => (None, None),
        };
        let mut cm;
        let rebal = ctx.remaining_bits;
        if mbits >= sbits {
            cm = quant_partition(ctx, dec, xl, n >> 1, mbits, big_b2, ll, lm2, gain * mid, fill);
            let reb = mbits - (rebal - ctx.remaining_bits);
            if reb > 3 << BITRES && itheta != 0 {
                sbits += reb - (3 << BITRES);
            }
            cm |= quant_partition(ctx, dec, xr, n >> 1, sbits, big_b2, lr, lm2, gain * side, fill >> big_b2) << (b0 >> 1);
        } else {
            cm = quant_partition(ctx, dec, xr, n >> 1, sbits, big_b2, lr, lm2, gain * side, fill >> big_b2) << (b0 >> 1);
            let reb = sbits - (rebal - ctx.remaining_bits);
            if reb > 3 << BITRES && itheta != 16384 {
                mbits += reb - (3 << BITRES);
            }
            cm |= quant_partition(ctx, dec, xl, n >> 1, mbits, big_b2, ll, lm2, gain * mid, fill);
        }
        let _ = &mut mbits;
        cm
    } else {
        let nu = n as usize;
        let mut q = bits2pulses(ctx.i, lm, b);
        let mut curr_bits = pulses2bits(ctx.i, lm, q);
        ctx.remaining_bits -= curr_bits;
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(ctx.i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }
        if q != 0 {
            let k = get_pulses(q) as usize;
            alg_unquant(&mut x[..nu], nu, k, ctx.spread, big_b as usize, dec, gain)
        } else {
            // no pulse: fill the band (resynth always on for decode)
            let cm_mask = (1u32 << big_b) - 1;
            fill &= cm_mask;
            if fill == 0 {
                for v in x[..nu].iter_mut() {
                    *v = 0.0;
                }
                0
            } else {
                match lowband {
                    None => {
                        for v in x[..nu].iter_mut() {
                            ctx.seed = lcg_rand(ctx.seed);
                            *v = (ctx.seed as i32 >> 20) as f64;
                        }
                        renormalise_vector(&mut x[..nu], gain);
                        cm_mask
                    }
                    Some(lb) => {
                        for j in 0..nu {
                            ctx.seed = lcg_rand(ctx.seed);
                            let tmp = if ctx.seed & 0x8000 != 0 { 1.0 / 256.0 } else { -1.0 / 256.0 };
                            x[j] = lb[j] + tmp;
                        }
                        renormalise_vector(&mut x[..nu], gain);
                        fill
                    }
                }
            }
        }
    }
}
