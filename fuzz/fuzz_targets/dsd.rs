#![no_main]

use libfuzzer_sys::fuzz_target;

// bytes grown from DSF and DSDIFF files, for the reader and decoder of src/decode/dsd.rs
fuzz_target!(|data: &[u8]| choon_fuzz::analyze(data));
