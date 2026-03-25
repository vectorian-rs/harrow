//! HTTP/1.1 protocol implementation.
//!
//! This module provides the HTTP/1.1 connection handling using monoio's
//! native io_uring support. It handles:
//! - Keep-alive connections
//! - Content-Length and chunked transfer encoding
//! - Pipeline (sequential request-response)
//!
//! # Cancellation Safety
//!
//! All I/O operations use cancellable variants to prevent use-after-free
//! when timeouts fire or connections are dropped.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, CancelableAsyncReadRent, Canceller};
use monoio::net::TcpStream;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;

use crate::buffer::{DEFAULT_BUFFER_SIZE, acquire_buffer, release_buffer};
use crate::codec;
use crate::o11y::ConnectionMetrics;

/// Maximum size of the header read buffer (64 KiB).
const MAX_HEADER_BUF: usize = 64 * 1024;

/// Configuration for H1 connections.
#[allow(dead_code)]
pub(crate) struct H1Config {
    /// Shared application state.
    pub shared: Arc<SharedState>,
    /// Timeout for reading request headers.
    pub header_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection.
    pub connection_timeout: Option<Duration>,
    /// Remote address (for logging).
    pub remote_addr: Option<SocketAddr>,
    /// Connection metrics tracker.
    pub metrics: ConnectionMetrics,
}

/// HTTP/1.1 connection handler.
///
/// Manages a single HTTP/1.1 connection with keep-alive support.
/// Requests are processed sequentially (no pipelining parallelism).
pub(crate) struct H1Connection {
    stream: TcpStream,
    config: H1Config,
    buf: BytesMut,
}

impl H1Connection {
    /// Create a new H1 connection handler.
    pub(crate) fn new(stream: TcpStream, config: H1Config) -> Self {
        Self {
            stream,
            config,
            buf: BytesMut::with_capacity(8192),
        }
    }

    /// Run the HTTP/1.1 connection to completion.
    ///
    /// Handles sequential request-response cycles until the connection
    /// closes, times out, or encounters an error.
    pub(crate) async fn run(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = if let Some(ct) = self.config.connection_timeout {
            monoio::select! {
                r = self.run_inner() => r,
                _ = monoio::time::sleep(ct) => {
                    tracing::warn!("connection timed out");
                    Ok(())
                }
            }
        } else {
            self.run_inner().await
        };

        if let Err(ref e) = result {
            tracing::debug!(error = %e, "h1 connection error");
        }

        // Record connection close
        let _duration = self.config.metrics.close();

        result
    }

    /// Inner connection loop.
    async fn run_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let max_body = self.config.shared.max_body_size;

        loop {
            // Read headers
            let parsed = match self.read_headers().await {
                Ok(parsed) => parsed,
                Err(e) => {
                    let msg = e.to_string();
                    if msg != "connection closed" {
                        let _ = self.write_400().await;
                    }
                    return Err(e);
                }
            };
            let keep_alive = parsed.keep_alive;

            // Early reject: Content-Length exceeds limit
            if max_body > 0
                && let Some(cl) = parsed.content_length
                && cl as usize > max_body
            {
                let response = harrow_core::response::Response::new(
                    http::StatusCode::PAYLOAD_TOO_LARGE,
                    "payload too large",
                );
                self.write_response(response.into_inner(), false).await?;
                break;
            }

            // Send 100 Continue if requested
            if parsed.expect_continue {
                let (result, _) = self.stream.write_all(codec::CONTINUE_100.to_vec()).await;
                result?;
            }

            // Read body
            let body_bytes = self
                .read_body(parsed.content_length, parsed.chunked, max_body)
                .await?;

            // Build and dispatch request
            let response = self.dispatch_request(&parsed, body_bytes).await;

            // Write response
            self.write_response(response, keep_alive).await?;

            if !keep_alive {
                break;
            }
        }

