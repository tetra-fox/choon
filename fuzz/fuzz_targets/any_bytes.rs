#![no_main]

use libfuzzer_sys::fuzz_target;

// any bytes at all, as a file a stranger sent
fuzz_target!(|data: &[u8]| choon_fuzz::analyze(data));
