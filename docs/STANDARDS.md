# Standards and licensing

This page lists, for each format this crate decodes or encodes, the specification it follows, who
publishes that specification, and what is understood about its licensing and patent position.
Each row links to the source to check.

## Read this first

- **An open standard is not the same as a royalty-free one.** A specification can be published
  for free and still be covered by patents that need a licence. Several formats below are in that
  position.
- **This is a codec implementation, not a certified or licensed product.** Nothing here is
  conformance-certified, and no codec vendor has licensed this code. The crate writes and reads
  bytes; whether using those bytes needs a patent licence is a separate question.
- **Nothing here is legal advice.** It records what the maintainers understand, with sources.
  Check the sources yourself before you ship a product built on these formats. See
  [Using this code in a product](#using-this-code-in-a-product).

## Verification status

The "Source check" column says how each row was checked.

- **Secondary** means the position was read in secondary sources (news reports, mailing-list and
  forum posts quoting the licensor's FAQ). The licensor's own page was not read.
- **No** means the row has not been checked against any source.

The primary hosts (`www.w3.org`, `xiph.org`, `www.rfc-editor.org`, `aomedia.org`,
`developers.google.com`, `www.itu.int`, `www.via-la.com`, `www.matroska.org`) could not be reached
from the environment that wrote this page. Each row gives the URL to check.

## Formats

| Format | Crate module(s) | Specification and body | Licensing and patent position (as understood) | Source check |
|---|---|---|---|---|
| PNG, RGBA8, non-interlaced | `src/png.rs` | PNG Specification, Third Edition. W3C Recommendation, identified as ISO/IEC 15948 | Published by the W3C under its Patent Policy, which provides royalty-free licensing. Source: https://www.w3.org/TR/png-3/ | No |
| JPEG, baseline sequential | `src/jpeg.rs`, `src/mjpeg.rs` | ITU-T T.81, equivalent to ISO/IEC 10918-1. ITU-T and ISO/IEC JPEG committee | The specification is freely downloadable. Baseline JPEG is widely treated as royalty-free, and the main early patent claims are understood to have expired. Source: https://www.itu.int/rec/T-REC-T.81 | No |
| WebP, VP8L lossless | `src/webp.rs` | WebP Lossless Bitstream Specification. Google | Google states a royalty-free patent licence for WebP. The reference library, libwebp, is BSD-3-Clause. Source: https://developers.google.com/speed/webp/docs/webp-lossless-bitstream-specification and https://developers.google.com/speed/webp/faq | No |
| RIFF WAVE, PCM | `src/main.rs`, `src/codec.rs` (CLI path) | RIFF/WAVE, as documented by Microsoft and IBM in the Multimedia Programming Interface and Data Specifications 1.0 (1991). No standards body | Not reviewed. The format is old and widely implemented; no licence requirement has been identified, but that has not been checked. | No |
| FLAC | `src/flac.rs` | FLAC format specification. Xiph.Org Foundation | Documented by Xiph as free to use with no royalties; the reference implementation is BSD-licensed. Source: https://xiph.org/flac/format.html | No |
| SLAC | `src/codec.rs` | This crate's own container. No external specification | Covered by this crate's licence. Nothing external to license. | Not applicable |
| MP3, Layer III decode | `src/mp3.rs` | ISO/IEC 11172-3 (MPEG-1 audio), ISO/IEC 13818-3 (MPEG-2 audio) | Fraunhofer and Technicolor ended their MP3 licensing program on 23 April 2017. Secondary sources report that the last US patent in the program expired on 16 April 2017 and that the core EU patents had expired by 2012. Other jurisdictions were not checked. Source: https://www.iso.org/standard/22412.html | Secondary |
| AAC-LC in ADTS, encode and decode | `src/aac.rs`, `src/aac_tables.rs` | ISO/IEC 13818-7 (MPEG-2 AAC), ISO/IEC 14496-3 (MPEG-4 Audio) | Patents are licensed through the Via LA AAC patent pool. Secondary sources quote Via LA's AAC FAQ as saying that manufacturers and developers of end-user encoder or decoder products need a licence, and that implementations, including open-source ones, may need licences from the patent holders. Royalties can apply. Source: https://via-la.com/licensing-programs/aac/ | Secondary |
| Opus, Ogg demux and TOC parsing | `src/opus.rs`, `src/opus_tables.rs` | RFC 6716 (Opus codec), RFC 7845 (Ogg encapsulation). IETF | Opus is documented in the RFCs as royalty-free, with IPR declarations in the RFC record. Packet-to-PCM decode is not implemented. Sources: https://www.rfc-editor.org/rfc/rfc6716 and https://www.rfc-editor.org/rfc/rfc7845 | No |
| H.264 (AVC), Annex B, intra and P-skip encode | `src/h264.rs`, `src/h264_intra.rs`, `src/h264_inter.rs`, `src/h264_i4x4.rs`, `src/h264bits.rs`, `src/h264_cavlc_tables.rs` | ITU-T H.264, equivalent to ISO/IEC 14496-10 | Patents are licensed through the Via LA AVC patent pool. Royalties apply above the pool's thresholds, with stated exceptions. Publishing source code does not license the patents: Cisco released OpenH264 under the BSD licence, covers the pool royalties only for the binaries it distributes, and tells others to get their own licence. **This is the row with the most patent exposure in this crate.** Source: https://via-la.com/licensing-programs/avc/ and https://www.itu.int/rec/T-REC-H.264 | Secondary |
| AV1, keyframe OBU encode | `src/av1_obu.rs`, `src/av1_frame.rs`, `src/av1_intra.rs`, `src/av1_coef.rs`, `src/av1_coef_tables.rs`, `src/av1_ec.rs`, `src/av1_dct.rs` | AV1 Bitstream and Decoding Process Specification. Alliance for Open Media (AOMedia) | AOMedia publishes a royalty-free patent licence for AV1. Its terms include a termination condition if a licensee sues over AV1. A royalty-free licence is not a statement that no patent applies. Source: https://aomedia.org/license/patent-license/ | No |
| MP4 writer | `src/mp4.rs` | ISO/IEC 14496-12 (ISO base media file format), ISO/IEC 14496-14 (MP4 file format) | Container only. Not reviewed. The writer carries H.264 by default, so the H.264 row applies to its output. | No |
| Matroska (MKV) writer | `src/mkv.rs` | Matroska specification. matroska.org | Container only. The licence of the published specification was not verified. The writer carries H.264, so the H.264 row applies to its output. Source: https://www.matroska.org/technical/specs/index.html | No |
| HEIF writer | `src/heif.rs` | ISO/IEC 23008-12 (HEIF). ISO/IEC | Container only. The image coding here is H.264, so the H.264 row applies to its output. Not reviewed beyond that. | No |
| Motion JPEG in AVI | `src/mjpeg.rs` | RIFF AVI, as documented by Microsoft. Frames are JPEG (ITU-T T.81) | AVI is not an ISO standard and its licence was not reviewed. The JPEG frames follow the JPEG row. | No |

NIfTI and HDF5 are **not implemented** in this source tree and are not covered here.

## What the licence covers

The crate's own code is licensed under the Apache License, Version 2.0 (`LICENSE`,
`license = "Apache-2.0"` in `Cargo.toml`). That licence covers this code only. It grants no rights
under any patent that applies to a format above.

## Using this code in a product

Publishing this source under Apache-2.0 does not license anyone under another party's patents.
That is true of every open-source codec, and it is the position Cisco's OpenH264 states.

- **Using the source or the library.** You are responsible for the patent licences your own product
  needs. For H.264 and AAC, check with the patent pool (Via LA) before you ship an encoder or
  decoder to end users.
- **Formats without a pool.** PNG, WebP, FLAC, Opus and AV1 are published by their authors under
  royalty-free terms (see the rows above). Those terms still carry conditions. Read them.
- **Containers.** The MP4, Matroska and HEIF writers carry H.264 here, so the H.264 row applies to
  their output.
- **No code is withheld.** All modules in this repository are published. Which codecs you build
  in, or ship, is your decision.
