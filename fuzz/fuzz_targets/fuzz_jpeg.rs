//! Untrusted input: any bytes are handed to the baseline JPEG decoder.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::jpeg::decode(data);
});
