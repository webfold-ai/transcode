// ═══════════════════════════════════════════════════════════════════════════
// png.rs — sovereign PNG codec (decode + encode), std-only, zero dependencies
// ═══════════════════════════════════════════════════════════════════════════
// A from-scratch PNG implementation in the same spirit as flac.rs: real
// bitstream work, no external crates. Includes a complete DEFLATE inflate
// (stored / fixed-Huffman / dynamic-Huffman) and a DEFLATE deflate (greedy
// LZ77 with hash chains + fixed Huffman), the zlib wrapper (Adler-32), PNG
// chunk parsing/writing (CRC-32), and all five scanline filters.
//
// Scope: 8-bit channel depth, non-interlaced. Decodes color types
// 0 (grayscale), 2 (RGB), 3 (palette), 4 (gray+alpha), 6 (RGBA). Encodes
// 2 (RGB) and 6 (RGBA). Output is canonical 8-bit RGBA in `Image::pixels`.
//
// NOTE ON THE SANGUINE FRAMING: DEFLATE's LZ77 match-finding and Huffman
// coding are data-dependent and are NOT affine recurrences, so the
// "x = B/U closed form" / "loop collapses to O(1)" results in
// proof/Substrate/StationaryPhase/{AffineLoopCollapse,SpinorLoop}.lean give
// no leverage here — confirmed by reading those proofs in full. This is the
// same honest entropy-coding work flac.rs does. The one framework result with
// real bearing (affine prefix scan ≡ left-fold, compose_assoc) would only
// apply to a summed-area-table pass, not to compression.
// ═══════════════════════════════════════════════════════════════════════════

/// A decoded image: 8-bit RGBA, row-major, `width*height*4` bytes.
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA8, contiguous, length = width*height*4.
    pub pixels: Vec<u8>,
}

// ─────────────────────────────────────────────────────────────────────────
// CRC-32 (PNG chunk CRC, IEEE 802.3 polynomial 0xEDB88320, reflected)
// ─────────────────────────────────────────────────────────────────────────

fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

fn crc32(data: &[u8]) -> u32 {
    let t = crc32_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ─────────────────────────────────────────────────────────────────────────
// Adler-32 (zlib check value)
// ─────────────────────────────────────────────────────────────────────────

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    // Process in chunks to bound the modulo frequency.
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// ─────────────────────────────────────────────────────────────────────────
// DEFLATE — bit reader (LSB-first), Huffman, inflate
// ─────────────────────────────────────────────────────────────────────────

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            acc: 0,
            nbits: 0,
        }
    }
    /// Read `n` bits, LSB-first (DEFLATE convention). `n` ≤ 16 in practice.
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 {
            return Ok(0);
        }
        while self.nbits < n {
            if self.pos >= self.data.len() {
                return Err("DEFLATE: unexpected end of stream".into());
            }
            self.acc |= (self.data[self.pos] as u32) << self.nbits;
            self.pos += 1;
            self.nbits += 8;
        }
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        let v = self.acc & mask;
        self.acc >>= n;
        self.nbits -= n;
        Ok(v)
    }
    fn align_byte(&mut self) {
        let drop = self.nbits % 8;
        self.acc >>= drop;
        self.nbits -= drop;
    }
    fn read_bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        // Must be byte-aligned; drain whole bytes from acc first.
        for slot in out.iter_mut() {
            if self.nbits >= 8 {
                *slot = (self.acc & 0xFF) as u8;
                self.acc >>= 8;
                self.nbits -= 8;
            } else if self.pos < self.data.len() {
                *slot = self.data[self.pos];
                self.pos += 1;
            } else {
                return Err("DEFLATE: unexpected end in stored block".into());
            }
        }
        Ok(())
    }
}

/// Canonical Huffman decode table built from a list of code lengths.
/// Uses the simple per-symbol code assignment + linear-by-length decode.
struct Huffman {
    // counts[len] = number of codes of that length; symbols sorted by (len, sym).
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // offsets per length into the symbol array
        let mut offsets = [0u16; 16];
        let mut sum = 0u16;
        for len in 1..16 {
            offsets[len] = sum;
            sum += counts[len];
        }
        let mut symbols = vec![0u16; sum as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    /// Decode one symbol (RFC 1951 canonical-code walk).
    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("DEFLATE: invalid Huffman code".into())
    }
}

