//! HTTP/2 protocol implementation using monoio-http.
//!
//! This module provides HTTP/2 support via the monoio-http crate, which offers
//! native io_uring-based HTTP/2 with multiplexed streams and flow control.
//!
//! # Key Features
//!
//! - **Multiplexing**: Multiple concurrent streams per connection
//! - **Flow Control**: HTTP/2 window-based flow control  
//! - **Prior Knowledge**: Direct H2 connections without ALPN
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │           H2Connection                    │
//! │  ┌──────────────────────────────────┐   │
//! │  │  monoio_http::h2::server         │   │
//! │  │  (connection driver)             │   │
//! │  └──────────┬───────────────────────┘   │
//! │             │                            │
//! │             ▼                            │
//! │  ┌──────────────────────────────────┐   │
//! │  │     Stream Handler Tasks         │   │
//! │  │  ┌─────┐ ┌─────┐ ┌─────┐        │   │
//! │  │  │ S1  │ │ S2  │ │ S3  │ ...    │   │
//! │  │  └──┬──┘ └──┬──┘ └──┬──┘        │   │
//! │  └─────┼───────┼───────┼───────────┘   │
//! │        │       │       │                │
//! │        ▼       ▼       ▼                │
//! │  ┌──────────────────────────────────┐   │
//! │  │   Harrow Dispatch (per-stream)   │   │
//! │  └──────────────────────────────────┘   │
//! └──────────────────────────────────────────┘
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use monoio::net::TcpStream;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;
use harrow_core::response::Response;

use crate::o11y::ConnectionMetrics;
use crate::protocol::ProtocolError;

/// Configuration for H2 connections.
pub(crate) struct H2Config {
    /// Shared application state.
    pub shared: Arc<SharedState>,
    /// Timeout for reading request headers.
    pub header_read_timeout: Option<Duration>,
    /// Timeout for reading request bodies.
    pub body_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection.
    pub connection_timeout: Option<Duration>,
    /// Maximum concurrent streams allowed on this H2 connection.
    pub max_concurrent_streams: u32,
    /// Connection metrics tracker.
    pub metrics: ConnectionMetrics,
}

/// HTTP/2 connection handler.
///
/// Manages a single HTTP/2 connection with support for multiple
/// concurrent streams. Uses monoio-http's H2 implementation.
pub(crate) struct H2Connection {
    stream: TcpStream,
    config: H2Config,
}

struct ActiveStreamGuard {
    active_streams: Arc<AtomicUsize>,
}

impl ActiveStreamGuard {
    fn new(active_streams: Arc<AtomicUsize>) -> Self {
        active_streams.fetch_add(1, Ordering::AcqRel);
        Self { active_streams }
    }
}

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        self.active_streams.fetch_sub(1, Ordering::AcqRel);
    }
}

impl H2Connection {
    /// Create a new H2 connection handler.
    pub(crate) fn new(stream: TcpStream, config: H2Config) -> Self {
        Self { stream, config }
    }

    /// Run the HTTP/2 connection to completion.
    ///
    /// This drives the HTTP/2 connection state machine, accepting streams
    /// and spawning tasks to handle each request concurrently.
    pub(crate) async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let H2Connection { stream, config } = self;
        let H2Config {
            shared,
            header_read_timeout,
            body_read_timeout,
            connection_timeout,
            max_concurrent_streams,
            metrics,
        } = config;
        let metrics_id = metrics.id;
        let connection_deadline =
            connection_timeout.and_then(|timeout| Instant::now().checked_add(timeout));

        // Build H2 server with default configuration
        let mut builder = monoio_http::h2::server::Builder::new();
        builder.max_concurrent_streams(max_concurrent_streams);

        // Perform HTTP/2 handshake
        let handshake_timeout = effective_timeout(header_read_timeout, connection_deadline);
        let mut connection = if let Some(timeout) = handshake_timeout {
            match monoio::select! {
                result = builder.handshake(stream) => Ok(result),
                _ = monoio::time::sleep(timeout) => Err(()),
            } {
                Ok(result) => result.map_err(|e| format!("h2 handshake failed: {}", e))?,
                Err(()) => {
                    if deadline_expired(connection_deadline) {
                        tracing::debug!(
                            connection.id = metrics_id,
                            "h2 connection timeout during handshake"
                        );
                    } else {
                        tracing::debug!(
                            connection.id = metrics_id,
                            "h2 header read timeout during handshake"
                        );
                    }
                    let _duration = metrics.close();
                    return Ok(());
                }
            }
        } else {
            builder
                .handshake(stream)
                .await
                .map_err(|e| format!("h2 handshake failed: {}", e))?
        };

