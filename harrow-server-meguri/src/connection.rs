//! Connection state machine for the meguri server.
//!
//! Each connection tracks its own read buffer, parse state, and response
//! serialization. The main loop drives transitions via CQE completions.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use bytes::{Buf, BytesMut};
use http_body_util::BodyExt;

use crate::codec::{self, CodecError, ParsedRequest};

/// Connection states in the lifecycle.
#[derive(Debug)]
pub(crate) enum ConnState {
    /// Waiting for header bytes to arrive via RECV completion.
    Headers,
    /// Headers parsed; waiting for body bytes.
    Body {
        content_length: Option<u64>,
        chunked: bool,
    },
    /// Dispatching request through Harrow pipeline (blocking).
    Dispatching,
    /// Writing serialized response bytes to the socket.
    Writing,
    /// Connection closed; waiting for removal from slab.
    #[allow(dead_code)]
    Closed,
}

/// Per-connection state.
pub(crate) struct Conn {
    pub fd: RawFd,
    pub state: ConnState,
    /// Read buffer: holds raw bytes from RECV completions.
    pub buf: BytesMut,
    /// Parsed request headers (set after Headers -> Body/Dispatching transition).
    pub parsed: Option<ParsedRequest>,
    /// Body bytes collected so far (for Content-Length bodies).
    pub body_bytes: BytesMut,
    /// Serialized response bytes to write.
    pub response_buf: Vec<u8>,
    /// Number of response bytes already written.
    pub response_written: usize,
    /// Whether to keep-alive after this request.
    pub keep_alive: bool,
    /// Whether there is a pending RECV SQE for this connection.
    pub recv_pending: bool,
    /// Whether there is a pending WRITE SQE for this connection.
    pub write_pending: bool,
    /// When this connection was accepted.
    pub accepted_at: Instant,
    /// When the current request started (reset on keep-alive).
    pub request_started_at: Instant,
}

/// Result of processing a RECV completion on a connection.
pub(crate) enum ProcessResult {
    /// Submit a RECV SQE (need more data).
    NeedRecv,
    /// Dispatch the request through Harrow.
    Dispatch,
    /// Write a serialized error response.
    WriteError(Vec<u8>),
    /// Connection should be closed (clean close or error).
    Close,
}

/// Result of processing a WRITE completion.
pub(crate) enum WriteResult {
    /// Submit another WRITE SQE (more bytes to send).
    WriteMore,
    /// Submit a RECV SQE (keep-alive, start next request).
    RecvNext,
    /// Connection should be closed.
    Close,
}

impl Conn {
    pub fn new(fd: RawFd) -> Self {
        let now = Instant::now();
        Self {
            fd,
            state: ConnState::Headers,
            buf: BytesMut::with_capacity(codec::DEFAULT_BUFFER_SIZE),
            parsed: None,
            body_bytes: BytesMut::new(),
            response_buf: Vec::new(),
            response_written: 0,
            keep_alive: true,
            recv_pending: false,
            write_pending: false,
            accepted_at: now,
            request_started_at: now,
        }
    }

    /// Process bytes from a RECV completion. Returns the next action.
    pub fn on_recv(&mut self, nbytes: usize, max_body: usize) -> ProcessResult {
        if nbytes == 0 {
            return ProcessResult::Close;
        }

        match self.state {
            ConnState::Headers => self.process_headers(max_body),
            ConnState::Body {
                content_length,
                chunked,
            } => self.process_body(content_length, chunked, max_body),
            _ => ProcessResult::Close, // shouldn't get RECV in other states
        }
    }

    fn process_headers(&mut self, max_body: usize) -> ProcessResult {
        loop {
            match codec::try_parse_request(&self.buf) {
                Ok(parsed) => {
                    let header_len = parsed.header_len;
                    let keep_alive = parsed.keep_alive;
                    let content_length = parsed.content_length;
                    let chunked = parsed.chunked;

                    // Remove consumed header bytes.
                    self.buf.advance(header_len);
                    // Any leftover bytes in buf are body data.

                    // Early reject: Content-Length exceeds limit.
                    if max_body > 0
                        && let Some(cl) = content_length
                        && cl as usize > max_body
                    {
                        let resp = error_response(
                            http::StatusCode::PAYLOAD_TOO_LARGE,
                            "payload too large",
                            false,
                        );
                        return ProcessResult::WriteError(resp);
                    }

                    // Check if there is a body to read.
                    let has_body = content_length.is_some_and(|cl| cl > 0) || chunked;
                    if has_body {
                        self.parsed = Some(parsed);
                        self.state = ConnState::Body {
                            content_length,
                            chunked,
                        };
                        return self.process_body(content_length, chunked, max_body);
                    } else {
                        // No body — ready to dispatch.
                        self.parsed = Some(parsed);
                        self.keep_alive = keep_alive;
                        self.state = ConnState::Dispatching;
                        return ProcessResult::Dispatch;
                    }
                }
                Err(CodecError::Incomplete) => {
                    if self.buf.len() >= codec::MAX_HEADER_BUF {
                        let resp = error_response(
                            http::StatusCode::BAD_REQUEST,
                            "request headers too large",
                            false,
                        );
                        return ProcessResult::WriteError(resp);
                    }
                    return ProcessResult::NeedRecv;
                }
                Err(CodecError::Invalid(_)) => {
                    let resp = error_response(http::StatusCode::BAD_REQUEST, "bad request", false);
                    return ProcessResult::WriteError(resp);
                }
                Err(CodecError::BodyTooLarge) => {
                    let resp = error_response(
                        http::StatusCode::PAYLOAD_TOO_LARGE,
                        "payload too large",
                        false,
                    );
                    return ProcessResult::WriteError(resp);
                }
            }
        }
    }

