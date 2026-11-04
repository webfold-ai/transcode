# Standards and licensing

This page lists, for each format this crate decodes or encodes, the specification it follows, who
publishes that specification, and what is understood about its licensing and patent position.
It is written so a maintainer or a counsel can check each row quickly.

## Read this first

- **An open standard is not the same as a royalty-free one.** A specification can be published
  for free and still be covered by patents that need a licence. Several formats below are in that
  position.
- **This is a codec implementation, not a certified or licensed product.** Nothing here is
  conformance-certified, and no codec vendor has licensed this code. The crate writes and reads
  bytes; whether using those bytes needs a patent licence is a separate question.
- **Nothing here is legal advice.** Each row must be reviewed by counsel before the crate is
  published or used commercially. See the decision list at the end.

## Verification status

**The rows below were not verified against their primary sources in the session that wrote this
page.** Outbound DNS to the standards hosts (`www.w3.org`, `xiph.org`, `www.rfc-editor.org`,
`aomedia.org`, `developers.google.com`, `www.itu.int`, `via-la.com`, `www.matroska.org`) failed
from that environment. Each row gives the URL to check. Mark a row "verified" only after its
source has been read.

## Formats

| Format | Crate module(s) | Specification and body | Licensing and patent position (as understood) | Verified |
|---|---|---|---|---|
| PNG, RGBA8, non-interlaced | `src/png.rs` | PNG Specification, Third Edition. W3C Recommendation, identified as ISO/IEC 15948 | Published by the W3C under its Patent Policy, which provides royalty-free licensing. Source: https://www.w3.org/TR/png-3/ | No |
| JPEG, baseline sequential | `src/jpeg.rs`, `src/mjpeg.rs` | ITU-T T.81, equivalent to ISO/IEC 10918-1. ITU-T and ISO/IEC JPEG committee | The specification is freely downloadable. Baseline JPEG is widely treated as royalty-free, and the main early patent claims are understood to have expired. Confirm with counsel. Source: https://www.itu.int/rec/T-REC-T.81 | No |
| WebP, VP8L lossless | `src/webp.rs` | WebP Lossless Bitstream Specification. Google | Google states a royalty-free patent licence for WebP. The reference library, libwebp, is BSD-3-Clause. Source: https://developers.google.com/speed/webp/docs/webp-lossless-bitstream-specification and https://developers.google.com/speed/webp/faq | No |
| RIFF WAVE, PCM | `src/main.rs`, `src/codec.rs` (CLI path) | RIFF/WAVE, as documented by Microsoft and IBM in the Multimedia Programming Interface and Data Specifications 1.0 (1991). No standards body | Not reviewed. The format is old and widely implemented; no licence requirement has been identified, but that has not been checked. | No |
| FLAC | `src/flac.rs` | FLAC format specification. Xiph.Org Foundation | Documented by Xiph as free to use with no royalties; the reference implementation is BSD-licensed. Source: https://xiph.org/flac/format.html | No |
| SLAC | `src/codec.rs` | This crate's own container. No external specification | Covered by this crate's licence. Nothing external to license. | Not applicable |
| MP3, Layer III decode | `src/mp3.rs` | ISO/IEC 11172-3 (MPEG-1 audio), ISO/IEC 13818-3 (MPEG-2 audio) | The core MP3 patents are understood to have expired in most jurisdictions, but the position varies by country, so this is not a clean grant. Confirm with counsel. Source: https://www.iso.org/standard/22412.html | No |
| AAC-LC in ADTS, encode and decode | `src/aac.rs`, `src/aac_tables.rs` | ISO/IEC 13818-7 (MPEG-2 AAC), ISO/IEC 14496-3 (MPEG-4 Audio) | Patents are licensed through the Via LA AAC patent pool. Royalties can apply. Source: https://via-la.com/licensing-programs/aac/ | No |
| Opus, Ogg demux and TOC parsing | `src/opus.rs`, `src/opus_tables.rs` | RFC 6716 (Opus codec), RFC 7845 (Ogg encapsulation). IETF | Opus is documented in the RFCs as royalty-free, with IPR declarations in the RFC record. Packet-to-PCM decode is not implemented. Sources: https://www.rfc-editor.org/rfc/rfc6716 and https://www.rfc-editor.org/rfc/rfc7845 | No |
| H.264 (AVC), Annex B, intra and P-skip encode | `src/h264.rs`, `src/h264_intra.rs`, `src/h264_inter.rs`, `src/h264_i4x4.rs`, `src/h264bits.rs`, `src/h264_cavlc_tables.rs` | ITU-T H.264, equivalent to ISO/IEC 14496-10 | Patents are licensed through the Via LA AVC patent pool. Royalties apply above the pool's thresholds, with stated exceptions. **This is the highest-risk row in this crate.** Source: https://via-la.com/licensing-programs/avc/ and https://www.itu.int/rec/T-REC-H.264 | No |
| AV1, keyframe OBU encode | `src/av1_obu.rs`, `src/av1_frame.rs`, `src/av1_intra.rs`, `src/av1_coef.rs`, `src/av1_coef_tables.rs`, `src/av1_ec.rs`, `src/av1_dct.rs` | AV1 Bitstream and Decoding Process Specification. Alliance for Open Media (AOMedia) | AOMedia publishes a royalty-free patent licence for AV1. Its terms include a termination condition if a licensee sues over AV1. A royalty-free licence is not a statement that no patent applies. Source: https://aomedia.org/license/patent-license/ | No |
| MP4 writer | `src/mp4.rs` | ISO/IEC 14496-12 (ISO base media file format), ISO/IEC 14496-14 (MP4 file format) | Container only. Not reviewed. The writer carries H.264 by default, so the H.264 row applies to its output. | No |
| Matroska (MKV) writer | `src/mkv.rs` | Matroska specification. matroska.org | Container only. The licence of the published specification was not verified. The writer carries H.264, so the H.264 row applies to its output. Source: https://www.matroska.org/technical/specs/index.html | No |
| HEIF writer | `src/heif.rs` | ISO/IEC 23008-12 (HEIF). ISO/IEC | Container only. The image coding here is H.264, so the H.264 row applies to its output. Not reviewed beyond that. | No |
| Motion JPEG in AVI | `src/mjpeg.rs` | RIFF AVI, as documented by Microsoft. Frames are JPEG (ITU-T T.81) | AVI is not an ISO standard and its licence was not reviewed. The JPEG frames follow the JPEG row. | No |

Two formats named in the repository description are **not implemented** in this source tree:
NIfTI and HDF5. This page does not cover them. Do not cite them as supported.

## What the MIT licence covers

The crate's own code is licensed under the MIT licence (`LICENSE-MIT`, `license = "MIT"` in
`Cargo.toml`). That licence covers this code only. It grants no rights under any patent that
applies to a format above. A dual `MIT OR Apache-2.0` licence has been discussed but is not in the
repository; that is an owners' decision.

## Decisions for the owners

1. **Patent review (H.264, AAC, and the MP4, MKV and HEIF writers that carry H.264).** Counsel must
   review each row before the crate is published. Until then the README must not describe the
   crate as patent-free.
2. **MP3 and JPEG.** Confirm the expiry dates and jurisdictions with counsel.
3. **Verification of every row.** Read each primary source above and mark the "Verified" column.
4. **Licence.** MIT only, or dual MIT and Apache-2.0 with a `LICENSE-APACHE` file.
5. **Publication.** Whether to publish any encoder that writes H.264 or AAC at all.
