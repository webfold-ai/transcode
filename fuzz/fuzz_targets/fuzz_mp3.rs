//! Untrusted input: MPEG Layer III parsing and full PCM decode.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::mp3::parse(data);
    let _ = transcode::mp3::decode_pcm(data);
});
