use bytes::{Bytes, BytesMut};
use http::header::{CONNECTION, CONTENT_LENGTH, EXPECT, TRANSFER_ENCODING};
use http::{HeaderMap, Method, Uri, Version};

/// Maximum number of headers we parse per request.
const MAX_HEADERS: usize = 100;

/// Result of parsing HTTP/1.1 request headers.
pub(crate) struct ParsedRequest {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    /// Number of bytes consumed from the buffer (headers + \r\n\r\n).
    pub header_len: usize,
    /// Content-Length value, if present.
    pub content_length: Option<u64>,
    /// Whether Transfer-Encoding: chunked is present.
    pub chunked: bool,
    /// Whether to keep the connection alive after this request.
    pub keep_alive: bool,
    /// Whether the client sent `Expect: 100-continue`.
    pub expect_continue: bool,
}

/// Errors from the codec layer.
#[derive(Debug)]
pub(crate) enum CodecError {
    /// Need more data to parse headers.
    Incomplete,
    /// Decoded chunked body exceeds the configured limit.
    BodyTooLarge,
    /// Invalid HTTP request.
    Invalid(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Incomplete => write!(f, "incomplete HTTP request"),
            CodecError::BodyTooLarge => write!(f, "body too large"),
            CodecError::Invalid(msg) => write!(f, "invalid HTTP request: {msg}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Try to parse HTTP/1.1 request headers from the buffer.
///
/// Returns `Err(CodecError::Incomplete)` if there isn't a complete header block yet.
pub(crate) fn try_parse_request(buf: &[u8]) -> Result<ParsedRequest, CodecError> {
    let mut headers_buf = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers_buf);

    let header_len = match parsed.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Err(CodecError::Incomplete),
        Err(e) => return Err(CodecError::Invalid(e.to_string())),
    };

    let method = parsed
        .method
        .ok_or_else(|| CodecError::Invalid("missing method".into()))?;
    let method: Method = method
        .parse()
        .map_err(|e: http::method::InvalidMethod| CodecError::Invalid(e.to_string()))?;

    let path = parsed
        .path
        .ok_or_else(|| CodecError::Invalid("missing path".into()))?;
    let uri: Uri = path
        .parse()
        .map_err(|e: http::uri::InvalidUri| CodecError::Invalid(e.to_string()))?;

    let version = match parsed.version {
        Some(1) => Version::HTTP_11,
        Some(0) => Version::HTTP_10,
        _ => Version::HTTP_11,
    };

    let mut headers = HeaderMap::with_capacity(parsed.headers.len());
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    let mut conn_close = false;
    let mut conn_keep_alive = false;
    let mut expect_continue = false;

    for h in parsed.headers.iter() {
        let name = http::header::HeaderName::from_bytes(h.name.as_bytes())
            .map_err(|e| CodecError::Invalid(e.to_string()))?;
        let value = http::header::HeaderValue::from_bytes(h.value)
            .map_err(|e| CodecError::Invalid(e.to_string()))?;

        if name == CONTENT_LENGTH {
            if let Ok(s) = std::str::from_utf8(h.value)
                && let Ok(len) = s.trim().parse::<u64>()
            {
                content_length = Some(len);
            }
        } else if name == TRANSFER_ENCODING {
            if let Ok(s) = std::str::from_utf8(h.value)
                && s.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        } else if name == CONNECTION
            && let Ok(s) = std::str::from_utf8(h.value)
        {
            let lower = s.to_ascii_lowercase();
            if lower.contains("close") {
                conn_close = true;
            }
            if lower.contains("keep-alive") {
                conn_keep_alive = true;
            }
        } else if name == EXPECT
            && let Ok(s) = std::str::from_utf8(h.value)
            && s.trim().eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }

        headers.append(name, value);
    }

    let keep_alive = should_keep_alive(version, conn_close, conn_keep_alive);

    if content_length.is_some() && chunked {
        return Err(CodecError::Invalid(
            "content-length and transfer-encoding: chunked cannot be combined".into(),
        ));
    }

    Ok(ParsedRequest {
        method,
        uri,
        version,
        headers,
        header_len,
        content_length,
        chunked,
        keep_alive,
        expect_continue,
    })
}

/// Determine keep-alive from HTTP version and Connection header.
///
/// HTTP/1.1: keep-alive by default, unless `Connection: close`.
/// HTTP/1.0: close by default, unless `Connection: keep-alive`.
fn should_keep_alive(version: Version, conn_close: bool, conn_keep_alive: bool) -> bool {
    if conn_close {
        return false;
    }
    if conn_keep_alive {
        return true;
    }
    // Default: HTTP/1.1 keeps alive, HTTP/1.0 does not.
    version == Version::HTTP_11
}

