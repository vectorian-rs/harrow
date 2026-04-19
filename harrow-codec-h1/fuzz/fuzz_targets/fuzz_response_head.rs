#![no_main]
use libfuzzer_sys::fuzz_target;

use http::{HeaderMap, StatusCode};

fuzz_target!(|data: &[u8]| {
    // Build a HeaderMap from fuzz data: interpret pairs of length-prefixed
    // byte slices as header name/value pairs.
    let mut headers = HeaderMap::new();
    let mut pos = 0;

    while pos + 2 < data.len() {
        let name_len = data[pos] as usize;
        pos += 1;
        if pos + name_len >= data.len() {
            break;
        }
        let name_bytes = &data[pos..pos + name_len];
        pos += name_len;

        let val_len = data[pos] as usize;
        pos += 1;
        if pos + val_len > data.len() {
            break;
        }
        let val_bytes = &data[pos..pos + val_len];
        pos += val_len;

        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(name_bytes),
            http::header::HeaderValue::from_bytes(val_bytes),
        ) {
            headers.append(name, val);
        }
    }

    // Status code from first two bytes.
    let code = if data.len() >= 2 {
        let raw = u16::from_le_bytes([data[0], data[1]]);
        StatusCode::from_u16(raw.clamp(100, 599)).unwrap_or(StatusCode::OK)
    } else {
        StatusCode::OK
    };

    // Must not panic.
    let _ = harrow_codec_h1::write_response_head(code, &headers, false);
    let _ = harrow_codec_h1::write_response_head(code, &headers, true);

    let mut buf = Vec::new();
    harrow_codec_h1::write_response_head_into(code, &headers, false, &mut buf);
});
