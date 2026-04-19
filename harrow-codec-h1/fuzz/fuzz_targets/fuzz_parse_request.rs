#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Must not panic on any input. Errors are fine.
    let _ = harrow_codec_h1::try_parse_request(data);
});
