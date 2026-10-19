#![no_main]

use bytes::BytesMut;
use harrow_codec_h1::{CHUNK_TERMINATOR, PayloadDecoder, PayloadItem, encode_chunk_into, try_parse_request};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let use_chunked = data.first().is_some_and(|b| b & 1 == 1);
    let path_len = data.get(1).copied().unwrap_or(0) as usize % 24;
    let body_len = data.len().saturating_sub(2 + path_len).min(4096);

    // Inputs shorter than the two control bytes are fine: no path, no body.
    let rest = data.get(2..).unwrap_or(&[]);
    let path_bytes = &rest[..path_len.min(rest.len())];
    let mut path = String::from("/roundtrip");
    if !path_bytes.is_empty() {
        path.push('/');
        for &byte in path_bytes {
            let ch = match byte % 4 {
                0 => (b'a' + (byte % 26)) as char,
                1 => (b'0' + (byte % 10)) as char,
                2 => '-',
                _ => '_',
            };
            path.push(ch);
        }
    }

    let body = &rest[path_bytes.len()..path_bytes.len() + body_len];

    let mut request = if use_chunked {
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .into_bytes()
    } else {
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes()
    };

    if use_chunked {
        let split = body.len() / 2;
        encode_chunk_into(&body[..split], &mut request);
        encode_chunk_into(&body[split..], &mut request);
        request.extend_from_slice(CHUNK_TERMINATOR);
    } else {
        request.extend_from_slice(body);
    }

    let parsed = try_parse_request(&request).expect("constructed request should parse");
    // `Content-Length: 0` has no payload to decode.
    let Some(mut decoder) = PayloadDecoder::from_parsed(&parsed) else {
        assert!(body.is_empty() && !use_chunked, "only an empty fixed-length body has no decoder");
        return;
    };
    let mut buf = BytesMut::from(&request[parsed.header_len..]);
    let mut decoded = Vec::new();

    loop {
        match decoder.decode(&mut buf, Some(8192)).expect("decode") {
            Some(PayloadItem::Chunk(chunk)) => decoded.extend_from_slice(&chunk),
            Some(PayloadItem::Eof) => break,
            None => panic!("constructed request body should decode completely"),
        }
    }

    assert_eq!(decoded.as_slice(), body);
});
