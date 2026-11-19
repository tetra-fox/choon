#![no_main]

use libfuzzer_sys::fuzz_target;

// bytes grown from AAC in ADTS and MP4, for the decoder of src/decode/aac.rs
fuzz_target!(|data: &[u8]| choon_fuzz::analyze(data));
