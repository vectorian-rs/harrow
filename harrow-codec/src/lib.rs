//! HTTP/1.1 codec for Harrow.
//!
//! Parses HTTP/1.1 request headers via [`httparse`] and serializes
//! response status lines, headers, and chunked transfer-encoding.
//!
//! This crate is runtime-agnostic — no tokio, no async. It operates
//! on byte slices and [`BytesMut`] buffers.
//!
//! # Stateful decoding
//!
//! [`PayloadDecoder`] tracks its position across calls, so incremental
//! recv completions are O(n) total — no re-scanning from the start.
//!
//! # Buffer pool
//!
//! [`BufPool`] provides thread-local buffer reuse to eliminate
//! per-request allocations.

pub mod buf_pool;
pub use buf_pool::BufPool;

use std::io::Write as _;
use std::task::Poll;

use bytes::{Buf, Bytes, BytesMut};
use http::header::{CONNECTION, CONTENT_LENGTH, EXPECT, TRANSFER_ENCODING};
use http::{HeaderMap, Method, StatusCode, Uri, Version};

pub const MAX_HEADERS: usize = 100;
pub const MAX_HEADER_BUF: usize = 64 * 1024;
pub const DEFAULT_BUFFER_SIZE: usize = 8192;
pub const CHUNK_TERMINATOR: &[u8] = b"0\r\n\r\n";
pub const CONTINUE_100: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ParsedRequest {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub header_len: usize,
    pub content_length: Option<u64>,
    pub chunked: bool,
    pub keep_alive: bool,
    pub expect_continue: bool,
}