// Length code base values & extra bits for symbols 257..=285.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_lit_huffman() -> Huffman {
    let mut lengths = vec![0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = if i < 144 {
            8
        } else if i < 256 {
            9
        } else if i < 280 {
            7
        } else {
            8
        };
    }
    Huffman::from_lengths(&lengths)
}

fn fixed_dist_huffman() -> Huffman {
    Huffman::from_lengths(&[5u8; 30])
}

/// Inflate a raw DEFLATE stream (no zlib header) into `out`.
fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored block.
                br.align_byte();
                let mut hdr = [0u8; 4];
                br.read_bytes(&mut hdr)?;
                let len = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
                let nlen = u16::from_le_bytes([hdr[2], hdr[3]]);
                if (len as u16) != !nlen {
                    return Err("DEFLATE: stored block LEN/NLEN mismatch".into());
                }
                let start = out.len();
                out.resize(start + len, 0);
                br.read_bytes(&mut out[start..start + len])?;
            }
            1 | 2 => {
                let (lit, dist) = if btype == 1 {
                    (fixed_lit_huffman(), fixed_dist_huffman())
                } else {
                    read_dynamic_tables(&mut br)?
                };
                inflate_block(&mut br, &lit, &dist, &mut out)?;
            }
            _ => return Err("DEFLATE: reserved block type 3".into()),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

fn read_dynamic_tables(br: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[ORDER[i]] = br.bits(3)? as u8;
    }
    let cl_huff = Huffman::from_lengths(&cl_lengths);

    let mut lengths: Vec<u8> = Vec::with_capacity(hlit + hdist);
    while lengths.len() < hlit + hdist {
        let sym = cl_huff.decode(br)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last().ok_or("DEFLATE: repeat with no prev")?;
                let rep = br.bits(2)? + 3;
                for _ in 0..rep {
                    lengths.push(prev);
                }
            }
            17 => {
                let rep = br.bits(3)? + 3;
                for _ in 0..rep {
                    lengths.push(0);
                }
            }
            18 => {
                let rep = br.bits(7)? + 11;
                for _ in 0..rep {
                    lengths.push(0);
                }
            }
            _ => return Err("DEFLATE: bad code-length symbol".into()),
        }
    }
    if lengths.len() != hlit + hdist {
        return Err("DEFLATE: code-length overflow".into());
    }
    let lit = Huffman::from_lengths(&lengths[..hlit]);
    let dist = Huffman::from_lengths(&lengths[hlit..]);
    Ok((lit, dist))
}

fn inflate_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        if sym == 256 {
            return Ok(());
        } else if sym < 256 {
            out.push(sym as u8);
        } else {
            let li = (sym - 257) as usize;
            if li >= 29 {
                return Err("DEFLATE: bad length symbol".into());
            }
            let length = LEN_BASE[li] as usize + br.bits(LEN_EXTRA[li] as u32)? as usize;
            let dsym = dist.decode(br)? as usize;
            if dsym >= 30 {
                return Err("DEFLATE: bad distance symbol".into());
            }
            let distance = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym] as u32)? as usize;
            if distance > out.len() {
                return Err("DEFLATE: distance exceeds output".into());
            }
            let start = out.len() - distance;
            for i in 0..length {
                let b = out[start + i];
                out.push(b);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// DEFLATE — deflate (greedy LZ77 + fixed Huffman)
// ─────────────────────────────────────────────────────────────────────────

struct BitWriter {
    buf: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }
    /// Write `n` bits LSB-first.
    fn bits(&mut self, val: u32, n: u32) {
        self.acc |= (val & (if n == 32 { u32::MAX } else { (1 << n) - 1 })) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.buf.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }
    /// Write a Huffman code value MSB-first (codes are emitted high-bit-first
    /// even though the bit container is LSB-first).
    fn huff(&mut self, code: u32, len: u32) {
        // reverse the low `len` bits of `code`, then emit LSB-first.
        let mut rev = 0u32;
        for i in 0..len {
            rev |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.bits(rev, len);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.buf.push((self.acc & 0xFF) as u8);
        }
        self.buf
    }
}

