//! Untrusted input: Ogg demux, OpusHead parse and TOC parse. Full Opus decode is not implemented yet (ROADMAP.md), so it is not fuzzed here.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = transcode::opus::parse(data);
    if let Ok(packets) = transcode::opus::demux_ogg(data) {
        for p in packets.iter().take(64) {
            if let Some(&toc) = p.data.first() {
                let _ = transcode::opus::parse_toc(toc);
            }
        }
    }
});
