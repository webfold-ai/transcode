//! Untrusted input: any bytes are handed to the SLAC (lossless container) decoder.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::codec::decode(data);
});
