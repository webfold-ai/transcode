//! Lossless WebP (VP8L) encoder for RGBA8 input.
//!
//! The bitstream is valid VP8L with no predictive transform, no colour cache, and flat
//! 8-bit prefix codes for the four literal channels. That is correct but not compact: each
//! pixel costs about four bytes before the RIFF wrapper. Entropy coding (per-image prefix
//! codes, transforms, colour cache) is the next step if output size matters.
//!
//! Lossy WebP (VP8) is not implemented.

/// Encode `pixels` (RGBA8, row-major, `width * height * 4` bytes) as a lossless WebP file.
pub fn encode(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(format!("webp: dimensions {width}x{height} out of range (1..=16384)"));
    }
    let expected = width as usize * height as usize * 4;
    if pixels.len() != expected {
        return Err(format!("webp: expected {expected} RGBA bytes, got {}", pixels.len()));
    }

    let mut bits = BitWriter::default();
    // VP8L signature, then width-1 and height-1 (14 bits each), alpha hint, version 0.
    bits.put(0x2f, 8);
    bits.put(width - 1, 14);
    bits.put(height - 1, 14);
    bits.put(1, 1);
    bits.put(0, 3);
    // No transform, no colour cache, no meta prefix codes.
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    // Prefix codes: green (256 literals + 24 length prefixes), red, blue, alpha, distance.
    write_flat_literal_code(&mut bits, 280);
    write_flat_literal_code(&mut bits, 256);
    write_flat_literal_code(&mut bits, 256);
    write_flat_literal_code(&mut bits, 256);
    // Distance code: a single symbol, so it costs no bits per use (back-references are unused).
    bits.put(1, 1); // simple code
    bits.put(0, 1); // one symbol
    bits.put(0, 1); // symbol fits in one bit
    bits.put(0, 1); // symbol 0

    for px in pixels.chunks_exact(4) {
        // Bitstream order per pixel: green, red, blue, alpha.
        bits.put(reverse8(px[1]), 8);
        bits.put(reverse8(px[0]), 8);
        bits.put(reverse8(px[2]), 8);
        bits.put(reverse8(px[3]), 8);
    }
    let payload = bits.finish();
    Ok(riff_wrap(&payload))
}

/// Canonical code for a symbol in an alphabet where every literal has length 8: the code is the
/// symbol itself. The code is read MSB-first, so the bits are written reversed.
fn write_flat_literal_code(bits: &mut BitWriter, alphabet: usize) {
    // Normal (not simple) code.
    bits.put(0, 1);
    // Code-length code: only symbols 0 and 8 are used, each with length 1. The order table
    // below is fixed by the spec; symbol 8 sits at index 11, so 12 entries are sent.
    const ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let needed = ORDER.iter().position(|&s| s == 8).map_or(0, |p| p + 1);
    bits.put((needed - 4) as u32, 4);
    for &sym in ORDER.iter().take(needed) {
        let len = if sym == 0 || sym == 8 { 1 } else { 0 };
        bits.put(len, 3);
    }
    // Use every symbol (no max_symbol limit).
    bits.put(0, 1);
    // Code lengths, each sent with the 1-bit code: 0 → "0", 8 → "1".
    for symbol in 0..alphabet {
        bits.put(u32::from(symbol < 256), 1);
    }
}

fn reverse8(v: u8) -> u32 {
    u32::from(v.reverse_bits())
}

fn riff_wrap(payload: &[u8]) -> Vec<u8> {
    let pad = payload.len() % 2;
    let chunk_len = payload.len() as u32;
    let riff_len = 4 + 8 + chunk_len + pad as u32;
    let mut out = Vec::with_capacity(12 + 8 + payload.len() + pad);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8L");
    out.extend_from_slice(&chunk_len.to_le_bytes());
    out.extend_from_slice(payload);
    if pad == 1 {
        out.push(0);
    }
    out
}

/// LSB-first bit writer: values are appended from the least significant bit upward.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        self.acc |= u64::from(value) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn decode(webp: &[u8]) -> (u32, u32, bool, Vec<u8>) {
        let mut dec = image_webp::WebPDecoder::new(Cursor::new(webp)).expect("valid webp header");
        let (w, h) = dec.dimensions();
        let mut buf = vec![0u8; dec.output_buffer_size().expect("buffer size")];
        dec.read_image(&mut buf).expect("decode");
        (w, h, dec.has_alpha(), buf)
    }

    #[test]
    fn round_trips_every_pixel_exactly() {
        let (w, h) = (5u32, 3u32);
        let pixels: Vec<u8> = (0..w * h).flat_map(|i| [(i * 37) as u8, (i * 91) as u8, (i * 13) as u8, (255 - i) as u8]).collect();
        let webp = encode(w, h, &pixels).expect("encode");
        assert_eq!(&webp[0..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        let (dw, dh, has_alpha, decoded) = decode(&webp);
        assert_eq!((dw, dh), (w, h));
        assert!(has_alpha);
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn odd_payload_length_is_padded_to_even() {
        let webp = encode(1, 1, &[1, 2, 3, 4]).expect("encode");
        let riff_len = u32::from_le_bytes([webp[4], webp[5], webp[6], webp[7]]) as usize;
        assert_eq!(riff_len + 8, webp.len());
        assert_eq!(webp.len() % 2, 0);
    }

    #[test]
    fn rejects_bad_dimensions_and_buffer_sizes() {
        assert!(encode(0, 1, &[]).is_err());
        assert!(encode(2, 2, &[0; 4]).is_err());
    }
}
