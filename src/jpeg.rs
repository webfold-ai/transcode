// ═══════════════════════════════════════════════════════════════════════════
// jpeg.rs — sovereign baseline JPEG codec (decode + encode), std-only
// ═══════════════════════════════════════════════════════════════════════════
// Baseline sequential DCT, Huffman entropy coding (no arithmetic, no
// progressive), 8-bit. Decodes grayscale + YCbCr with arbitrary sampling
// factors (4:2:0 / 4:2:2 / 4:4:4), restart markers handled. Encodes 3-component
// YCbCr 4:4:4 with the standard Annex-K Huffman tables and IJG quality scaling.
// Returns/consumes canonical 8-bit RGBA (the same `png::Image` layout).
//
// THE DCT is the one genuine point of contact with the framework's real
// strength: an 8-point DCT is a linear transform and factors like the
// FFT-factored MDCT in aac.rs (commit bb375fb). This file uses a clean direct
// separable float DCT for correctness first; the AAN/FFT fast path is a
// drop-in optimisation later. Huffman RLE, by contrast, is data-dependent
// entropy work with no closed form — same honest territory as flac.rs/png.rs.
// ═══════════════════════════════════════════════════════════════════════════

use crate::png::Image;

// ─────────────────────────────────────────────────────────────────────────
// Zig-zag scan order (JPEG Annex A, Figure A.6)
// ─────────────────────────────────────────────────────────────────────────

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// Standard quantization tables (Annex K.1), in natural (row-major) order.
const STD_LUMA_Q: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const STD_CHROMA_Q: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

// Standard Huffman tables (Annex K.3). BITS[i] = number of codes of length i+1.
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

// ─────────────────────────────────────────────────────────────────────────
// DCT basis (precomputed 8×8 cosines)
// ─────────────────────────────────────────────────────────────────────────

fn dct_basis() -> [[f32; 8]; 8] {
    // basis[u][x] = c(u) * cos((2x+1)uπ/16), c(0)=1/√2, else 1
    let mut b = [[0.0f32; 8]; 8];
    let pi = std::f32::consts::PI;
    for u in 0..8 {
        let cu = if u == 0 { (0.5f32).sqrt() } else { 1.0 };
        for x in 0..8 {
            b[u][x] = 0.5 * cu * ((2 * x + 1) as f32 * u as f32 * pi / 16.0).cos();
        }
    }
    b
}

/// Forward DCT-II on an 8×8 block (separable). `block` is row-major spatial,
/// output is row-major frequency (u = row, v = col).
// Even/odd basis-symmetry fold: since b[u][7−x] = (−1)ᵘ·b[u][x], each 8-point
// transform splits into a 4-point even part on the pairwise sums and a 4-point
// odd part on the pairwise differences — exactly half the multiplies, and the
// result is algebraically identical to the direct O(N²) form (no approximation).
fn fdct8x8(block: &[f32; 64], basis: &[[f32; 8]; 8]) -> [f32; 64] {
    let mut tmp = [0.0f32; 64];
    // row pass: out[u] = Σ_x b[u][x]·in[x]
    for y in 0..8 {
        let r = &block[y * 8..y * 8 + 8];
        let s = [r[0] + r[7], r[1] + r[6], r[2] + r[5], r[3] + r[4]];
        let d = [r[0] - r[7], r[1] - r[6], r[2] - r[5], r[3] - r[4]];
        for u in (0..8).step_by(2) {
            tmp[y * 8 + u] =
                basis[u][0] * s[0] + basis[u][1] * s[1] + basis[u][2] * s[2] + basis[u][3] * s[3];
        }
        for u in (1..8).step_by(2) {
            tmp[y * 8 + u] =
                basis[u][0] * d[0] + basis[u][1] * d[1] + basis[u][2] * d[2] + basis[u][3] * d[3];
        }
    }
    // column pass: out[u][v] = Σ_y b[u][y]·tmp[y][v]
    let mut out = [0.0f32; 64];
    for v in 0..8 {
        let c = [tmp[v], tmp[8 + v], tmp[16 + v], tmp[24 + v], tmp[32 + v], tmp[40 + v], tmp[48 + v], tmp[56 + v]];
        let s = [c[0] + c[7], c[1] + c[6], c[2] + c[5], c[3] + c[4]];
        let d = [c[0] - c[7], c[1] - c[6], c[2] - c[5], c[3] - c[4]];
        for u in (0..8).step_by(2) {
            out[u * 8 + v] =
                basis[u][0] * s[0] + basis[u][1] * s[1] + basis[u][2] * s[2] + basis[u][3] * s[3];
        }
        for u in (1..8).step_by(2) {
            out[u * 8 + v] =
                basis[u][0] * d[0] + basis[u][1] * d[1] + basis[u][2] * d[2] + basis[u][3] * d[3];
        }
    }
    out
}