#[derive(Debug)]
pub enum CodecError {
    Incomplete,
    BodyTooLarge,
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
/// On success, the caller should `buf.advance(parsed.header_len)` to
/// consume the header bytes. Any remaining bytes are body data.
pub fn try_parse_request(buf: &[u8]) -> Result<ParsedRequest, CodecError> {
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
    let mut seen_te = false;
    let mut conn_close = false;
    let mut conn_keep_alive = false;
    let mut expect_continue = false;

    for h in parsed.headers.iter() {
        let name = http::header::HeaderName::from_bytes(h.name.as_bytes())
            .map_err(|e| CodecError::Invalid(e.to_string()))?;
        let value = http::header::HeaderValue::from_bytes(h.value)
            .map_err(|e| CodecError::Invalid(e.to_string()))?;

        if name == CONTENT_LENGTH {
            let s = std::str::from_utf8(h.value)
                .map_err(|_| CodecError::Invalid("invalid content-length encoding".into()))?;
            let len: u64 = s
                .trim()
                .parse()
                .map_err(|_| CodecError::Invalid("invalid content-length value".into()))?;
            if content_length.is_some_and(|prev| prev != len) {
                return Err(CodecError::Invalid(
                    "conflicting content-length values".into(),
                ));
            }
            content_length = Some(len);
        } else if name == TRANSFER_ENCODING {
            // Reject duplicate TE headers (request smuggling vector).
            if seen_te {
                return Err(CodecError::Invalid(
                    "duplicate transfer-encoding header".into(),
                ));
            }
            seen_te = true;
            // TE is only valid on HTTP/1.1 (HTTP/1.0 has no chunked encoding).
            if version == Version::HTTP_11
                && let Ok(s) = std::str::from_utf8(h.value)
            {
                for token in s.split(',') {
                    if token.trim().eq_ignore_ascii_case("chunked") {
                        chunked = true;
                    }
                }
            }
        } else if name == CONNECTION {
            if let Ok(s) = std::str::from_utf8(h.value) {
                for token in s.split(',') {
                    let token = token.trim();
                    if token.eq_ignore_ascii_case("close") {
                        conn_close = true;
                    }
                    if token.eq_ignore_ascii_case("keep-alive") {
                        conn_keep_alive = true;
                    }
                }
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

    // HTTP/1.0 POST without Content-Length has indeterminate body length.
    // Reject to prevent request smuggling (RFC 1945 §7.2.2).
    if version == Version::HTTP_10
        && (method == Method::POST || method == Method::PUT)
        && content_length.is_none()
    {
        return Err(CodecError::Invalid(
            "HTTP/1.0 POST/PUT requires content-length".into(),
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

// ---------------------------------------------------------------------------
// Stateful payload decoder (ntex model)
// ---------------------------------------------------------------------------

/// Decoded payload item returned by [`PayloadDecoder::decode`].
#[derive(Debug)]
pub enum PayloadItem {
    /// A chunk of body data (zero-copy view into the read buffer).
    Chunk(Bytes),
    /// End of payload.
    Eof,
}

/// Stateful payload decoder for Content-Length and chunked bodies.
///
/// Tracks its position across calls so incremental recv completions
/// are O(n) total. Operates on `&mut BytesMut` in place — consumed
/// bytes are removed from the buffer via `split_to`.
#[derive(Debug)]
pub struct PayloadDecoder {
    kind: PayloadKind,
}

#[derive(Debug, Clone, Copy)]
enum PayloadKind {
    Length(u64),
    /// (state, current_chunk_remaining, cumulative_decoded_bytes)
    Chunked(ChunkedState, u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkedState {
    Size,
    SizeLws,
    Extension,
    SizeLf,
    Body,
    BodyCr,
    BodyLf,
    EndCr,
    EndLf,
    End,
}

impl PayloadDecoder {
    /// Create a decoder for a Content-Length body.
    pub fn length(len: u64) -> Self {
        Self {
            kind: PayloadKind::Length(len),
        }
    }

    /// Create a decoder for a chunked transfer-encoded body.
    pub fn chunked() -> Self {
        Self {
            kind: PayloadKind::Chunked(ChunkedState::Size, 0, 0),
        }
    }

    /// Create the appropriate decoder from a [`ParsedRequest`].
    ///
    /// Returns `None` if the request has no body.
    pub fn from_parsed(parsed: &ParsedRequest) -> Option<Self> {
        if parsed.chunked {
            Some(Self::chunked())
        } else if let Some(len) = parsed.content_length {
            if len > 0 {
                Some(Self::length(len))
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Returns true when the decoder has reached the end of the payload.
    pub fn is_eof(&self) -> bool {
        matches!(
            self.kind,
            PayloadKind::Length(0) | PayloadKind::Chunked(ChunkedState::End, _, _)
        )
    }

    /// Decode the next payload item from the buffer.
    ///
    /// Consumes processed bytes from `src` via `split_to`. Returns:
    /// - `Ok(Some(Chunk(bytes)))` — a piece of body data
    /// - `Ok(Some(Eof))` — the body is complete
    /// - `Ok(None)` — need more data
    /// - `Err(CodecError)` — invalid input
    pub fn decode(
        &mut self,
        src: &mut BytesMut,
        max_body: Option<usize>,
    ) -> Result<Option<PayloadItem>, CodecError> {
        match &mut self.kind {
            PayloadKind::Length(remaining) => {
                if *remaining == 0 {
                    return Ok(Some(PayloadItem::Eof));
                }
                if src.is_empty() {
                    return Ok(None);
                }
                let len = src.len() as u64;
                let chunk = if *remaining > len {
                    *remaining -= len;
                    src.split_to(src.len())
                } else {
                    let n = *remaining as usize;
                    *remaining = 0;
                    src.split_to(n)
                };
                Ok(Some(PayloadItem::Chunk(chunk.freeze())))
            }
            PayloadKind::Chunked(state, size, decoded_total) => loop {
                let mut chunk = None;
                match state.step(src, size, &mut chunk) {
                    Poll::Pending => return Ok(None),
                    Poll::Ready(Err(msg)) => {
                        return Err(CodecError::Invalid(msg.into()));
                    }
                    Poll::Ready(Ok(next)) => *state = next,
                }

                if *state == ChunkedState::End {
                    return Ok(Some(PayloadItem::Eof));
                }

                if let Some(body_chunk) = chunk {
                    *decoded_total += body_chunk.len() as u64;
                    if let Some(limit) = max_body
                        && *decoded_total > limit as u64
                    {
                        return Err(CodecError::BodyTooLarge);
                    }
                    return Ok(Some(PayloadItem::Chunk(body_chunk)));
                }

                if src.is_empty() {
                    return Ok(None);
                }
            },
        }
    }
}

impl ChunkedState {
    fn step(
        &self,
        src: &mut BytesMut,
        size: &mut u64,
        chunk: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, &'static str>> {
        match self {
            ChunkedState::Size => Self::read_size(src, size),
            ChunkedState::SizeLws => Self::read_size_lws(src),
            ChunkedState::Extension => Self::read_extension(src),
            ChunkedState::SizeLf => Self::read_size_lf(src, size),
            ChunkedState::Body => Self::read_body(src, size, chunk),
            ChunkedState::BodyCr => Self::expect_byte(
                src,
                b'\r',
                ChunkedState::BodyLf,
                "expected CR after chunk body",
            ),
            ChunkedState::BodyLf => Self::expect_byte(
                src,
                b'\n',
                ChunkedState::Size,
                "expected LF after chunk body",
            ),
            ChunkedState::EndCr => Self::expect_byte(
                src,
                b'\r',
                ChunkedState::EndLf,
                "expected CR at end of chunked stream",
            ),
            ChunkedState::EndLf => Self::expect_byte(
                src,
                b'\n',
                ChunkedState::End,
                "expected LF at end of chunked stream",
            ),
            ChunkedState::End => Poll::Ready(Ok(ChunkedState::End)),
        }
    }

    fn read_size(src: &mut BytesMut, size: &mut u64) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let b = src[0];
        src.advance(1);

        let rem = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b + 10 - b'a',
            b'A'..=b'F' => b + 10 - b'A',
            b'\t' | b' ' => return Poll::Ready(Ok(ChunkedState::SizeLws)),
            b';' => return Poll::Ready(Ok(ChunkedState::Extension)),
            b'\r' => return Poll::Ready(Ok(ChunkedState::SizeLf)),
            _ => return Poll::Ready(Err("invalid chunk size character")),
        };

        match size.checked_mul(16) {
            Some(n) => {
                *size = n + u64::from(rem);
                Poll::Ready(Ok(ChunkedState::Size))
            }
            None => Poll::Ready(Err("chunk size overflow")),
        }
    }

    fn read_size_lws(src: &mut BytesMut) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let b = src[0];
        src.advance(1);
        match b {
            b'\t' | b' ' => Poll::Ready(Ok(ChunkedState::SizeLws)),
            b';' => Poll::Ready(Ok(ChunkedState::Extension)),
            b'\r' => Poll::Ready(Ok(ChunkedState::SizeLf)),
            _ => Poll::Ready(Err("invalid chunk size whitespace")),
        }
    }

    fn read_extension(src: &mut BytesMut) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let b = src[0];
        src.advance(1);
        match b {
            b'\r' => Poll::Ready(Ok(ChunkedState::SizeLf)),
            0x00..=0x08 | 0x0a..=0x1f | 0x7f => Poll::Ready(Err("invalid chunk extension")),
            _ => Poll::Ready(Ok(ChunkedState::Extension)),
        }
    }

    fn read_size_lf(
        src: &mut BytesMut,
        size: &mut u64,
    ) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let b = src[0];
        src.advance(1);
        match b {
            b'\n' if *size > 0 => Poll::Ready(Ok(ChunkedState::Body)),
            b'\n' => Poll::Ready(Ok(ChunkedState::EndCr)),
            _ => Poll::Ready(Err("expected LF after chunk size")),
        }
    }

    fn read_body(
        src: &mut BytesMut,
        remaining: &mut u64,
        chunk: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let len = src.len() as u64;
        let slice = if *remaining > len {
            *remaining -= len;
            src.split_to(src.len())
        } else {
            let n = *remaining as usize;
            *remaining = 0;
            src.split_to(n)
        };
        *chunk = Some(slice.freeze());
        if *remaining > 0 {
            Poll::Ready(Ok(ChunkedState::Body))
        } else {
            Poll::Ready(Ok(ChunkedState::BodyCr))
        }
    }

    fn expect_byte(
        src: &mut BytesMut,
        expected: u8,
        next: ChunkedState,
        err: &'static str,
    ) -> Poll<Result<ChunkedState, &'static str>> {
        if src.is_empty() {
            return Poll::Pending;
        }
        let b = src[0];
        src.advance(1);
        if b == expected {
            Poll::Ready(Ok(next))
        } else {
            Poll::Ready(Err(err))
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy stateless decoder (kept for backward compatibility)
// ---------------------------------------------------------------------------

/// Decode chunked transfer-encoding in a single pass.
///
/// Prefer [`PayloadDecoder::chunked`] for incremental decoding.
pub fn decode_chunked_with_limit(
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
        let chunk_size_u64 = u64::from_str_radix(size_str, 16)
            .map_err(|_| CodecError::Invalid(format!("invalid chunk size: {size_str}")))?;
        let chunk_size = usize::try_from(chunk_size_u64)
            .map_err(|_| CodecError::Invalid("chunk size too large".into()))?;

        pos += crlf_pos + 2;

        if chunk_size == 0 {
            if buf.len() < pos + 2 {
                return Ok(None);
            }
            pos += 2;
            return Ok(Some((decoded.freeze(), pos)));
        }

        let needed = pos.saturating_add(chunk_size).saturating_add(2);
        if buf.len() < needed {
            return Ok(None);
        }

        if max_body.is_some_and(|limit| decoded.len().saturating_add(chunk_size) > limit) {
            return Err(CodecError::BodyTooLarge);
        }

        decoded.extend_from_slice(&buf[pos..pos + chunk_size]);
        pos += chunk_size + 2;
    }
}

// ---------------------------------------------------------------------------
// Response serialization
// ---------------------------------------------------------------------------

/// Write the HTTP response status line + headers into a caller-provided buffer.
pub fn write_response_head_into(
    status: StatusCode,
    headers: &HeaderMap,
    chunked: bool,
    buf: &mut Vec<u8>,
) {
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
}

/// Write the HTTP response status line + headers, returning a new buffer.
pub fn write_response_head(status: StatusCode, headers: &HeaderMap, chunked: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    write_response_head_into(status, headers, chunked, &mut buf);
    buf
}

/// Encode a single chunk into a caller-provided buffer.
pub fn encode_chunk_into(data: &[u8], buf: &mut Vec<u8>) {
    let _ = write!(buf, "{:x}", data.len());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(data);
    buf.extend_from_slice(b"\r\n");
}

/// Encode a single chunk, returning a new buffer.
pub fn encode_chunk(data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(data.len() + 20);
    encode_chunk_into(data, &mut buf);
    buf
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    let mut start = 0;
    while start < buf.len() {
        match memchr::memchr(b'\r', &buf[start..]) {
            Some(pos) => {
                let abs = start + pos;
                if abs + 1 < buf.len() && buf[abs + 1] == b'\n' {
                    return Some(abs);
                }
                start = abs + 1;
            }
            None => return None,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        assert!(!parsed.expect_continue);
        assert_eq!(parsed.header_len, req.len());
    }

    #[test]
    fn parse_post_with_content_length() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = try_parse_request(req).unwrap();
        assert_eq!(parsed.method, Method::POST);
        assert_eq!(parsed.content_length, Some(5));
    }

    #[test]
    fn parse_chunked() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(parsed.chunked);
        assert_eq!(parsed.content_length, None);
    }

    #[test]
    fn reject_content_length_and_chunked() {
        let req = b"POST /data HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn reject_invalid_content_length() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: abc\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn reject_conflicting_content_lengths() {
        let req = b"POST /data HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn accept_duplicate_same_content_length() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = try_parse_request(req).unwrap();
        assert_eq!(parsed.content_length, Some(5));
    }

    #[test]
    fn incomplete_request() {
        assert!(matches!(
            try_parse_request(b"GET /hello HTTP/1.1\r\nHost: loc"),
            Err(CodecError::Incomplete)
        ));
    }

    #[test]
    fn keep_alive_http10_close() {
        let req = b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";
        assert!(!try_parse_request(req).unwrap().keep_alive);
    }

    #[test]
    fn keep_alive_http10_explicit() {
        let req = b"GET / HTTP/1.0\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        assert!(try_parse_request(req).unwrap().keep_alive);
    }

    #[test]
    fn connection_close_http11() {
        let req = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert!(!try_parse_request(req).unwrap().keep_alive);
    }

    #[test]
    fn expect_continue() {
        let req = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 1024\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(parsed.expect_continue);
        assert_eq!(parsed.content_length, Some(1024));
    }

    #[test]
    fn chunked_in_comma_list() {
        let req =
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        assert!(try_parse_request(req).unwrap().chunked);
    }

    #[test]
    fn connection_tokens_comma_separated() {
        let req = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive, upgrade\r\n\r\n";
        assert!(try_parse_request(req).unwrap().keep_alive);
    }

    // --- Security: request smuggling prevention ---

    #[test]
    fn reject_duplicate_transfer_encoding() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn ignore_transfer_encoding_on_http10() {
        let req = b"POST /data HTTP/1.0\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello";
        let parsed = try_parse_request(req).unwrap();
        assert!(!parsed.chunked);
        assert_eq!(parsed.content_length, Some(5));
    }

    #[test]
    fn reject_http10_post_without_content_length() {
        let req = b"POST /data HTTP/1.0\r\nHost: localhost\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn reject_http10_put_without_content_length() {
        let req = b"PUT /data HTTP/1.0\r\nHost: localhost\r\n\r\n";
        assert!(matches!(
            try_parse_request(req),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn allow_http10_get_without_content_length() {
        let req = b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";
        try_parse_request(req).unwrap();
    }

    #[test]
    fn payload_chunked_cumulative_max_body() {
        // 3 chunks of 5 bytes each = 15 bytes total, limit is 10
        let mut buf = BytesMut::from(&b"5\r\nhello\r\n5\r\nworld\r\n5\r\nagain\r\n0\r\n\r\n"[..]);
        let mut dec = PayloadDecoder::chunked();

        // First chunk (5 bytes) — OK
        match dec.decode(&mut buf, Some(10)).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.len(), 5),
            _ => panic!("expected Chunk"),
        }
        // Second chunk (cumulative 10 bytes) — OK
        match dec.decode(&mut buf, Some(10)).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.len(), 5),
            _ => panic!("expected Chunk"),
        }
        // Third chunk (cumulative 15 bytes) — exceeds limit
        assert!(matches!(
            dec.decode(&mut buf, Some(10)),
            Err(CodecError::BodyTooLarge)
        ));
    }

    // --- PayloadDecoder: Content-Length ---

    #[test]
    fn payload_length_exact() {
        let mut buf = BytesMut::from(&b"hello"[..]);
        let mut dec = PayloadDecoder::length(5);
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"hello"),
            _ => panic!("expected Chunk"),
        }
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Eof => {}
            _ => panic!("expected Eof"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn payload_length_incremental() {
        let mut dec = PayloadDecoder::length(5);

        let mut buf = BytesMut::from(&b"hel"[..]);
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"hel"),
            _ => panic!("expected Chunk"),
        }
        assert!(buf.is_empty());

        buf.extend_from_slice(b"lo");
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"lo"),
            _ => panic!("expected Chunk"),
        }
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Eof => {}
            _ => panic!("expected Eof"),
        }
    }

    #[test]
    fn payload_length_with_trailing_data() {
        let mut buf = BytesMut::from(&b"helloGET /next"[..]);
        let mut dec = PayloadDecoder::length(5);
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"hello"),
            _ => panic!("expected Chunk"),
        }
        // Trailing pipelined data preserved
        assert_eq!(buf.as_ref(), b"GET /next");
    }

