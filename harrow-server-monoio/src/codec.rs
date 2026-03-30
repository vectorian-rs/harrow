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
}
