#![no_main]

use libfuzzer_sys::fuzz_target;

// bytes grown from Opus in Ogg, WebM and MP4, for src/decode/opus.rs and the Opus decoder
fuzz_target!(|data: &[u8]| choon_fuzz::analyze(data));