    // --- PayloadDecoder: Chunked ---

    #[test]
    fn payload_chunked_single() {
        let mut buf = BytesMut::from(&b"5\r\nhello\r\n0\r\n\r\n"[..]);
        let mut dec = PayloadDecoder::chunked();

        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"hello"),
            _ => panic!("expected Chunk"),
        }
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Eof => {}
            _ => panic!("expected Eof"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn payload_chunked_multi() {
        let mut buf = BytesMut::from(&b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"[..]);
        let mut dec = PayloadDecoder::chunked();

        let mut body = Vec::new();
        loop {
            match dec.decode(&mut buf, None).unwrap() {
                Some(PayloadItem::Chunk(c)) => body.extend_from_slice(&c),
                Some(PayloadItem::Eof) => break,
                None => panic!("unexpected incomplete"),
            }
        }
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn payload_chunked_incremental() {
        let mut dec = PayloadDecoder::chunked();
        let mut body = Vec::new();

        // First recv: partial chunk size
        let mut buf = BytesMut::from(&b"5\r\nhel"[..]);
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => body.extend_from_slice(&c),
            _ => panic!("expected partial Chunk"),
        }
        // Decoder consumed what it could, needs more
        assert!(dec.decode(&mut buf, None).unwrap().is_none());

        // Second recv: rest of chunk + terminator
        buf.extend_from_slice(b"lo\r\n0\r\n\r\n");
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => body.extend_from_slice(&c),
            _ => panic!("expected Chunk"),
        }
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Eof => {}
            _ => panic!("expected Eof"),
        }
        assert_eq!(body, b"hello");
    }

    #[test]
    fn payload_chunked_with_extensions() {
        let mut buf = BytesMut::from(&b"5;ext=val\r\nhello\r\n0\r\n\r\n"[..]);
        let mut dec = PayloadDecoder::chunked();
        match dec.decode(&mut buf, None).unwrap().unwrap() {
            PayloadItem::Chunk(c) => assert_eq!(c.as_ref(), b"hello"),
            _ => panic!("expected Chunk"),
        }
    }

    #[test]
    fn payload_from_parsed_chunked() {
        let req = b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        let dec = PayloadDecoder::from_parsed(&parsed);
        assert!(dec.is_some());
    }

    #[test]
    fn payload_from_parsed_no_body() {
        let req = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let parsed = try_parse_request(req).unwrap();
        assert!(PayloadDecoder::from_parsed(&parsed).is_none());
    }

    // --- Legacy stateless decoder ---

    #[test]
    fn decode_chunked_complete() {
        let data = b"5\r\nhello\r\n0\r\n\r\n";
        let result = decode_chunked_with_limit(data, None).unwrap().unwrap();
        assert_eq!(result.0.as_ref(), b"hello");
        assert_eq!(result.1, data.len());
    }

    #[test]
    fn decode_chunked_incomplete() {
        assert!(
            decode_chunked_with_limit(b"5\r\nhel", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn decode_chunked_too_large() {
        assert!(matches!(
            decode_chunked_with_limit(b"5\r\nhello\r\n0\r\n\r\n", Some(3)),
            Err(CodecError::BodyTooLarge)
        ));
    }

    #[test]
    fn decode_chunked_multi_chunk() {
        let data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let result = decode_chunked_with_limit(data, None).unwrap().unwrap();
        assert_eq!(result.0.as_ref(), b"hello world");
    }

    // --- Response / chunk encoding ---

    #[test]
    fn write_response_head_basic() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());
        let head = write_response_head(StatusCode::OK, &headers, false);
        let s = String::from_utf8(head).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("content-type: text/plain\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn write_response_head_chunked() {
        let head = write_response_head(StatusCode::OK, &HeaderMap::new(), true);
        let s = String::from_utf8(head).unwrap();
        assert!(s.contains("transfer-encoding: chunked\r\n"));
    }

    #[test]
    fn encode_chunk_basic() {
        assert_eq!(encode_chunk(b"hello"), b"5\r\nhello\r\n");
    }

    #[test]
    fn find_crlf_basic() {
        assert_eq!(find_crlf(b"hello\r\nworld"), Some(5));
        assert_eq!(find_crlf(b"\r\n"), Some(0));
        assert_eq!(find_crlf(b"no crlf here"), None);
        assert_eq!(find_crlf(b"just\r no lf"), None);
        assert_eq!(find_crlf(b""), None);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // --- Chunked encode/decode roundtrip ---

    proptest! {
        #[test]
        fn chunked_roundtrip(body in proptest::collection::vec(any::<u8>(), 1..4096)) {
            let mut encoded = encode_chunk(&body);
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            let result = decode_chunked_with_limit(&encoded, None).unwrap().unwrap();
            prop_assert_eq!(result.0.as_ref(), body.as_slice());
            prop_assert_eq!(result.1, encoded.len());
        }

        #[test]
        fn chunked_roundtrip_stateful(body in proptest::collection::vec(any::<u8>(), 1..4096)) {
            let mut encoded = encode_chunk(&body);
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            // Decode with stateful PayloadDecoder.
            let mut dec = PayloadDecoder::chunked();
            let mut buf = BytesMut::from(encoded.as_slice());
            let mut decoded = Vec::new();

            loop {
                match dec.decode(&mut buf, None).unwrap() {
                    Some(PayloadItem::Chunk(c)) => decoded.extend_from_slice(&c),
                    Some(PayloadItem::Eof) => break,
                    None => prop_assert!(false, "unexpected incomplete"),
                }
            }
            prop_assert_eq!(decoded.as_slice(), body.as_slice());
        }

        #[test]
        fn chunked_multi_chunk_roundtrip(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 1..512),
                1..8
            )
        ) {
            // Encode multiple chunks.
            let mut encoded = Vec::new();
            let mut expected_body = Vec::new();
            for chunk in &chunks {
                encode_chunk_into(chunk, &mut encoded);
                expected_body.extend_from_slice(chunk);
            }
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            // Decode with stateful decoder.
            let mut dec = PayloadDecoder::chunked();
            let mut buf = BytesMut::from(encoded.as_slice());
            let mut decoded = Vec::new();

            loop {
                match dec.decode(&mut buf, None).unwrap() {
                    Some(PayloadItem::Chunk(c)) => decoded.extend_from_slice(&c),
                    Some(PayloadItem::Eof) => break,
                    None => prop_assert!(false, "unexpected incomplete"),
                }
            }
            prop_assert_eq!(decoded.as_slice(), expected_body.as_slice());
        }

        #[test]
        fn chunked_incremental_roundtrip(
            body in proptest::collection::vec(any::<u8>(), 1..2048),
            split in 1usize..100
        ) {
            // Encode as a single chunk + terminator.
            let mut encoded = encode_chunk(&body);
            encoded.extend_from_slice(CHUNK_TERMINATOR);

            // Feed in increments of `split` bytes.
            let mut dec = PayloadDecoder::chunked();
            let mut decoded = Vec::new();
            let mut pos = 0;
            let mut buf = BytesMut::new();

            loop {
                // Feed next chunk of bytes.
                let end = (pos + split).min(encoded.len());
                if end > pos {
                    buf.extend_from_slice(&encoded[pos..end]);
                    pos = end;
                }

                match dec.decode(&mut buf, None).unwrap() {
                    Some(PayloadItem::Chunk(c)) => decoded.extend_from_slice(&c),
                    Some(PayloadItem::Eof) => break,
                    None => {
                        if pos >= encoded.len() {
                            prop_assert!(false, "all data fed but decoder still incomplete");
                        }
                    }
                }
            }
            prop_assert_eq!(decoded.as_slice(), body.as_slice());
        }
    }

    // --- Content-Length decoder roundtrip ---

    proptest! {
        #[test]
        fn content_length_roundtrip(body in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut dec = PayloadDecoder::length(body.len() as u64);
            let mut buf = BytesMut::from(body.as_slice());
            let mut decoded = Vec::new();

            loop {
                match dec.decode(&mut buf, None).unwrap() {
                    Some(PayloadItem::Chunk(c)) => decoded.extend_from_slice(&c),
                    Some(PayloadItem::Eof) => break,
                    None => prop_assert!(false, "unexpected incomplete"),
                }
            }
            prop_assert_eq!(decoded.as_slice(), body.as_slice());
            prop_assert!(buf.is_empty());
        }

        #[test]
        fn content_length_preserves_trailing(
            body in proptest::collection::vec(any::<u8>(), 1..256),
            trailing in proptest::collection::vec(any::<u8>(), 1..256),
        ) {
            let mut full = Vec::new();
            full.extend_from_slice(&body);
            full.extend_from_slice(&trailing);

            let mut dec = PayloadDecoder::length(body.len() as u64);
            let mut buf = BytesMut::from(full.as_slice());
            let mut decoded = Vec::new();

            loop {
                match dec.decode(&mut buf, None).unwrap() {
                    Some(PayloadItem::Chunk(c)) => decoded.extend_from_slice(&c),
                    Some(PayloadItem::Eof) => break,
                    None => prop_assert!(false, "unexpected incomplete"),
                }
            }
            prop_assert_eq!(decoded.as_slice(), body.as_slice());
            prop_assert_eq!(buf.as_ref(), trailing.as_slice());
        }
    }

    // --- Keep-alive logic (RFC 7230 §6.1) ---

    proptest! {
        #[test]
        fn keep_alive_http11_default(
            path in "/[a-z]{1,20}",
        ) {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let parsed = try_parse_request(req.as_bytes()).unwrap();
            prop_assert!(parsed.keep_alive, "HTTP/1.1 defaults to keep-alive");
        }

        #[test]
        fn keep_alive_http10_default(
            path in "/[a-z]{1,20}",
        ) {
            let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
            let parsed = try_parse_request(req.as_bytes()).unwrap();
            prop_assert!(!parsed.keep_alive, "HTTP/1.0 defaults to close");
        }
    }

    // --- Valid request parse never panics ---

    fn valid_method() -> impl Strategy<Value = &'static str> {
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

    proptest! {
        #[test]
        fn valid_request_parses(
            method in valid_method(),
            path in "/[a-z0-9/_-]{1,50}",
            host in "[a-z]{3,15}\\.[a-z]{2,5}",
        ) {
            let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n\r\n");
            let parsed = try_parse_request(req.as_bytes()).unwrap();
            prop_assert_eq!(parsed.method.as_str(), method);
            prop_assert_eq!(parsed.uri.path(), path.as_str());
            prop_assert_eq!(parsed.version, Version::HTTP_11);
        }

        #[test]
        fn valid_post_with_content_length(
            path in "/[a-z]{1,20}",
            body in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let req = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let mut full = req.into_bytes();
            full.extend_from_slice(&body);

            let parsed = try_parse_request(&full).unwrap();
            prop_assert_eq!(parsed.method, Method::POST);
            prop_assert_eq!(parsed.content_length, Some(body.len() as u64));
        }
    }
}