// Fixed-Huffman literal/length code emission (RFC 1951 §3.2.6).
fn emit_fixed_litlen(bw: &mut BitWriter, sym: u16) {
    match sym {
        0..=143 => bw.huff(0x30 + sym as u32, 8),
        144..=255 => bw.huff(0x190 + (sym as u32 - 144), 9),
        256..=279 => bw.huff(sym as u32 - 256, 7),
        280..=287 => bw.huff(0xC0 + (sym as u32 - 280), 8),
        _ => unreachable!(),
    }
}

fn length_to_sym(len: usize) -> (u16, u8, u32) {
    // returns (litlen symbol 257.., extra_bits, extra_value)
    for i in (0..29).rev() {
        let base = LEN_BASE[i] as usize;
        if len >= base {
            return (257 + i as u16, LEN_EXTRA[i], (len - base) as u32);
        }
    }
    (257, 0, 0)
}

fn dist_to_sym(dist: usize) -> (u16, u8, u32) {
    for i in (0..30).rev() {
        let base = DIST_BASE[i] as usize;
        if dist >= base {
            return (i as u16, DIST_EXTRA[i], (dist - base) as u32);
        }
    }
    (0, 0, 0)
}

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW: usize = 32768;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

fn hash3(d: &[u8], i: usize) -> usize {
    let h = (d[i] as usize) << 10 ^ (d[i + 1] as usize) << 5 ^ (d[i + 2] as usize);
    h & (HASH_SIZE - 1)
}

/// Greedy LZ77 + single fixed-Huffman block. Produces valid DEFLATE that any
/// conformant decoder (zlib, browsers) accepts.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    // single final fixed-Huffman block: BFINAL=1, BTYPE=01
    bw.bits(1, 1);
    bw.bits(1, 2);

    let n = data.len();
    let mut head = vec![-1i32; HASH_SIZE];
    let mut prev = vec![-1i32; n.max(1)];

    let mut i = 0usize;
    while i < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + MIN_MATCH <= n {
            let h = hash3(data, i);
            let mut cand = head[h];
            let mut chain = 128; // bounded chain length for speed
            let max_len = (n - i).min(MAX_MATCH);
            while cand >= 0 && chain > 0 {
                let c = cand as usize;
                let dist = i - c;
                if dist > WINDOW {
                    break;
                }
                // count match length
                let mut l = 0usize;
                while l < max_len && data[c + l] == data[i + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_dist = dist;
                    if l >= max_len {
                        break;
                    }
                }
                cand = prev[c];
                chain -= 1;
            }
        }

        if best_len >= MIN_MATCH {
            // emit <length, distance>
            let (lsym, lextra, lval) = length_to_sym(best_len);
            emit_fixed_litlen(&mut bw, lsym);
            if lextra > 0 {
                bw.bits(lval, lextra as u32);
            }
            let (dsym, dextra, dval) = dist_to_sym(best_dist);
            bw.huff(dsym as u32, 5); // fixed distance codes are 5-bit
            if dextra > 0 {
                bw.bits(dval, dextra as u32);
            }
            // insert hashes for the matched span
            let end = i + best_len;
            while i < end {
                if i + MIN_MATCH <= n {
                    let h = hash3(data, i);
                    prev[i] = head[h];
                    head[h] = i as i32;
                }
                i += 1;
            }
        } else {
            emit_fixed_litlen(&mut bw, data[i] as u16);
            if i + MIN_MATCH <= n {
                let h = hash3(data, i);
                prev[i] = head[h];
                head[h] = i as i32;
            }
            i += 1;
        }
    }
    emit_fixed_litlen(&mut bw, 256); // end of block
    bw.finish()
}