/// Write the HTTP response status line + headers into a buffer.
///
/// If the response body will use chunked encoding, `chunked` should be `true`
/// so that `Transfer-Encoding: chunked` is added.
pub(crate) fn write_response_head(
    status: http::StatusCode,
    headers: &HeaderMap,
    chunked: bool,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    // Status line
    buf.extend_from_slice(b"HTTP/1.1 ");
    buf.extend_from_slice(status.as_str().as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(status.canonical_reason().unwrap_or("").as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Headers
    for (name, value) in headers.iter() {
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    if chunked {
        buf.extend_from_slice(b"transfer-encoding: chunked\r\n");
    }

    buf.extend_from_slice(b"\r\n");
    buf
}

/// Encode a single chunk for chunked transfer-encoding.
pub(crate) fn encode_chunk(data: &[u8]) -> Vec<u8> {
    let hex_len = format!("{:x}", data.len());
    let mut buf = Vec::with_capacity(hex_len.len() + 2 + data.len() + 2);
    buf.extend_from_slice(hex_len.as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(data);
    buf.extend_from_slice(b"\r\n");
    buf
}

/// Chunked transfer-encoding terminator.
pub(crate) const CHUNK_TERMINATOR: &[u8] = b"0\r\n\r\n";

/// HTTP/1.1 100 Continue interim response.
pub(crate) const CONTINUE_100: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Decode chunked transfer-encoding and optionally fail once the decoded body
/// would exceed `max_body`.
pub(crate) fn decode_chunked_with_limit(
    buf: &[u8],
    max_body: Option<usize>,
) -> Result<Option<(Bytes, usize)>, CodecError> {
    let mut decoded = BytesMut::new();
    let mut pos = 0;

    loop {
        // Find the chunk size line ending
        let remaining = &buf[pos..];
        let crlf_pos = match find_crlf(remaining) {
            Some(p) => p,
            None => return Ok(None), // need more data
        };

        let size_str = std::str::from_utf8(&remaining[..crlf_pos])
            .map_err(|_| CodecError::Invalid("invalid chunk size".into()))?;
        let size_str = size_str.trim();
        let chunk_size = u64::from_str_radix(size_str, 16)
            .map_err(|_| CodecError::Invalid(format!("invalid chunk size: {size_str}")))?
            as usize;

        pos += crlf_pos + 2; // skip past size line + CRLF

        if chunk_size == 0 {
            // Final chunk — expect trailing CRLF
            if buf.len() < pos + 2 {
                return Ok(None);
            }
            pos += 2; // skip final CRLF
            return Ok(Some((decoded.freeze(), pos)));
        }

        // Need chunk_size bytes + CRLF
        if buf.len() < pos + chunk_size + 2 {
            return Ok(None);
        }

        if max_body.is_some_and(|limit| decoded.len() + chunk_size > limit) {
            return Err(CodecError::BodyTooLarge);
        }

        decoded.extend_from_slice(&buf[pos..pos + chunk_size]);
        pos += chunk_size + 2; // skip data + CRLF
    }
}

/// Find the position of the first \r\n in `buf`.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolero::check;
    use proptest::prelude::*;

    #[test]
    fn fuzz_http_request_parsing() {
        check!().for_each(|input: &[u8]| match try_parse_request(input) {
            Ok(parsed) => {
                assert!(parsed.header_len <= input.len());
                assert!(!(parsed.content_length.is_some() && parsed.chunked));
            }
            Err(CodecError::Incomplete | CodecError::Invalid(_)) => {}
            Err(CodecError::BodyTooLarge) => {
                panic!("BodyTooLarge from try_parse_request is unexpected");
            }
        });
    }

    #[test]
    fn fuzz_chunked_decode() {
        check!().for_each(|input: &[u8]| {
            // No limit
            match decode_chunked_with_limit(input, None) {
                Ok(Some((body, consumed))) => {
                    assert!(consumed <= input.len());
                    let _ = body.len();
                }
                Ok(None) => {}
                Err(CodecError::Invalid(_) | CodecError::Incomplete) => {}
                Err(CodecError::BodyTooLarge) => {
                    panic!("BodyTooLarge with no limit is unexpected");
                }
            }
            // With limit
            match decode_chunked_with_limit(input, Some(64)) {
                Ok(Some((body, consumed))) => {
                    assert!(consumed <= input.len());
                    assert!(body.len() <= 64);
                }
                Ok(None) => {}
                Err(CodecError::BodyTooLarge | CodecError::Invalid(_) | CodecError::Incomplete) => {
                }
            }
        });
    }

    #[test]
    fn parse_simple_get() {
        let req = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert_eq!(parsed.method, Method::GET);
        assert_eq!(parsed.uri, "/hello");
        assert_eq!(parsed.version, Version::HTTP_11);
        assert!(parsed.keep_alive);
        assert_eq!(parsed.content_length, None);
        assert!(!parsed.chunked);
        assert_eq!(parsed.header_len, req.len());
    }

    #[test]
    fn parse_post_with_content_length() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert_eq!(parsed.method, Method::POST);
        assert_eq!(parsed.content_length, Some(5));
        assert!(!parsed.chunked);
    }

    #[test]
    fn parse_chunked_transfer_encoding() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(parsed.chunked);
        assert_eq!(parsed.content_length, None);
    }

    #[test]
    fn parse_rejects_content_length_and_chunked() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(msg)) if msg.contains("content-length")
        ));
    }

    #[test]
    fn parse_connection_close() {
        let req = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(!parsed.keep_alive);
    }

    #[test]
    fn parse_http10_default_close() {
        let req = b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(!parsed.keep_alive);
    }

    #[test]
    fn parse_http10_keep_alive() {
        let req = b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(parsed.keep_alive);
    }

    #[test]
    fn parse_incomplete() {
        let req = b"GET /hello HTTP/1.1\r\nHost: loc";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Incomplete)
        ));
    }

    #[test]
    fn parse_invalid() {
        let req = b"INVALID\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn response_head_basic() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());
        let head = write_response_head(http::StatusCode::OK, &headers, false);
        let head_str = String::from_utf8(head).unwrap();
        assert!(head_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head_str.contains("content-type: text/plain\r\n"));
        assert!(head_str.ends_with("\r\n"));
    }

    #[test]
    fn response_head_chunked() {
        let headers = HeaderMap::new();
        let head = write_response_head(http::StatusCode::OK, &headers, true);
        let head_str = String::from_utf8(head).unwrap();
        assert!(head_str.contains("transfer-encoding: chunked\r\n"));
    }

    #[test]
    fn chunk_encoding() {
        let chunk = encode_chunk(b"hello");
        assert_eq!(chunk, b"5\r\nhello\r\n");
    }

    #[test]
    fn chunk_encoding_empty() {
        // Empty chunk is different from terminator
        let chunk = encode_chunk(b"");
        assert_eq!(chunk, b"0\r\n\r\n");
    }

    #[test]
    fn decode_chunked_simple() {
        let data = b"5\r\nhello\r\n0\r\n\r\n";
        let (body, consumed) = decode_chunked_with_limit(data, None).unwrap().unwrap();
        assert_eq!(body, Bytes::from("hello"));
        assert_eq!(consumed, data.len());
    }

    #[test]
    fn decode_chunked_multi() {
        let data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (body, consumed) = decode_chunked_with_limit(data, None).unwrap().unwrap();
        assert_eq!(body, Bytes::from("hello world"));
        assert_eq!(consumed, data.len());
    }

    #[test]
    fn decode_chunked_incomplete() {
        let data = b"5\r\nhel";
        assert!(decode_chunked_with_limit(data, None).unwrap().is_none());
    }

    #[test]
    fn decode_chunked_respects_max_body() {
        let data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert!(matches!(
            decode_chunked_with_limit(data, Some(8)),
            Err(CodecError::BodyTooLarge)
        ));
    }

    /// Generate a valid HTTP method.
    fn arb_method() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("GET"),
            Just("POST"),
            Just("PUT"),
            Just("DELETE"),
            Just("PATCH"),
            Just("HEAD"),
            Just("OPTIONS"),
        ]
    }

    /// Generate a valid URI path.
    fn arb_path() -> impl Strategy<Value = String> {
        prop::collection::vec("[a-zA-Z0-9._~-]{1,20}", 1..=5)
            .prop_map(|segs| format!("/{}", segs.join("/")))
    }

    /// Generate a valid header name (lowercase alpha + hyphens).
    fn arb_header_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,19}".prop_filter("no empty", |s| !s.is_empty())
    }

    /// Generate a valid header value (visible ASCII, no CR/LF).
    fn arb_header_value() -> impl Strategy<Value = String> {
        "[!-~]{1,50}"
    }

    proptest! {
        /// Valid HTTP requests always parse successfully and round-trip method/path.
        #[test]
        fn proptest_valid_request_parses(
            method in arb_method(),
            path in arb_path(),
            headers in prop::collection::vec(
                (arb_header_name(), arb_header_value()),
                0..=8
            ),
        ) {
            let mut raw = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\n");
            for (name, value) in &headers {
                // Skip headers that conflict with codec logic.
                if ["content-length", "transfer-encoding", "connection", "expect"]
                    .contains(&name.as_str())
                {
                    continue;
                }
                raw.push_str(&format!("{name}: {value}\r\n"));
            }
            raw.push_str("\r\n");

            let parsed = try_parse_request(raw.as_bytes()).unwrap();
            prop_assert_eq!(parsed.method.as_str(), method);
            prop_assert_eq!(parsed.uri.path(), path.as_str());
            prop_assert_eq!(parsed.version, Version::HTTP_11);
            prop_assert_eq!(parsed.header_len, raw.len());
        }

        /// Truncated requests always return Incomplete, never panic.
        #[test]
        fn proptest_truncated_never_panics(
            method in arb_method(),
            path in arb_path(),
            cut in 1usize..100,
        ) {
            let raw = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let truncated = &raw.as_bytes()[..cut.min(raw.len() - 1)];
            match try_parse_request(truncated) {
                Err(CodecError::Incomplete) => {} // expected
                Err(CodecError::Invalid(_)) => {} // also acceptable for badly cut data
                Ok(_) => {} // subset might be a valid request on its own
                Err(CodecError::BodyTooLarge) => {
                    panic!("BodyTooLarge from header parsing is unexpected");
                }
            }
        }

        /// Arbitrary bytes never panic the parser.
        #[test]
        fn proptest_arbitrary_bytes_never_panic(data in prop::collection::vec(any::<u8>(), 0..=1024)) {
            let _ = try_parse_request(&data);
        }

        /// Chunked encode/decode round-trips for arbitrary data.
        #[test]
        fn proptest_chunked_roundtrip(chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..=256), 1..=4)) {
            let mut encoded = Vec::new();
            let mut expected = Vec::new();
            for chunk in &chunks {
                encoded.extend_from_slice(&encode_chunk(chunk));
                expected.extend_from_slice(chunk);
            }
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            let (decoded, consumed) = decode_chunked_with_limit(&encoded, None)
                .unwrap()
                .unwrap();
            prop_assert_eq!(decoded.as_ref(), expected.as_slice());
            prop_assert_eq!(consumed, encoded.len());
        }

        /// Chunked body limit is enforced: decoded body exceeding limit returns BodyTooLarge.
        #[test]
        fn proptest_chunked_limit_enforced(
            data in prop::collection::vec(any::<u8>(), 10..=200),
            limit in 1usize..=9,
        ) {
            let mut encoded = encode_chunk(&data);
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            match decode_chunked_with_limit(&encoded, Some(limit)) {
                Err(CodecError::BodyTooLarge) => {} // expected
                other => prop_assert!(false, "expected BodyTooLarge, got {:?}", other.map(|o| o.map(|(b, c)| (b.len(), c)))),
            }
        }

        /// HTTP/1.0 defaults to close, HTTP/1.1 defaults to keep-alive.
        #[test]
        fn proptest_keep_alive_version_default(version in prop_oneof![Just(0u8), Just(1u8)]) {
            let raw = format!(
                "GET / HTTP/1.{version}\r\nHost: localhost\r\n\r\n"
            );
            let parsed = try_parse_request(raw.as_bytes()).unwrap();
            if version == 1 {
                prop_assert!(parsed.keep_alive);
            } else {
                prop_assert!(!parsed.keep_alive);
            }
        }

        /// Connection: close always disables keep-alive regardless of version.
        #[test]
        fn proptest_connection_close_overrides(version in prop_oneof![Just(0u8), Just(1u8)]) {
            let raw = format!(
                "GET / HTTP/1.{version}\r\nConnection: close\r\n\r\n"
            );
            let parsed = try_parse_request(raw.as_bytes()).unwrap();
            prop_assert!(!parsed.keep_alive);
        }

        /// Response head always starts with "HTTP/1.1 {status}" and ends with "\r\n".
        #[test]
        fn proptest_response_head_format(
            status_code in 200u16..=599,
            n_headers in 0usize..=5,
            chunked in any::<bool>(),
        ) {
            let status = http::StatusCode::from_u16(status_code).unwrap();
            let mut headers = HeaderMap::new();
            for i in 0..n_headers {
                let name: http::header::HeaderName = format!("x-test-{i}").parse().unwrap();
                headers.insert(name, "value".parse().unwrap());
            }
            let head = write_response_head(status, &headers, chunked);
            let head_str = std::str::from_utf8(&head).unwrap();

            let expected_start = format!("HTTP/1.1 {}", status.as_str());
            prop_assert!(head_str.starts_with(&expected_start));
            prop_assert!(head_str.ends_with("\r\n"));
            if chunked {
                prop_assert!(head_str.contains("transfer-encoding: chunked\r\n"));
            }
            for i in 0..n_headers {
                let expected_hdr = format!("x-test-{}: value\r\n", i);
                prop_assert!(head_str.contains(&expected_hdr));
            }
        }
    }
}
