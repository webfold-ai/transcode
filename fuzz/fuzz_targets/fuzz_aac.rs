//! Untrusted input: ADTS/AAC-LC parsing and full PCM decode.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::aac::parse(data);
    let _ = transcode::aac::decode_pcm(data);
});