/// zlib-wrap a DEFLATE stream (2-byte header + Adler-32 trailer).
fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x78); // CMF: CM=8, CINFO=7 (32K window)
    out.push(0x01); // FLG: no dict, check bits make (0x78<<8|0x01)=30721 % 31 == 0
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Un-zlib: strip 2-byte header + 4-byte Adler trailer, inflate, verify check.
fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 {
        return Err("zlib: stream too short".into());
    }
    let cmf = data[0];
    if cmf & 0x0F != 8 {
        return Err("zlib: not DEFLATE".into());
    }
    if (data[1] & 0x20) != 0 {
        return Err("zlib: preset dictionary not supported".into());
    }
    let body = &data[2..data.len() - 4];
    let out = inflate(body)?;
    let want = u32::from_be_bytes([
        data[data.len() - 4],
        data[data.len() - 3],
        data[data.len() - 2],
        data[data.len() - 1],
    ]);
    if adler32(&out) != want {
        return Err("zlib: Adler-32 mismatch".into());
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────
// PNG scanline filters (RFC 2083 §6)
// ─────────────────────────────────────────────────────────────────────────

#[inline]
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reverse a filter on one scanline in place. `bpp` = bytes per pixel.
fn unfilter(filter: u8, line: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), String> {
    let len = line.len();
    match filter {
        0 => {}
        1 => {
            for i in bpp..len {
                line[i] = line[i].wrapping_add(line[i - bpp]);
            }
        }
        2 => {
            for i in 0..len {
                line[i] = line[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..len {
                let a = if i >= bpp { line[i - bpp] as u32 } else { 0 };
                let b = prev[i] as u32;
                line[i] = line[i].wrapping_add(((a + b) / 2) as u8);
            }
        }
        4 => {
            for i in 0..len {
                let a = if i >= bpp { line[i - bpp] as i32 } else { 0 };
                let b = prev[i] as i32;
                let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                line[i] = line[i].wrapping_add(paeth(a, b, c) as u8);
            }
        }
        _ => return Err(format!("PNG: unknown filter type {filter}")),
    }
    Ok(())
}

/// Apply a single filter type to a raw scanline, writing the filtered output.
fn filter_line(filter: u8, raw: &[u8], prev: &[u8], bpp: usize, out: &mut Vec<u8>) {
    out.push(filter);
    let len = raw.len();
    match filter {
        0 => out.extend_from_slice(raw),
        1 => {
            for i in 0..len {
                let a = if i >= bpp { raw[i - bpp] } else { 0 };
                out.push(raw[i].wrapping_sub(a));
            }
        }
        2 => {
            for i in 0..len {
                out.push(raw[i].wrapping_sub(prev[i]));
            }
        }
        3 => {
            for i in 0..len {
                let a = if i >= bpp { raw[i - bpp] as u32 } else { 0 };
                let b = prev[i] as u32;
                out.push(raw[i].wrapping_sub(((a + b) / 2) as u8));
            }
        }
        4 => {
            for i in 0..len {
                let a = if i >= bpp { raw[i - bpp] as i32 } else { 0 };
                let b = prev[i] as i32;
                let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                out.push(raw[i].wrapping_sub(paeth(a, b, c) as u8));
            }
        }
        _ => unreachable!(),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PNG decode
// ─────────────────────────────────────────────────────────────────────────

const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Decode a PNG byte stream into 8-bit RGBA. Supports color types 0/2/3/4/6,
/// 8-bit depth, non-interlaced.
pub fn decode(data: &[u8]) -> Result<Image, String> {
    if data.len() < 8 || data[..8] != SIGNATURE {
        return Err("PNG: bad signature".into());
    }
    let mut pos = 8usize;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut idat: Vec<u8> = Vec::new();
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();

    while pos + 8 <= data.len() {
        let len = be32(&data[pos..]) as usize;
        let ctype = &data[pos + 4..pos + 8];
        let body_start = pos + 8;
        if body_start + len + 4 > data.len() {
            return Err("PNG: chunk overruns file".into());
        }
        let body = &data[body_start..body_start + len];
        // CRC is over type + body
        let want_crc = be32(&data[body_start + len..]);
        if crc32(&data[pos + 4..body_start + len]) != want_crc {
            return Err("PNG: chunk CRC mismatch".into());
        }
        match ctype {
            b"IHDR" => {
                if len != 13 {
                    return Err("PNG: bad IHDR length".into());
                }
                width = be32(&body[0..]);
                height = be32(&body[4..]);
                bit_depth = body[8];
                color_type = body[9];
                interlace = body[12];
            }
            b"PLTE" => {
                for c in body.chunks(3) {
                    if c.len() == 3 {
                        palette.push([c[0], c[1], c[2]]);
                    }
                }
            }
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {} // skip ancillary chunks
        }
        pos = body_start + len + 4;
    }

    if bit_depth != 8 {
        return Err(format!("PNG: only 8-bit depth supported (got {bit_depth})"));
    }
    if interlace != 0 {
        return Err("PNG: interlaced images not supported".into());
    }
    if width == 0 || height == 0 {
        return Err("PNG: zero dimension".into());
    }

    let channels = match color_type {
        0 => 1usize, // grayscale
        2 => 3,      // RGB
        3 => 1,      // palette index
        4 => 2,      // gray + alpha
        6 => 4,      // RGBA
        _ => return Err(format!("PNG: unsupported color type {color_type}")),
    };
    let bpp = channels; // 8-bit depth → channels == bytes/pixel
    let raw = zlib_decompress(&idat)?;

    let w = width as usize;
    let h = height as usize;
    let stride = w * bpp;
    let expected = (stride + 1) * h;
    if raw.len() < expected {
        return Err(format!(
            "PNG: decompressed size {} < expected {}",
            raw.len(),
            expected
        ));
    }

    // Unfilter into a tightly-packed buffer of `channels`-byte pixels.
    let mut unfiltered = vec![0u8; stride * h];
    let mut prev = vec![0u8; stride];
    for y in 0..h {
        let in_off = y * (stride + 1);
        let filter = raw[in_off];
        let line = &mut unfiltered[y * stride..(y + 1) * stride];
        line.copy_from_slice(&raw[in_off + 1..in_off + 1 + stride]);
        unfilter(filter, line, &prev, bpp)?;
        prev.copy_from_slice(line);
    }

    // Expand to RGBA8.
    let mut pixels = vec![0u8; w * h * 4];
    for p in 0..w * h {
        let src = &unfiltered[p * bpp..p * bpp + bpp];
        let dst = &mut pixels[p * 4..p * 4 + 4];
        match color_type {
            0 => {
                dst[0] = src[0];
                dst[1] = src[0];
                dst[2] = src[0];
                dst[3] = 255;
            }
            2 => {
                dst[0] = src[0];
                dst[1] = src[1];
                dst[2] = src[2];
                dst[3] = 255;
            }
            3 => {
                let idx = src[0] as usize;
                let rgb = *palette.get(idx).ok_or("PNG: palette index out of range")?;
                dst[0] = rgb[0];
                dst[1] = rgb[1];
                dst[2] = rgb[2];
                dst[3] = *trns.get(idx).unwrap_or(&255);
            }
            4 => {
                dst[0] = src[0];
                dst[1] = src[0];
                dst[2] = src[0];
                dst[3] = src[1];
            }
            6 => {
                dst.copy_from_slice(src);
            }
            _ => unreachable!(),
        }
    }

    Ok(Image {
        width,
        height,
        pixels,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// PNG encode
// ─────────────────────────────────────────────────────────────────────────

fn write_chunk(out: &mut Vec<u8>, ctype: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(ctype);
    out.extend_from_slice(body);
    let crc = crc32(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode an 8-bit image. `alpha=true` → color type 6 (RGBA), else type 2 (RGB).
/// `pixels` is RGBA8 (the canonical `Image` layout); the alpha channel is
/// dropped when `alpha=false`.
pub fn encode(width: u32, height: u32, pixels: &[u8], alpha: bool) -> Result<Vec<u8>, String> {
    let w = width as usize;
    let h = height as usize;
    if pixels.len() < w * h * 4 {
        return Err("PNG: pixel buffer too small for dimensions".into());
    }
    let (color_type, bpp) = if alpha { (6u8, 4usize) } else { (2u8, 3usize) };
    let stride = w * bpp;

    // Build raw (unfiltered) scanlines from RGBA input.
    let mut raw = vec![0u8; stride * h];
    for p in 0..w * h {
        let s = &pixels[p * 4..p * 4 + 4];
        let d = &mut raw[p * bpp..p * bpp + bpp];
        d[0] = s[0];
        d[1] = s[1];
        d[2] = s[2];
        if alpha {
            d[3] = s[3];
        }
    }

    // Filter each scanline, choosing the type that minimises the sum of
    // absolute signed residuals (the standard minimum-sum-of-absolute heuristic).
    let mut filtered: Vec<u8> = Vec::with_capacity((stride + 1) * h);
    let mut prev = vec![0u8; stride];
    for y in 0..h {
        let line = &raw[y * stride..(y + 1) * stride];
        let mut best: Option<(i64, Vec<u8>)> = None;
        for f in 0..5u8 {
            let mut candidate = Vec::with_capacity(stride + 1);
            filter_line(f, line, &prev, bpp, &mut candidate);
            let cost: i64 = candidate[1..]
                .iter()
                .map(|&b| (b as i8) as i64)
                .map(|v| v.abs())
                .sum();
            if best.as_ref().map(|(c, _)| cost < *c).unwrap_or(true) {
                best = Some((cost, candidate));
            }
        }
        filtered.extend_from_slice(&best.unwrap().1);
        prev.copy_from_slice(line);
    }

    let compressed = zlib_compress(&filtered);

    let mut out = Vec::new();
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(color_type);
    ihdr.push(0); // compression
    ihdr.push(0); // filter method
    ihdr.push(0); // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler_known() {
        // zlib adler32(b"abc") == 0x024d0127
        assert_eq!(adler32(b"abc"), 0x024d_0127);
    }

    #[test]
    fn crc_known() {
        // PNG IEND chunk CRC of type "IEND" with empty body == 0xAE426082
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn deflate_roundtrips_itself() {
        let inputs: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"hello hello hello hello world".to_vec(),
            (0..5000u32).map(|i| (i % 251) as u8).collect(),
            vec![0u8; 4096],
        ];
        for inp in inputs {
            let comp = deflate(&inp);
            let back = inflate(&comp).expect("inflate our own deflate");
            assert_eq!(back, inp, "self roundtrip failed for len {}", inp.len());
        }
    }

    #[test]
    fn zlib_roundtrips_itself() {
        let data: Vec<u8> = (0..20000u32)
            .map(|i| ((i * 7 + i / 13) % 256) as u8)
            .collect();
        let z = zlib_compress(&data);
        let back = zlib_decompress(&z).expect("zlib roundtrip");
        assert_eq!(back, data);
    }

    #[test]
    fn png_roundtrips_rgba() {
        let w = 17u32;
        let h = 11u32;
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                px[i] = (x * 13) as u8;
                px[i + 1] = (y * 7) as u8;
                px[i + 2] = (x ^ y) as u8;
                px[i + 3] = if (x + y) % 2 == 0 { 255 } else { 128 };
            }
        }
        let png = encode(w, h, &px, true).expect("encode");
        let img = decode(&png).expect("decode");
        assert_eq!(img.width, w);
        assert_eq!(img.height, h);
        assert_eq!(img.pixels, px);
    }

    #[test]
    fn png_roundtrips_rgb() {
        let w = 8u32;
        let h = 8u32;
        let mut px = vec![0u8; (w * h * 4) as usize];
        for p in 0..(w * h) as usize {
            px[p * 4] = (p * 3) as u8;
            px[p * 4 + 1] = (p * 5) as u8;
            px[p * 4 + 2] = (p * 11) as u8;
            px[p * 4 + 3] = 255;
        }
        let png = encode(w, h, &px, false).expect("encode rgb");
        let img = decode(&png).expect("decode rgb");
        // RGB encode drops alpha → decode fills 255; compare RGB only.
        for p in 0..(w * h) as usize {
            assert_eq!(&img.pixels[p * 4..p * 4 + 3], &px[p * 4..p * 4 + 3]);
            assert_eq!(img.pixels[p * 4 + 3], 255);
        }
    }
}