        tracing::debug!(connection.id = metrics_id, "h2 connection established");

        let active_streams = Arc::new(AtomicUsize::new(0));

        // Accept incoming streams
        loop {
            let accept_timeout = effective_timeout(header_read_timeout, connection_deadline);

            let accept_result = if let Some(timeout) = accept_timeout {
                match monoio::select! {
                    result = connection.accept() => Ok(result),
                    _ = monoio::time::sleep(timeout) => Err(()),
                } {
                    Ok(result) => result,
                    Err(()) => {
                        if deadline_expired(connection_deadline) {
                            tracing::debug!(connection.id = metrics_id, "h2 connection timeout");
                            break;
                        } else if active_streams.load(Ordering::Acquire) == 0 {
                            tracing::debug!(connection.id = metrics_id, "h2 header read timeout");
                            break;
                        }
                        continue;
                    }
                }
            } else {
                connection.accept().await
            };

            match accept_result {
                Some(Ok((request, respond))) => {
                    // Spawn a task to handle this stream
                    let shared = Arc::clone(&shared);
                    let max_body = shared.max_body_size;
                    let active_streams = Arc::clone(&active_streams);
                    let active_stream = ActiveStreamGuard::new(active_streams);

                    monoio::spawn(async move {
                        let _active_stream = active_stream;
                        if let Err(e) =
                            handle_stream(request, respond, shared, max_body, body_read_timeout)
                                .await
                        {
                            tracing::debug!(
                                connection.id = metrics_id,
                                error = %e,
                                "h2 stream error"
                            );
                        }
                    });
                }
                Some(Err(e)) => {
                    tracing::debug!(
                        connection.id = metrics_id,
                        error = %e,
                        "h2 accept error"
                    );
                    // Continue accepting other streams - one bad stream doesn't kill the connection
                }
                None => {
                    tracing::debug!(connection.id = metrics_id, "h2 connection closed by peer");
                    break;
                }
            }
        }

        // Record connection close
        let _duration = metrics.close();

        Ok(())
    }
}

fn effective_timeout(
    timeout: Option<Duration>,
    connection_deadline: Option<Instant>,
) -> Option<Duration> {
    match (timeout, remaining_until(connection_deadline)) {
        (Some(timeout), Some(remaining)) => Some(timeout.min(remaining)),
        (Some(timeout), None) => Some(timeout),
        (None, Some(remaining)) => Some(remaining),
        (None, None) => None,
    }
}

