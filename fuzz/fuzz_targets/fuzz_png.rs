//! Untrusted input: any bytes are handed to the PNG decoder. It must return Ok or Err and never panic, overflow or hang.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::png::decode(data);
});