    fn process_body(
        &mut self,
        content_length: Option<u64>,
        chunked: bool,
        max_body: usize,
    ) -> ProcessResult {
        // Move leftover bytes from buf to body_bytes.
        if !self.buf.is_empty() {
            self.body_bytes.extend_from_slice(&self.buf);
            self.buf.clear();
        }

        if chunked {
            match codec::decode_chunked_with_limit(
                &self.body_bytes,
                (max_body > 0).then_some(max_body),
            ) {
                Ok(Some((body, _consumed))) => {
                    self.body_bytes = BytesMut::from(&body[..]);
                    if let Some(ref parsed) = self.parsed {
                        self.keep_alive = parsed.keep_alive;
                    }
                    self.state = ConnState::Dispatching;
                    return ProcessResult::Dispatch;
                }
                Ok(None) => {
                    return ProcessResult::NeedRecv;
                }
                Err(CodecError::BodyTooLarge) => {
                    let resp = error_response(
                        http::StatusCode::PAYLOAD_TOO_LARGE,
                        "payload too large",
                        false,
                    );
                    return ProcessResult::WriteError(resp);
                }
                Err(CodecError::Invalid(_)) => {
                    let resp = error_response(http::StatusCode::BAD_REQUEST, "bad request", false);
                    return ProcessResult::WriteError(resp);
                }
                Err(CodecError::Incomplete) => {
                    return ProcessResult::NeedRecv;
                }
            }
        }

        // Content-Length body.
        let target = match content_length {
            Some(0) | None => 0,
            Some(len) => len as usize,
        };

        if target == 0 || self.body_bytes.len() >= target {
            if let Some(ref parsed) = self.parsed {
                self.keep_alive = parsed.keep_alive;
            }
            self.state = ConnState::Dispatching;
            ProcessResult::Dispatch
        } else {
            ProcessResult::NeedRecv
        }
    }

    /// Process a WRITE completion. Returns the next action.
    pub fn on_write(&mut self, nbytes: usize) -> WriteResult {
        self.response_written += nbytes;

        if self.response_written < self.response_buf.len() {
            WriteResult::WriteMore
        } else if self.keep_alive {
            // Reset for next request.
            self.reset();
            WriteResult::RecvNext
        } else {
            WriteResult::Close
        }
    }

    /// Build a harrow request from the parsed headers and body.
    pub fn build_harrow_request(&self) -> Option<http::Request<harrow_core::request::Body>> {
        let parsed = self.parsed.as_ref()?;

        let mut builder = http::Request::builder()
            .method(&parsed.method)
            .uri(&parsed.uri)
            .version(parsed.version);

        for (name, value) in parsed.headers.iter() {
            builder = builder.header(name, value);
        }

        let body: harrow_core::request::Body = {
            use http_body_util::Full;
            Full::new(self.body_bytes.clone().freeze())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
                .boxed()
        };

        builder.body(body).ok()
    }

    /// Serialize a harrow response into self.response_buf and transition to Writing.
    ///
    /// `body_data` is the pre-collected response body (collected inside the
    /// tokio runtime context during dispatch).
    pub fn set_response(
        &mut self,
        parts: http::response::Parts,
        body_data: Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>,
    ) {
        let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);
        let keep_alive = self.keep_alive;

        let mut head =
            codec::write_response_head(parts.status, &parts.headers, !has_content_length);

        if !keep_alive {
            // Ensure Connection: close is present.
            if !parts.headers.contains_key(http::header::CONNECTION) {
                // Append before final \r\n.
                let close_hdr = b"connection: close\r\n";
                let final_crlf_pos = head.len() - 2;
                head.splice(final_crlf_pos..final_crlf_pos, close_hdr.iter().copied());
            }
        }

        match body_data {
            Ok(data) => {
                if has_content_length {
                    head.extend_from_slice(&data);
                } else if !data.is_empty() {
                    head.extend_from_slice(&codec::encode_chunk(&data));
                    head.extend_from_slice(codec::CHUNK_TERMINATOR);
                } else {
                    head.extend_from_slice(codec::CHUNK_TERMINATOR);
                }
            }
            Err(_) => {
                head.clear();
                head.extend_from_slice(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 21\r\nconnection: close\r\n\r\ninternal server error",
                );
                self.keep_alive = false;
            }
        }

        self.response_buf = head;
        self.response_written = 0;
        self.state = ConnState::Writing;
    }

    /// Reset connection state for the next request (keep-alive).
    fn reset(&mut self) {
        self.state = ConnState::Headers;
        self.parsed = None;
        self.body_bytes.clear();
        self.response_buf.clear();
        self.response_written = 0;
        self.keep_alive = true;
        self.request_started_at = Instant::now();
        // Don't clear buf — leftover bytes from a pipelined request.
    }

    /// Check whether the connection has exceeded its lifetime limit.
    pub fn is_expired(&self, max_lifetime: Option<Duration>) -> bool {
        max_lifetime.is_some_and(|d| self.accepted_at.elapsed() >= d)
    }

    /// Check whether the current request has exceeded the header read timeout.
    pub fn header_timed_out(&self, timeout: Option<Duration>) -> bool {
        matches!(self.state, ConnState::Headers)
            && timeout.is_some_and(|d| self.request_started_at.elapsed() >= d)
    }
}

fn error_response(status: http::StatusCode, body: &'static str, keep_alive: bool) -> Vec<u8> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        http::header::CONTENT_LENGTH,
        body.len().to_string().parse().unwrap(),
    );
    if !keep_alive {
        headers.insert(http::header::CONNECTION, "close".parse().unwrap());
    }

    let mut resp = codec::write_response_head(status, &headers, false);
    resp.extend_from_slice(body.as_bytes());
    resp
}
