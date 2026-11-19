#![no_main]

use libfuzzer_sys::fuzz_target;

// bytes grown from WavPack files, for the reader and decoder of src/decode/wavpack.rs
fuzz_target!(|data: &[u8]| choon_fuzz::analyze(data));
