//! Library surface for the sovereign codec crate — exposes the from-scratch codec
//! decoders/encoders so other crates can call them (e.g. `transcode::flac::decode(bytes)`
//! → `(samples i32, rate, channels, bps)` for the STT audio front-end). The `transcode`
//! binary (`main.rs`) keeps its own module tree; this just makes the codecs reusable.

pub mod aac;
pub mod aac_tables;
pub mod codec;
pub mod flac;
pub mod mp3;
pub mod mp3_tables;
pub mod opus;
pub mod opus_tables;
