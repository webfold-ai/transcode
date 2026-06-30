//! Text previews for decoded raster images.
//!
//! The crate already knows how to decode PNG and JPEG into canonical RGBA.
//! This module turns that RGBA into a small grayscale terminal preview so a
//! caller can show something meaningful in a TUI without pulling in a heavy
//! graphics stack.

use crate::{jpeg, png};

/// Decode a supported raster image into canonical RGBA.
pub fn decode_image(data: &[u8]) -> Result<png::Image, String> {
    png::decode(data).or_else(|_| jpeg::decode(data))
}

fn luminance(rgba: &[u8]) -> u8 {
    // u32: the weighted sum reaches ~2.55M (255·10000 + 5000), which overflows u16
    // — in debug that panics, in release it wraps to garbage. Compute wide, then
    // narrow the final 0..=255 result.
    let r = rgba[0] as u32;
    let g = rgba[1] as u32;
    let b = rgba[2] as u32;
    // Rec. 709 luma, rounded to 8-bit.
    ((2126 * r + 7152 * g + 722 * b + 5000) / 10_000) as u8
}

fn shade_char(v: u8) -> char {
    const RAMP: &[u8] = b"@%#*+=-:. ";
    let idx = (usize::from(255u8.saturating_sub(v)) * (RAMP.len() - 1)) / 255;
    RAMP[idx] as char
}

/// Render a supported raster image into a small ASCII preview.
///
/// The preview is grayscale, deterministic, and bounded by `max_width ×
/// max_height`. Callers can prefix a label line if they want metadata too.
pub fn render_preview(
    data: &[u8],
    max_width: usize,
    max_height: usize,
) -> Result<Vec<String>, String> {
    if max_width == 0 || max_height == 0 {
        return Ok(Vec::new());
    }
    let img = decode_image(data)?;
    let w = img.width as usize;
    let h = img.height as usize;
    if w == 0 || h == 0 {
        return Err("image has zero dimensions".into());
    }

    let scale_w = max_width as f64 / w as f64;
    let scale_h = max_height as f64 / h as f64;
    let scale = scale_w.min(scale_h).min(1.0);
    let out_w = ((w as f64 * scale).round() as usize).max(1).min(max_width);
    let out_h = ((h as f64 * scale).round() as usize).max(1).min(max_height);

    let mut lines = Vec::with_capacity(out_h);
    for oy in 0..out_h {
        let sy = oy * h / out_h;
        let mut line = String::with_capacity(out_w);
        for ox in 0..out_w {
            let sx = ox * w / out_w;
            let idx = (sy * w + sx) * 4;
            line.push(shade_char(luminance(&img.pixels[idx..idx + 4])));
        }
        lines.push(line);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::luminance;

    #[test]
    fn luminance_does_not_overflow_on_bright_pixels() {
        // White must not panic (debug) or wrap (release); luma(255,255,255) ≈ 255.
        assert_eq!(luminance(&[255, 255, 255, 255]), 255);
        assert_eq!(luminance(&[0, 0, 0, 255]), 0);
        // pure green carries the largest single-channel weight (7152)
        assert_eq!(luminance(&[0, 255, 0, 255]), 182);
    }
}
