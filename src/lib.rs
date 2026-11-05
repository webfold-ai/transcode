//! Library surface for the sovereign codec crate — exposes the from-scratch codec
//! decoders/encoders so other crates can call them (e.g. `transcode::flac::decode(bytes)`
//! → `(samples i32, rate, channels, bps)` for the STT audio front-end). The `transcode`
//! binary (`main.rs`) keeps its own module tree; this just makes the codecs reusable.

#![forbid(unsafe_code)]

pub mod aac;
pub mod aac_tables;
pub mod av1_coef;
pub mod av1_coef_tables;
pub mod av1_dct;
pub mod av1_ec;
pub mod av1_frame;
pub mod av1_intra;
pub mod av1_obu;
pub mod codec;
pub mod flac;
pub mod h264;
pub mod h264_i4x4;
pub mod h264_inter;
pub mod h264_intra;
pub mod h264bits;
pub mod heif;
pub mod jpeg;
pub mod mjpeg;
pub mod mkv;
pub mod mp3;
pub mod mp3_tables;
pub mod mp4;
pub mod opus;
pub mod opus_tables;
pub mod png;
pub mod preview;
pub mod webp;
