#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Stateless decoder: must not panic on any input.
    let _ = harrow_codec_h1::decode_chunked_with_limit(data, None);
    let _ = harrow_codec_h1::decode_chunked_with_limit(data, Some(1024));
    let _ = harrow_codec_h1::decode_chunked_with_limit(data, Some(0));
});