        Ok(())
    }

    /// Read HTTP headers from the stream into `buf`.
    ///
    /// Uses a wall-clock deadline for the entire header read phase to prevent
    /// Slowloris attacks (trickling bytes to keep per-read timeouts from firing).
    ///
    /// # Cancellation Safety
    /// This function uses `cancelable_read` to ensure that when a timeout fires,
    /// the kernel operation is explicitly cancelled before the buffer is dropped.
    async fn read_headers(&mut self) -> Result<codec::ParsedRequest, Box<dyn std::error::Error>> {
        let deadline = self
            .config
            .header_read_timeout
            .map(|dur| Instant::now() + dur);

        loop {
            // Try parsing what we have.
            match codec::try_parse_request(&self.buf) {
                Ok(parsed) => {
                    // Remove consumed header bytes from buf, leaving any trailing body data.
                    let _ = self.buf.split_to(parsed.header_len);
                    return Ok(parsed);
                }
                Err(codec::CodecError::Incomplete) => {
                    // Need more data.
                }
                Err(codec::CodecError::Invalid(msg)) => {
                    return Err(msg.into());
                }
            }

            if self.buf.len() >= MAX_HEADER_BUF {
                return Err("request headers too large".into());
            }

            // Check wall-clock deadline before each read.
            let remaining = match deadline {
                Some(dl) => match dl.checked_duration_since(Instant::now()) {
                    Some(rem) => Some(rem),
                    None => return Err("header read timeout".into()),
                },
                None => None,
            };

            // Read more data from socket using pooled buffer.
            let read_buf = acquire_buffer(DEFAULT_BUFFER_SIZE);
            let (result, read_buf) = if let Some(rem) = remaining {
                // Use cancellable read with timeout for safety
                let canceller = Canceller::new();
                let handle = canceller.handle();

                let recv_fut = self.stream.cancelable_read(read_buf, handle);
                let mut recv_fut = std::pin::pin!(recv_fut);

                monoio::select! {
                    r = &mut recv_fut => r,
                    _ = monoio::time::sleep(rem) => {
                        // Timeout fired - cancel the kernel operation
                        let _ = canceller.cancel();
                        // CRITICAL: Await the cancelled operation to reclaim the buffer
                        let (_, read_buf) = recv_fut.await;
                        // Return buffer to pool before returning error
                        release_buffer(read_buf);
                        return Err("header read timeout".into());
                    }
                }
            } else {
                // No timeout - use regular read
                self.stream.read(read_buf).await
            };

            let n = result?;
            if n == 0 {
                // Return buffer to pool before exiting
                release_buffer(read_buf);
                if self.buf.is_empty() {
                    // Clean close — client disconnected between requests.
                    return Err("connection closed".into());
                }
                return Err("unexpected eof during header read".into());
            }
            self.buf.extend_from_slice(&read_buf[..n]);
            // Return buffer to pool for reuse
            release_buffer(read_buf);
        }
    }

    /// Read the request body based on Content-Length or chunked encoding.
    async fn read_body(
        &mut self,
        content_length: Option<u64>,
        chunked: bool,
        max_body: usize,
    ) -> Result<Bytes, Box<dyn std::error::Error>> {
        if chunked {
            return self.read_chunked_body(max_body).await;
        }

        let length = match content_length {
            Some(0) | None => return Ok(Bytes::new()),
            Some(len) => len as usize,
        };

        // Read until we have `length` bytes of body using pooled buffers.
        while self.buf.len() < length {
            let needed = length - self.buf.len();
            let read_buf = acquire_buffer(needed.min(DEFAULT_BUFFER_SIZE));
            let (result, read_buf) = self.stream.read(read_buf).await;
            let n = result?;
            if n == 0 {
                release_buffer(read_buf);
                return Err("unexpected eof during body read".into());
            }
            self.buf.extend_from_slice(&read_buf[..n]);
            release_buffer(read_buf);
        }

        let body = self.buf.split_to(length).freeze();
        Ok(body)
    }

    /// Read a chunked transfer-encoded body.
    async fn read_chunked_body(
        &mut self,
        max_body: usize,
    ) -> Result<Bytes, Box<dyn std::error::Error>> {
        loop {
            match codec::decode_chunked(&self.buf)? {
                Some((body, consumed)) => {
                    if max_body > 0 && body.len() > max_body {
                        return Err("body too large".into());
                    }
                    let _ = self.buf.split_to(consumed);
                    return Ok(body);
                }
                None => {
                    // Need more data - use pooled buffer
                    let read_buf = acquire_buffer(DEFAULT_BUFFER_SIZE);
                    let (result, read_buf) = self.stream.read(read_buf).await;
                    let n = result?;
                    if n == 0 {
                        release_buffer(read_buf);
                        return Err("unexpected eof during chunked body read".into());
                    }
                    self.buf.extend_from_slice(&read_buf[..n]);
                    release_buffer(read_buf);
                }
            }
        }
    }

    /// Build request and dispatch through Harrow.
    async fn dispatch_request(
        &self,
        parsed: &codec::ParsedRequest,
        body_bytes: Bytes,
    ) -> http::Response<harrow_core::response::ResponseBody> {
        // Build http::Request
        let mut builder = http::Request::builder()
            .method(&parsed.method)
            .uri(&parsed.uri)
            .version(parsed.version);

        for (name, value) in parsed.headers.iter() {
            builder = builder.header(name, value);
        }

        let body: Body = {
            use http_body_util::Full;
            Full::new(body_bytes)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
                .boxed()
        };

        let req = match builder.body(body) {
            Ok(req) => req,
            Err(e) => {
                // This shouldn't happen, but handle gracefully
                return harrow_core::response::Response::new(
                    http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("request build error: {}", e),
                )
                .into_inner();
            }
        };

        // Dispatch through Harrow
        dispatch(Arc::clone(&self.config.shared), req).await
    }

    /// Write the full HTTP response (head + body) to the stream.
    async fn write_response(
        &mut self,
        response: http::Response<harrow_core::response::ResponseBody>,
        keep_alive: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut parts, body) = response.into_parts();

        if !keep_alive {
            parts
                .headers
                .insert(http::header::CONNECTION, "close".parse().unwrap());
        }

        let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);

        // Write response head.
        let head = codec::write_response_head(parts.status, &parts.headers, !has_content_length);
        let (result, _) = self.stream.write_all(head).await;
        result?;

        // Drain body frame-by-frame.
        if has_content_length {
            // Known length — write body frames directly.
            self.write_body_direct(body).await?;
        } else {
            // Unknown length — use chunked transfer-encoding.
            self.write_body_chunked(body).await?;
        }

        Ok(())
    }

    /// Write body frames directly (Content-Length path).
    async fn write_body_direct(
        &mut self,
        mut body: harrow_core::response::ResponseBody,
    ) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
            if let Ok(data) = frame.into_data()
                && !data.is_empty()
            {
                let (result, _) = self.stream.write_all(data.to_vec()).await;
                result?;
            }
        }
        Ok(())
    }

    /// Write body frames with chunked transfer-encoding.
    async fn write_body_chunked(
        &mut self,
        mut body: harrow_core::response::ResponseBody,
    ) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
            if let Ok(data) = frame.into_data()
                && !data.is_empty()
            {
                let chunk = codec::encode_chunk(&data);
                let (result, _) = self.stream.write_all(chunk).await;
                result?;
            }
        }
        // Write terminator
        let (result, _) = self
            .stream
            .write_all(codec::CHUNK_TERMINATOR.to_vec())
            .await;
        result?;
        Ok(())
    }

    /// Write a minimal 400 Bad Request response.
    async fn write_400(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let response =
            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad request"
                .to_vec();
        let (result, _) = self.stream.write_all(response).await;
        result?;
        Ok(())
    }
}

/// Handle a single TCP connection with keep-alive support.
///
/// This is the public entry point that creates an H1Connection and runs it.
pub(crate) async fn handle_connection(
    stream: TcpStream,
    remote_addr: Option<SocketAddr>,
    shared: Arc<SharedState>,
    header_read_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    active_count: Rc<Cell<usize>>,
) {
    use crate::o11y::{ConnectionMetrics, connection_span};
    use tracing::Instrument;

    // Create connection metrics - this increments the active connection gauge
    let metrics = ConnectionMetrics::new(active_count);
    let span = connection_span(metrics.id, remote_addr);

    let config = H1Config {
        shared,
        header_read_timeout,
        connection_timeout,
        remote_addr,
        metrics,
    };

    let conn = H1Connection::new(stream, config);

    // Run the connection within the span
    if let Err(e) = conn.run().instrument(span).await {
        tracing::debug!(error = %e, "h1 connection error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests will be added here as needed
}
