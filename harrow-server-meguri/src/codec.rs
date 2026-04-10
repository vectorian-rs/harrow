//! HTTP/1.1 codec for the meguri server.
//!
//! Copied from harrow-server-monoio/src/codec.rs.
//! Parses HTTP/1.1 request headers and serializes responses.

use bytes::{Bytes, BytesMut};
use http::header::{CONNECTION, CONTENT_LENGTH, EXPECT, TRANSFER_ENCODING};
use http::{HeaderMap, Method, Uri, Version};

/// Maximum number of headers we parse per request.
const MAX_HEADERS: usize = 100;

/// Maximum size of the header read buffer (64 KiB).
pub(crate) const MAX_HEADER_BUF: usize = 64 * 1024;

/// Default read buffer size.
pub(crate) const DEFAULT_BUFFER_SIZE: usize = 8192;

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
    #[allow(dead_code)]
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

fn should_keep_alive(version: Version, conn_close: bool, conn_keep_alive: bool) -> bool {
    if conn_close {
        return false;
    }
    if conn_keep_alive {
        return true;
    }
    version == Version::HTTP_11
}

/// Write the HTTP response status line + headers into a buffer.
pub(crate) fn write_response_head(
    status: http::StatusCode,
    headers: &HeaderMap,
    chunked: bool,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"HTTP/1.1 ");
    buf.extend_from_slice(status.as_str().as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(status.canonical_reason().unwrap_or("").as_bytes());
    buf.extend_from_slice(b"\r\n");

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
#[allow(dead_code)]
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
        let remaining = &buf[pos..];
        let crlf_pos = match find_crlf(remaining) {
            Some(p) => p,
            None => return Ok(None),
        };

        let size_str = std::str::from_utf8(&remaining[..crlf_pos])
            .map_err(|_| CodecError::Invalid("invalid chunk size".into()))?;
        let size_str = size_str.trim();
        let chunk_size = u64::from_str_radix(size_str, 16)
            .map_err(|_| CodecError::Invalid(format!("invalid chunk size: {size_str}")))?
            as usize;

        pos += crlf_pos + 2;

        if chunk_size == 0 {
            if buf.len() < pos + 2 {
                return Ok(None);
            }
            pos += 2;
            return Ok(Some((decoded.freeze(), pos)));
        }

        if buf.len() < pos + chunk_size + 2 {
            return Ok(None);
        }

        if max_body.is_some_and(|limit| decoded.len() + chunk_size > limit) {
            return Err(CodecError::BodyTooLarge);
        }

        decoded.extend_from_slice(&buf[pos..pos + chunk_size]);
        pos += chunk_size + 2;
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}