/// Inverse DCT-III on an 8×8 block (separable), same even/odd fold.
fn idct8x8(coef: &[f32; 64], basis: &[[f32; 8]; 8]) -> [f32; 64] {
    let mut tmp = [0.0f32; 64];
    // tmp[y][v] = Σ_u b[u][y]·coef[u][v]  (split by parity of u)
    for v in 0..8 {
        let c = [coef[v], coef[8 + v], coef[16 + v], coef[24 + v], coef[32 + v], coef[40 + v], coef[48 + v], coef[56 + v]];
        for y in 0..4 {
            let e = basis[0][y] * c[0] + basis[2][y] * c[2] + basis[4][y] * c[4] + basis[6][y] * c[6];
            let o = basis[1][y] * c[1] + basis[3][y] * c[3] + basis[5][y] * c[5] + basis[7][y] * c[7];
            tmp[y * 8 + v] = e + o;
            tmp[(7 - y) * 8 + v] = e - o;
        }
    }
    // out[y][x] = Σ_v b[v][x]·tmp[y][v]  (split by parity of v)
    let mut out = [0.0f32; 64];
    for y in 0..8 {
        let r = &tmp[y * 8..y * 8 + 8];
        for x in 0..4 {
            let e = basis[0][x] * r[0] + basis[2][x] * r[2] + basis[4][x] * r[4] + basis[6][x] * r[6];
            let o = basis[1][x] * r[1] + basis[3][x] * r[3] + basis[5][x] * r[5] + basis[7][x] * r[7];
            out[y * 8 + x] = e + o;
            out[y * 8 + (7 - x)] = e - o;
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────
// Huffman tables
// ─────────────────────────────────────────────────────────────────────────

/// Decode-side Huffman: maps (code,len) → symbol via min/max code per length,
/// plus an 8-bit lookahead table `fast[prefix] = (len<<8)|symbol` for codes of
/// length ≤ 8 (0 = miss → slow per-bit path for the rare longer codes).
struct HuffDec {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [i32; 17],
    vals: Vec<u8>,
    fast: [u16; 256],
}

impl HuffDec {
    fn build(bits: &[u8; 16], vals: &[u8]) -> HuffDec {
        // generate canonical code sizes then min/max codes (Annex C/F)
        let mut huffsize = Vec::new();
        for (l, &count) in bits.iter().enumerate() {
            for _ in 0..count {
                huffsize.push((l + 1) as u8);
            }
        }
        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0i32; 17];
        let mut fast = [0u16; 256];
        let mut code = 0i32;
        let mut k = 0usize;
        for l in 1..=16 {
            let cnt = bits[l - 1] as usize;
            if cnt > 0 {
                valptr[l] = k as i32;
                mincode[l] = code;
                // fill the 8-bit lookahead for codes of length ≤ 8
                if l <= 8 {
                    let shift = 8 - l;
                    for j in 0..cnt {
                        let c = (code + j as i32) as usize;
                        let sym = vals[k + j] as u16;
                        let entry = ((l as u16) << 8) | sym;
                        let base = c << shift;
                        for p in base..base + (1 << shift) {
                            fast[p] = entry;
                        }
                    }
                }
                code += cnt as i32;
                maxcode[l] = code - 1;
                k += cnt;
            } else {
                maxcode[l] = -1;
            }
            code <<= 1;
        }
        HuffDec { mincode, maxcode, valptr, vals: vals.to_vec(), fast }
    }
}

/// Encode-side Huffman: symbol → (code, length).
struct HuffEnc {
    codes: [u32; 256],
    sizes: [u8; 256],
}

impl HuffEnc {
    fn build(bits: &[u8; 16], vals: &[u8]) -> HuffEnc {
        let mut codes = [0u32; 256];
        let mut sizes = [0u8; 256];
        let mut code = 0u32;
        let mut k = 0usize;
        for l in 1..=16u8 {
            for _ in 0..bits[(l - 1) as usize] {
                let sym = vals[k] as usize;
                codes[sym] = code;
                sizes[sym] = l;
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        HuffEnc { codes, sizes }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Bit I/O (JPEG: MSB-first, 0xFF byte-stuffing)
// ─────────────────────────────────────────────────────────────────────────

struct JpegBitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    nbits: u32,
    /// set when a marker (0xFF followed by non-zero) was hit during fill
    hit_marker: bool,
    marker: u8,
}

impl<'a> JpegBitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        JpegBitReader { data, pos, acc: 0, nbits: 0, hit_marker: false, marker: 0 }
    }
    fn fill(&mut self) {
        // 64-bit accumulator, MSB-first (next bit at bit 63). Bulk-load 4
        // marker-free bytes at once; fall to byte-wise only near a 0xFF.
        while self.nbits <= 56 {
            if self.pos >= self.data.len() {
                self.nbits += 8; // pad with zero bits at EOF (acc low bits already 0)
                continue;
            }
            // fast bulk path: 4 bytes, none 0xFF, room for 32 bits
            if self.nbits <= 24
                && self.pos + 4 <= self.data.len()
                && self.data[self.pos] != 0xFF
                && self.data[self.pos + 1] != 0xFF
                && self.data[self.pos + 2] != 0xFF
                && self.data[self.pos + 3] != 0xFF
            {
                let chunk = (self.data[self.pos] as u64) << 24
                    | (self.data[self.pos + 1] as u64) << 16
                    | (self.data[self.pos + 2] as u64) << 8
                    | (self.data[self.pos + 3] as u64);
                self.acc |= chunk << (32 - self.nbits);
                self.pos += 4;
                self.nbits += 32;
                continue;
            }
            let b = self.data[self.pos];
            if b == 0xFF {
                let next = *self.data.get(self.pos + 1).unwrap_or(&0);
                if next == 0x00 {
                    self.pos += 2;
                    self.acc |= 0xFFu64 << (56 - self.nbits);
                    self.nbits += 8;
                } else {
                    // restart or other marker: stop feeding; caller resyncs
                    self.hit_marker = true;
                    self.marker = next;
                    self.nbits += 8;
                }
            } else {
                self.pos += 1;
                self.acc |= (b as u64) << (56 - self.nbits);
                self.nbits += 8;
            }
        }
    }
    fn get_bit(&mut self) -> u32 {
        if self.nbits == 0 {
            self.fill();
        }
        let b = (self.acc >> 63) as u32 & 1;
        self.acc <<= 1;
        self.nbits -= 1;
        b
    }
    fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.nbits < n {
            self.fill();
        }
        let v = (self.acc >> (64 - n)) as u32;
        self.acc <<= n;
        self.nbits -= n;
        v
    }
    fn decode_huff(&mut self, h: &HuffDec) -> Result<u8, String> {
        // fast path: 8-bit lookahead table covers all codes of length ≤ 8.
        if self.nbits < 8 {
            self.fill();
        }
        let entry = h.fast[(self.acc >> 56) as usize & 0xFF];
        if entry != 0 {
            let len = (entry >> 8) as u32;
            self.acc <<= len;
            self.nbits -= len;
            return Ok((entry & 0xFF) as u8);
        }
        // slow path: codes longer than 8 bits (rare), per-bit walk
        let mut code = 0i32;
        for l in 1..=16 {
            code = (code << 1) | self.get_bit() as i32;
            if h.maxcode[l] >= 0 && code <= h.maxcode[l] {
                let idx = (h.valptr[l] + (code - h.mincode[l])) as usize;
                return Ok(*h.vals.get(idx).ok_or("JPEG: huff index oob")?);
            }
        }
        Err("JPEG: bad Huffman code".into())
    }
    /// Discard partial bits and resync to the next byte boundary at a restart
    /// marker; advance pos past the RSTn marker.
    fn restart_resync(&mut self) {
        self.acc = 0;
        self.nbits = 0;
        self.hit_marker = false;
        // skip to the 0xFF Dx marker and past it
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF && (0xD0..=0xD7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }
}

/// Sign-extend an `s`-bit JPEG "receive" value (Annex F.2.2.1: EXTEND).
fn extend(v: u32, s: u32) -> i32 {
    if s == 0 {
        return 0;
    }
    let vt = 1i32 << (s - 1);
    if (v as i32) < vt {
        v as i32 + (-1i32 << s) + 1
    } else {
        v as i32
    }
}

struct JpegBitWriter {
    buf: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl JpegBitWriter {
    fn new() -> Self {
        JpegBitWriter { buf: Vec::new(), acc: 0, nbits: 0 }
    }
    fn put_bits(&mut self, code: u32, len: u32) {
        // place `code` just below the filled region of the 64-bit accumulator
        self.acc |= (code as u64) << (64 - self.nbits - len);
        self.nbits += len;
        // Accumulate across calls; drain only at ≥32 bits so the common
        // stuffing-free case emits 4 bytes in one shot. (codes ≤16 bits keep
        // nbits < 48, well within the u64.)
        while self.nbits >= 32 {
            let word = (self.acc >> 32) as u32;
            // SWAR: any byte of `word` == 0xFF? (a zero byte in `!word`)
            let v = !word;
            if v.wrapping_sub(0x0101_0101) & !v & 0x8080_8080 == 0 {
                self.buf.extend_from_slice(&word.to_be_bytes());
                self.acc <<= 32;
                self.nbits -= 32;
            } else {
                self.drain_byte();
            }
        }
    }
    #[inline]
    fn drain_byte(&mut self) {
        let b = (self.acc >> 56) as u8;
        self.buf.push(b);
        if b == 0xFF {
            self.buf.push(0x00); // byte stuffing
        }
        self.acc <<= 8;
        self.nbits -= 8;
    }
    fn flush(&mut self) {
        if self.nbits % 8 != 0 {
            let pad = 8 - self.nbits % 8;
            self.put_bits((1 << pad) - 1, pad); // pad with 1s (JPEG convention)
        }
        while self.nbits >= 8 {
            self.drain_byte();
        }
        self.acc = 0;
        self.nbits = 0;
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Color conversion
// ─────────────────────────────────────────────────────────────────────────

#[inline]
fn clamp_u8(v: f32) -> u8 {
    if v < 0.0 {
        0
    } else if v > 255.0 {
        255
    } else {
        (v + 0.5) as u8
    }
}

#[inline]
fn ycbcr_to_rgb(y: f32, cb: f32, cr: f32) -> (u8, u8, u8) {
    let r = y + 1.402 * (cr - 128.0);
    let g = y - 0.344136 * (cb - 128.0) - 0.714136 * (cr - 128.0);
    let b = y + 1.772 * (cb - 128.0);
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

#[inline]
fn rgb_to_ycbcr(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = -0.168736 * r - 0.331264 * g + 0.5 * b + 128.0;
    let cr = 0.5 * r - 0.418688 * g - 0.081312 * b + 128.0;
    (y, cb, cr)
}

// ─────────────────────────────────────────────────────────────────────────
// Decode
// ─────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Component {
    id: u8,
    h: u8,
    v: u8,
    tq: u8,   // quant table selector
    td: u8,   // DC huff selector
    ta: u8,   // AC huff selector
    // full-resolution plane (filled after upsampling); during scan, blocks
    // are written into a per-component grid sized to its own sampling.
    plane: Vec<u8>,
    bw: usize, // plane width in pixels (blocks_x*8)
    bh: usize,
}

pub fn decode(data: &[u8]) -> Result<Image, String> {
    let basis = dct_basis();
    let mut qtables: [[u16; 64]; 4] = [[0; 64]; 4];
    let mut dc_huff: Vec<Option<HuffDec>> = (0..4).map(|_| None).collect();
    let mut ac_huff: Vec<Option<HuffDec>> = (0..4).map(|_| None).collect();
    let mut comps: Vec<Component> = Vec::new();
    let mut width = 0usize;
    let mut height = 0usize;
    let mut restart_interval = 0usize;

    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err("JPEG: missing SOI".into());
    }
    let mut p = 2usize;
    loop {
        if p + 1 >= data.len() {
            return Err("JPEG: truncated before SOS".into());
        }
        if data[p] != 0xFF {
            p += 1;
            continue;
        }
        let marker = data[p + 1];
        p += 2;
        match marker {
            0xD9 => return Err("JPEG: EOI before scan".into()),
            0xC0 | 0xC1 => {
                // SOF0 baseline / SOF1 extended sequential (treat same)
                let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                let prec = data[p + 2];
                if prec != 8 {
                    return Err("JPEG: only 8-bit precision".into());
                }
                height = u16::from_be_bytes([data[p + 3], data[p + 4]]) as usize;
                width = u16::from_be_bytes([data[p + 5], data[p + 6]]) as usize;
                let nc = data[p + 7] as usize;
                comps.clear();
                for i in 0..nc {
                    let o = p + 8 + i * 3;
                    let id = data[o];
                    let hv = data[o + 1];
                    comps.push(Component {
                        id,
                        h: hv >> 4,
                        v: hv & 0x0F,
                        tq: data[o + 2],
                        ..Default::default()
                    });
                }
                p += len;
            }
            0xC2 => return Err("JPEG: progressive JPEG not supported".into()),
            0xC4 => {
                // DHT (may contain several tables)
                let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                let mut q = p + 2;
                let end = p + len;
                while q < end {
                    let tc_th = data[q];
                    let class = tc_th >> 4;
                    let id = (tc_th & 0x0F) as usize;
                    q += 1;
                    let mut bits = [0u8; 16];
                    let mut total = 0usize;
                    for b in &mut bits {
                        *b = data[q];
                        total += *b as usize;
                        q += 1;
                    }
                    let vals = data[q..q + total].to_vec();
                    q += total;
                    let h = HuffDec::build(&bits, &vals);
                    if class == 0 {
                        dc_huff[id] = Some(h);
                    } else {
                        ac_huff[id] = Some(h);
                    }
                }
                p = end;
            }
            0xDB => {
                // DQT
                let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                let mut q = p + 2;
                let end = p + len;
                while q < end {
                    let pq_tq = data[q];
                    let prec = pq_tq >> 4;
                    let id = (pq_tq & 0x0F) as usize;
                    q += 1;
                    let mut table = [0u16; 64];
                    for k in 0..64 {
                        if prec == 0 {
                            table[ZIGZAG[k]] = data[q] as u16;
                            q += 1;
                        } else {
                            table[ZIGZAG[k]] = u16::from_be_bytes([data[q], data[q + 1]]);
                            q += 2;
                        }
                    }
                    qtables[id] = table;
                }
                p = end;
            }
            0xDD => {
                // DRI
                restart_interval = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
                p += 4;
            }
            0xDA => {
                // SOS
                let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                let ns = data[p + 2] as usize;
                for i in 0..ns {
                    let cs = data[p + 3 + i * 2];
                    let tdta = data[p + 4 + i * 2];
                    if let Some(c) = comps.iter_mut().find(|c| c.id == cs) {
                        c.td = tdta >> 4;
                        c.ta = tdta & 0x0F;
                    }
                }
                p += len;
                // entropy-coded data begins at p
                return decode_scan(
                    data, p, width, height, &mut comps, &qtables, &dc_huff, &ac_huff,
                    restart_interval, &basis,
                );
            }
            0xD8 => {}                              // SOI (shouldn't recur)
            0x01 | 0xD0..=0xD7 => {}                // TEM / stray RST
            _ => {
                // skip any other marker segment with a length
                let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                p += len;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    start: usize,
    width: usize,
    height: usize,
    comps: &mut [Component],
    qtables: &[[u16; 64]; 4],
    dc_huff: &[Option<HuffDec>],
    ac_huff: &[Option<HuffDec>],
    restart_interval: usize,
    basis: &[[f32; 8]; 8],
) -> Result<Image, String> {
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1) as usize;
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1) as usize;
    let mcus_x = (width + 8 * hmax - 1) / (8 * hmax);
    let mcus_y = (height + 8 * vmax - 1) / (8 * vmax);

    // allocate per-component planes sized to blocks
    for c in comps.iter_mut() {
        c.bw = mcus_x * c.h as usize * 8;
        c.bh = mcus_y * c.v as usize * 8;
        c.plane = vec![0u8; c.bw * c.bh];
    }

    let mut br = JpegBitReader::new(data, start);
    let mut dc_pred = vec![0i32; comps.len()];
    let mut mcu_count = 0usize;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval != 0 && mcu_count != 0 && mcu_count % restart_interval == 0 {
                br.restart_resync();
                for d in dc_pred.iter_mut() {
                    *d = 0;
                }
            }
            for (ci, c) in comps.iter_mut().enumerate() {
                let dch = dc_huff[c.td as usize]
                    .as_ref()
                    .ok_or("JPEG: missing DC table")?;
                let ach = ac_huff[c.ta as usize]
                    .as_ref()
                    .ok_or("JPEG: missing AC table")?;
                let qt = &qtables[c.tq as usize];
                for by in 0..c.v as usize {
                    for bx in 0..c.h as usize {
                        let mut coef = [0.0f32; 64];
                        // DC
                        let t = br.decode_huff(dch)?;
                        let diff = extend(br.get_bits(t as u32), t as u32);
                        dc_pred[ci] += diff;
                        coef[0] = (dc_pred[ci] * qt[0] as i32) as f32;
                        // AC
                        let mut k = 1usize;
                        while k < 64 {
                            let rs = br.decode_huff(ach)?;
                            let r = (rs >> 4) as usize;
                            let s = (rs & 0x0F) as u32;
                            if s == 0 {
                                if r == 15 {
                                    k += 16;
                                    continue;
                                } else {
                                    break; // EOB
                                }
                            }
                            k += r;
                            if k >= 64 {
                                break;
                            }
                            let val = extend(br.get_bits(s), s);
                            let nat = ZIGZAG[k];
                            coef[nat] = (val * qt[nat] as i32) as f32;
                            k += 1;
                        }
                        let spatial = idct8x8(&coef, basis);
                        // write block into plane (level shift +128)
                        let px0 = (mx * c.h as usize + bx) * 8;
                        let py0 = (my * c.v as usize + by) * 8;
                        for yy in 0..8 {
                            for xx in 0..8 {
                                c.plane[(py0 + yy) * c.bw + (px0 + xx)] =
                                    clamp_u8(spatial[yy * 8 + xx] + 128.0);
                            }
                        }
                    }
                }
            }
            mcu_count += 1;
        }
    }

    // Assemble RGBA. Integer YCbCr→RGB via precomputed contribution tables
    // (no per-pixel float, no multiplies), and precomputed sample-index maps
    // per component (no per-pixel division for chroma upsampling).
    let mut pixels = vec![0u8; width * height * 4];
    let ncomp = comps.len();
    // chroma→RGB contribution tables, indexed by the chroma byte 0..=255
    let mut cr_r = [0i32; 256];
    let mut cb_b = [0i32; 256];
    let mut cr_g = [0i32; 256];
    let mut cb_g = [0i32; 256];
    for i in 0..256 {
        let d = i as f32 - 128.0;
        cr_r[i] = (1.402 * d).round() as i32;
        cb_b[i] = (1.772 * d).round() as i32;
        cr_g[i] = (0.714136 * d).round() as i32;
        cb_g[i] = (0.344136 * d).round() as i32;
    }
    #[inline]
    fn cl(v: i32) -> u8 {
        v.clamp(0, 255) as u8
    }
    if ncomp == 1 {
        let c = &comps[0];
        let sxm: Vec<usize> = (0..width).map(|x| (x * c.h as usize / hmax).min(c.bw - 1)).collect();
        let sym: Vec<usize> = (0..height).map(|y| (y * c.v as usize / vmax).min(c.bh - 1)).collect();
        for y in 0..height {
            let row = sym[y] * c.bw;
            for x in 0..width {
                let g = c.plane[row + sxm[x]];
                let di = (y * width + x) * 4;
                pixels[di] = g;
                pixels[di + 1] = g;
                pixels[di + 2] = g;
                pixels[di + 3] = 255;
            }
        }
    } else {
        let (cy, ccb, ccr) = (&comps[0], &comps[1], &comps[2]);
        let sxm = |c: &Component| -> Vec<usize> {
            (0..width).map(|x| (x * c.h as usize / hmax).min(c.bw - 1)).collect()
        };
        let sym = |c: &Component| -> Vec<usize> {
            (0..height).map(|y| (y * c.v as usize / vmax).min(c.bh - 1)).collect()
        };
        let (yxm, yym) = (sxm(cy), sym(cy));
        let (bxm, bym) = (sxm(ccb), sym(ccb));
        let (rxm, rym) = (sxm(ccr), sym(ccr));
        for y in 0..height {
            let (yr, br, rr) = (yym[y] * cy.bw, bym[y] * ccb.bw, rym[y] * ccr.bw);
            for x in 0..width {
                let yv = cy.plane[yr + yxm[x]] as i32;
                let cb = ccb.plane[br + bxm[x]] as usize;
                let cr = ccr.plane[rr + rxm[x]] as usize;
                let di = (y * width + x) * 4;
                pixels[di] = cl(yv + cr_r[cr]);
                pixels[di + 1] = cl(yv - cb_g[cb] - cr_g[cr]);
                pixels[di + 2] = cl(yv + cb_b[cb]);
                pixels[di + 3] = 255;
            }
        }
    }

    Ok(Image { width: width as u32, height: height as u32, pixels })
}

// ─────────────────────────────────────────────────────────────────────────
// Encode (3-component YCbCr 4:4:4, baseline, standard tables)
// ─────────────────────────────────────────────────────────────────────────

fn scale_qtable(base: &[u8; 64], quality: u32) -> [u16; 64] {
    let q = quality.clamp(1, 100);
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for i in 0..64 {
        let v = (base[i] as u32 * scale + 50) / 100;
        out[i] = v.clamp(1, 255) as u16;
    }
    out
}

fn put_marker(out: &mut Vec<u8>, m: u8) {
    out.push(0xFF);
    out.push(m);
}

fn put_seg(out: &mut Vec<u8>, m: u8, body: &[u8]) {
    put_marker(out, m);
    let len = (body.len() + 2) as u16;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
}

/// Encode RGBA8 pixels as a baseline JPEG (4:4:4) at the given quality (1-100).
pub fn encode(width: u32, height: u32, pixels: &[u8], quality: u32) -> Result<Vec<u8>, String> {
    let w = width as usize;
    let h = height as usize;
    if pixels.len() < w * h * 4 {
        return Err("JPEG: pixel buffer too small".into());
    }
    let basis = dct_basis();
    let lq = scale_qtable(&STD_LUMA_Q, quality);
    let cq = scale_qtable(&STD_CHROMA_Q, quality);

    let dc_l = HuffEnc::build(&DC_LUMA_BITS, &DC_LUMA_VALS);
    let ac_l = HuffEnc::build(&AC_LUMA_BITS, &AC_LUMA_VALS);
    let dc_c = HuffEnc::build(&DC_CHROMA_BITS, &DC_CHROMA_VALS);
    let ac_c = HuffEnc::build(&AC_CHROMA_BITS, &AC_CHROMA_VALS);

    // Build Y/Cb/Cr planes padded to multiple of 8 by edge replication.
    let bw = (w + 7) / 8 * 8;
    let bh = (h + 7) / 8 * 8;
    let mut yp = vec![0.0f32; bw * bh];
    let mut cbp = vec![0.0f32; bw * bh];
    let mut crp = vec![0.0f32; bw * bh];
    for y in 0..bh {
        for x in 0..bw {
            let sx = x.min(w - 1);
            let sy = y.min(h - 1);
            let i = (sy * w + sx) * 4;
            let (yy, cb, cr) =
                rgb_to_ycbcr(pixels[i] as f32, pixels[i + 1] as f32, pixels[i + 2] as f32);
            yp[y * bw + x] = yy;
            cbp[y * bw + x] = cb;
            crp[y * bw + x] = cr;
        }
    }

    // ── headers ──
    let mut out = Vec::new();
    put_marker(&mut out, 0xD8); // SOI
    // APP0 JFIF
    put_seg(
        &mut out,
        0xE0,
        &[0x4A, 0x46, 0x49, 0x46, 0x00, 1, 1, 0, 0, 1, 0, 1, 0, 0],
    );
    // DQT (luma id0, chroma id1) in zigzag order
    let mut dqt = Vec::new();
    dqt.push(0x00);
    for k in 0..64 {
        dqt.push(lq[ZIGZAG[k]] as u8);
    }
    put_seg(&mut out, 0xDB, &dqt);
    let mut dqt2 = Vec::new();
    dqt2.push(0x01);
    for k in 0..64 {
        dqt2.push(cq[ZIGZAG[k]] as u8);
    }
    put_seg(&mut out, 0xDB, &dqt2);
    // SOF0
    let mut sof = Vec::new();
    sof.push(8); // precision
    sof.extend_from_slice(&(h as u16).to_be_bytes());
    sof.extend_from_slice(&(w as u16).to_be_bytes());
    sof.push(3); // components
    sof.extend_from_slice(&[1, 0x11, 0]); // Y: id1, 1x1, qt0
    sof.extend_from_slice(&[2, 0x11, 1]); // Cb
    sof.extend_from_slice(&[3, 0x11, 1]); // Cr
    put_seg(&mut out, 0xC0, &sof);
    // DHT (4 tables)
    let put_dht = |out: &mut Vec<u8>, tc_th: u8, bits: &[u8; 16], vals: &[u8]| {
        let mut body = Vec::new();
        body.push(tc_th);
        body.extend_from_slice(bits);
        body.extend_from_slice(vals);
        put_seg(out, 0xC4, &body);
    };
    put_dht(&mut out, 0x00, &DC_LUMA_BITS, &DC_LUMA_VALS);
    put_dht(&mut out, 0x10, &AC_LUMA_BITS, &AC_LUMA_VALS);
    put_dht(&mut out, 0x01, &DC_CHROMA_BITS, &DC_CHROMA_VALS);
    put_dht(&mut out, 0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS);
    // SOS
    let mut sos = Vec::new();
    sos.push(3);
    sos.extend_from_slice(&[1, 0x00]); // Y: DC0 AC0
    sos.extend_from_slice(&[2, 0x11]); // Cb: DC1 AC1
    sos.extend_from_slice(&[3, 0x11]); // Cr
    sos.extend_from_slice(&[0, 63, 0]); // Ss, Se, Ah/Al
    put_seg(&mut out, 0xDA, &sos);

    // ── entropy-coded scan ──
    let mut bw_writer = JpegBitWriter::new();
    let mut dc_pred = [0i32; 3];
    let planes: [&Vec<f32>; 3] = [&yp, &cbp, &crp];
    // Precompute reciprocal quant tables: per-coefficient float divide → multiply.
    let mut inv_lq = [0.0f32; 64];
    let mut inv_cq = [0.0f32; 64];
    for i in 0..64 {
        inv_lq[i] = 1.0 / lq[i] as f32;
        inv_cq[i] = 1.0 / cq[i] as f32;
    }
    let inv_qts: [&[f32; 64]; 3] = [&inv_lq, &inv_cq, &inv_cq];
    let dchs: [&HuffEnc; 3] = [&dc_l, &dc_c, &dc_c];
    let achs: [&HuffEnc; 3] = [&ac_l, &ac_c, &ac_c];

    let bx = bw / 8;
    let by = bh / 8;
    for byi in 0..by {
        for bxi in 0..bx {
            for comp in 0..3 {
                let mut block = [0.0f32; 64];
                for yy in 0..8 {
                    for xx in 0..8 {
                        block[yy * 8 + xx] =
                            planes[comp][(byi * 8 + yy) * bw + (bxi * 8 + xx)] - 128.0;
                    }
                }
                let freq = fdct8x8(&block, &basis);
                let iqt = inv_qts[comp];
                let mut q = [0i32; 64];
                for i in 0..64 {
                    q[i] = (freq[i] * iqt[i]).round() as i32;
                }
                encode_block(
                    &mut bw_writer,
                    &q,
                    &mut dc_pred[comp],
                    dchs[comp],
                    achs[comp],
                );
            }
        }
    }
    bw_writer.flush();
    out.extend_from_slice(&bw_writer.buf);
    put_marker(&mut out, 0xD9); // EOI
    Ok(out)
}

fn magnitude_category(v: i32) -> u32 {
    let a = v.unsigned_abs();
    32 - a.leading_zeros()
}

fn encode_block(
    bw: &mut JpegBitWriter,
    q: &[i32; 64],
    dc_pred: &mut i32,
    dch: &HuffEnc,
    ach: &HuffEnc,
) {
    // DC
    let diff = q[0] - *dc_pred;
    *dc_pred = q[0];
    let s = magnitude_category(diff);
    bw.put_bits(dch.codes[s as usize], dch.sizes[s as usize] as u32);
    if s > 0 {
        let v = if diff < 0 { (diff - 1) & ((1 << s) - 1) } else { diff } as u32;
        bw.put_bits(v, s);
    }
    // AC in zigzag order
    let mut run = 0u32;
    for k in 1..64 {
        let coef = q[ZIGZAG[k]];
        if coef == 0 {
            run += 1;
        } else {
            while run >= 16 {
                // ZRL (0xF0)
                bw.put_bits(ach.codes[0xF0], ach.sizes[0xF0] as u32);
                run -= 16;
            }
            let s = magnitude_category(coef);
            let rs = ((run << 4) | s) as usize;
            bw.put_bits(ach.codes[rs], ach.sizes[rs] as u32);
            let v = if coef < 0 { (coef - 1) & ((1 << s) - 1) } else { coef } as u32;
            bw.put_bits(v, s);
            run = 0;
        }
    }
    if run > 0 {
        // EOB (0x00)
        bw.put_bits(ach.codes[0x00], ach.sizes[0x00] as u32);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = ((x * 255) / w.max(1)) as u8;
                px[i + 1] = ((y * 255) / h.max(1)) as u8;
                px[i + 2] = (((x + y) * 128) / (w + h).max(1)) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn dct_roundtrips() {
        let basis = dct_basis();
        let mut blk = [0.0f32; 64];
        for (i, b) in blk.iter_mut().enumerate() {
            *b = ((i * 7 % 256) as f32) - 128.0;
        }
        let f = fdct8x8(&blk, &basis);
        let b = idct8x8(&f, &basis);
        for i in 0..64 {
            assert!((b[i] - blk[i]).abs() < 0.01, "DCT roundtrip off at {i}");
        }
    }

    #[test]
    fn jpeg_self_roundtrip_quality_high() {
        let (w, h) = (32usize, 24usize);
        let px = synth(w, h);
        let jpg = encode(w as u32, h as u32, &px, 92).expect("encode");
        let img = decode(&jpg).expect("decode");
        assert_eq!(img.width as usize, w);
        assert_eq!(img.height as usize, h);
        // lossy: check mean abs error is small at high quality
        let mut err = 0u64;
        for i in 0..w * h {
            for c in 0..3 {
                err += (img.pixels[i * 4 + c] as i64 - px[i * 4 + c] as i64).unsigned_abs();
            }
        }
        let mae = err as f64 / (w * h * 3) as f64;
        assert!(mae < 6.0, "MAE too high for q92: {mae}");
    }
}

#[cfg(test)]
mod huff_bench {
    use super::*;
    use std::time::Instant;
    // Load-invariant A/B: decode the SAME entropy stream via the 8-bit fast
    // table vs the per-bit slow walk, interleaved, min-of-many.
    #[test]
    fn fastpath_vs_perbit() {
        let h = HuffDec::build(&AC_LUMA_BITS, &AC_LUMA_VALS);
        let enc = HuffEnc::build(&AC_LUMA_BITS, &AC_LUMA_VALS);
        // build a long valid bitstream of AC symbols (deterministic pseudo-random)
        let mut bw = JpegBitWriter::new();
        let mut st = 0x12345u32;
        let nsym = 200_000usize;
        for _ in 0..nsym {
            st = st.wrapping_mul(1664525).wrapping_add(1013904223);
            // realistic skew: ~80% of symbols are the short-code (low-index) ones
            let sym = if (st >> 8) % 100 < 80 { (st >> 16) as usize % 12 } else { (st >> 16) as usize % 162 };
            let s = AC_LUMA_VALS[sym] as usize;
            bw.put_bits(enc.codes[s], enc.sizes[s] as u32);
        }
        bw.flush();
        let data = bw.buf.clone();

        let run_fast = || {
            let mut br = JpegBitReader::new(&data, 0);
            let mut acc = 0u32;
            for _ in 0..nsym { acc = acc.wrapping_add(br.decode_huff(&h).unwrap() as u32); }
            acc
        };
        // slow-only decoder (the old per-bit walk)
        let run_slow = || {
            let mut br = JpegBitReader::new(&data, 0);
            let mut acc = 0u32;
            for _ in 0..nsym {
                let mut code = 0i32;
                let mut sym = 0u8;
                for l in 1..=16 {
                    code = (code << 1) | br.get_bit() as i32;
                    if h.maxcode[l] >= 0 && code <= h.maxcode[l] {
                        sym = h.vals[(h.valptr[l] + (code - h.mincode[l])) as usize];
                        break;
                    }
                }
                acc = acc.wrapping_add(sym as u32);
            }
            acc
        };
        assert_eq!(run_fast(), run_slow(), "fast and slow must decode identically");
        let (mut fmin, mut smin) = (1e9f64, 1e9f64);
        for _ in 0..20 {
            let t = Instant::now(); std::hint::black_box(run_slow()); let e = t.elapsed().as_secs_f64(); if e < smin { smin = e; }
            let t = Instant::now(); std::hint::black_box(run_fast()); let e = t.elapsed().as_secs_f64(); if e < fmin { fmin = e; }
        }
        eprintln!("Huffman decode {nsym} symbols (min of 20, interleaved):");
        eprintln!("  per-bit (old): {:.3} ms   {:.1} Msym/s", smin*1e3, nsym as f64/1e6/smin);
        eprintln!("  fast table:    {:.3} ms   {:.1} Msym/s", fmin*1e3, nsym as f64/1e6/fmin);
        eprintln!("  speedup: {:.2}x", smin/fmin);
    }
}

#[cfg(test)]
mod writer_bench {
    use super::*;
    use std::time::Instant;
    // Load-invariant A/B: emit the same bit pattern via the bulk writer vs a
    // byte-at-a-time writer, interleaved, min-of-many. Same output asserted.
    #[test]
    fn bulk_vs_bytewise() {
        // deterministic (code,len) stream resembling JPEG entropy
        let mut st = 0x2468u32;
        let syms: Vec<(u32, u32)> = (0..300_000).map(|_| {
            st = st.wrapping_mul(1103515245).wrapping_add(12345);
            let len = 2 + (st >> 28) % 11;          // 2..12 bits
            let code = (st >> 8) & ((1 << len) - 1);
            (code, len)
        }).collect();

        let bulk = || {
            let mut w = JpegBitWriter::new();
            for &(c, l) in &syms { w.put_bits(c, l); }
            w.flush();
            w.buf
        };
        // byte-at-a-time reference (the old writer)
        let bytewise = || {
            let mut buf = Vec::new();
            let mut acc = 0u32; let mut nbits = 0u32;
            let mut put = |buf: &mut Vec<u8>, acc: &mut u32, nbits: &mut u32, code: u32, len: u32| {
                *acc |= code << (32 - *nbits - len); *nbits += len;
                while *nbits >= 8 { let b=(*acc>>24)as u8; buf.push(b); if b==0xFF{buf.push(0);} *acc<<=8; *nbits-=8; }
            };
            for &(c, l) in &syms { put(&mut buf, &mut acc, &mut nbits, c, l); }
            if nbits % 8 != 0 { let p=8-nbits%8; put(&mut buf,&mut acc,&mut nbits,(1<<p)-1,p); }
            buf
        };
        assert_eq!(bulk(), bytewise(), "bulk and byte-wise writers must produce identical bytes");
        let (mut bmin, mut wmin) = (1e9f64, 1e9f64);
        for _ in 0..20 {
            let t=Instant::now(); std::hint::black_box(bytewise()); let e=t.elapsed().as_secs_f64(); if e<wmin{wmin=e;}
            let t=Instant::now(); std::hint::black_box(bulk());     let e=t.elapsed().as_secs_f64(); if e<bmin{bmin=e;}
        }
        eprintln!("bit writer, 300k symbols (min of 20, interleaved):");
        eprintln!("  byte-wise (old): {:.3} ms", wmin*1e3);
        eprintln!("  bulk u64+SWAR:   {:.3} ms", bmin*1e3);
        eprintln!("  speedup: {:.2}x", wmin/bmin);
    }
}