fn remaining_until(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

fn deadline_expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Handle a single HTTP/2 stream.
///
/// Each stream is an independent request-response exchange.
/// This function bridges monoio-http's H2 types to Harrow's types.
async fn handle_stream(
    mut request: http::Request<monoio_http::h2::RecvStream>,
    mut respond: monoio_http::h2::server::SendResponse<bytes::Bytes>,
    shared: Arc<SharedState>,
    max_body: usize,
    body_read_timeout: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read request body from H2 stream
    let body_bytes = match read_h2_body(&mut request, max_body, body_read_timeout).await {
        Ok(body) => body,
        Err(ProtocolError::BodyTooLarge) => {
            send_h2_response(
                &mut respond,
                Response::new(http::StatusCode::PAYLOAD_TOO_LARGE, "payload too large")
                    .into_inner(),
            )
            .await?;
            return Ok(());
        }
        Err(ProtocolError::Timeout) => {
            send_h2_response(
                &mut respond,
                Response::new(http::StatusCode::REQUEST_TIMEOUT, "request timeout").into_inner(),
            )
            .await?;
            return Ok(());
        }
        Err(err) => return Err(Box::new(err)),
    };

    // Convert to Harrow request
    let harrow_request = convert_to_harrow_request(request, body_bytes)?;

    // Dispatch through Harrow
    let harrow_response = dispatch(shared, harrow_request).await;

    // Convert response and send
    send_h2_response(&mut respond, harrow_response).await?;

    Ok(())
}

/// Read the entire body from an HTTP/2 stream.
///
/// HTTP/2 streams use flow control - we must release capacity
/// as we receive data.
async fn read_h2_body(
    request: &mut http::Request<monoio_http::h2::RecvStream>,
    max_body: usize,
    body_read_timeout: Option<Duration>,
) -> Result<Bytes, ProtocolError> {
    let body = request.body_mut();
    let mut chunks = Vec::new();
    let mut total_len: usize = 0;

    loop {
        let data = if let Some(timeout) = body_read_timeout {
            monoio::select! {
                data = body.data() => data,
                _ = monoio::time::sleep(timeout) => return Err(ProtocolError::Timeout),
            }
        } else {
            body.data().await
        };

        let Some(data) = data else {
            break;
        };

        let data = data.map_err(|e| ProtocolError::Parse(format!("h2 body error: {e}")))?;
        let len = data.len();

        // Check body size limit
        if max_body > 0 && total_len + len > max_body {
            return Err(ProtocolError::BodyTooLarge);
        }

        total_len += len;
        chunks.push(data);

        // Release flow control capacity
        body.flow_control()
            .release_capacity(len)
            .map_err(|e| ProtocolError::ProtocolViolation(e.to_string()))?;
    }

    // Combine chunks
    let mut result = bytes::BytesMut::with_capacity(total_len);
    for chunk in chunks {
        result.extend_from_slice(&chunk);
    }

    Ok(result.freeze())
}

/// Convert monoio-http H2 request to Harrow request.
fn convert_to_harrow_request(
    request: http::Request<monoio_http::h2::RecvStream>,
    body_bytes: Bytes,
) -> Result<http::Request<Body>, Box<dyn std::error::Error>> {
    let (parts, _) = request.into_parts();
    let body = crate::protocol::body_from_bytes(body_bytes);

    Ok(http::Request::from_parts(parts, body))
}

/// Send Harrow response via HTTP/2 stream.
async fn send_h2_response(
    respond: &mut monoio_http::h2::server::SendResponse<bytes::Bytes>,
    response: http::Response<harrow_core::response::ResponseBody>,
) -> Result<(), Box<dyn std::error::Error>> {
    use http_body_util::BodyExt;

    let (parts, mut body) = response.into_parts();

    // Build H2 response (H2 always uses HTTP/2 version)
    let mut builder = http::Response::builder()
        .status(parts.status)
        .version(http::Version::HTTP_2);
    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }
    let response = builder.body(()).expect("valid response");

    // Collect body to determine if we have trailers
    let mut body_data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| format!("body frame error: {}", e))?;

        if let Ok(data) = frame.into_data() {
            body_data.push(data);
        }
    }

    // Send response headers
    let mut stream = if body_data.is_empty() {
        // No body - send headers with end_stream
        respond.send_response(response, true)?;
        return Ok(());
    } else {
        respond.send_response(response, false)?
    };

    // Send body data frames
    let total_chunks = body_data.len();
    for (i, data) in body_data.into_iter().enumerate() {
        let is_end = i == total_chunks - 1;
        stream.send_data(data, is_end)?;
    }

    Ok(())
}

/// Handle a single TCP connection with HTTP/2.
///
/// This is the public entry point that creates an H2Connection and runs it.
pub(crate) async fn handle_connection(stream: TcpStream, conn: crate::connection::ConnConfig) {
    let remote_addr = conn.remote_addr;
    let shared = conn.shared;
    let header_read_timeout = conn.header_read_timeout;
    let body_read_timeout = conn.body_read_timeout;
    let connection_timeout = conn.connection_timeout;
    let max_concurrent_streams = conn.max_h2_streams;
    let active_count = conn.active_count;
    use crate::o11y::connection_span;
    use tracing::Instrument;

    // Create connection metrics - this increments the active connection gauge
    let metrics = ConnectionMetrics::new(active_count);
    let span = connection_span(metrics.id, remote_addr);

    let config = H2Config {
        shared,
        header_read_timeout,
        body_read_timeout,
        connection_timeout,
        max_concurrent_streams,
        metrics,
    };

    let conn = H2Connection::new(stream, config);
    let connection_id = conn.config.metrics.id;

    // Run the connection within the span
    if let Err(e) = conn.run().instrument(span).await {
        tracing::debug!(
            connection.id = connection_id,
            error = %e,
            "h2 connection error"
        );
    }
}

#[cfg(test)]
mod tests {
    // Tests will be added in Phase 2 integration
}
