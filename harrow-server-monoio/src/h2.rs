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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use monoio::net::TcpStream;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;

use crate::o11y::ConnectionMetrics;

/// Configuration for H2 connections.
#[allow(dead_code)]
pub(crate) struct H2Config {
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

/// HTTP/2 connection handler.
///
/// Manages a single HTTP/2 connection with support for multiple
/// concurrent streams. Uses monoio-http's H2 implementation.
pub(crate) struct H2Connection {
    stream: TcpStream,
    config: H2Config,
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
        // Build H2 server with default configuration
        // TODO: Expose configuration options (max_concurrent_streams, window sizes, etc.)
        let builder = monoio_http::h2::server::Builder::new();

        // Perform HTTP/2 handshake
        let mut connection = builder
            .handshake(self.stream)
            .await
            .map_err(|e| format!("h2 handshake failed: {}", e))?;

        tracing::debug!(
            connection.id = self.config.metrics.id,
            "h2 connection established"
        );

        let metrics_id = self.config.metrics.id;

        // Accept incoming streams
        loop {
            match connection.accept().await {
                Some(Ok((request, respond))) => {
                    // Spawn a task to handle this stream
                    let shared = Arc::clone(&self.config.shared);
                    let max_body = shared.max_body_size;

                    monoio::spawn(async move {
                        if let Err(e) = handle_stream(request, respond, shared, max_body).await {
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
                        connection.id = self.config.metrics.id,
                        error = %e,
                        "h2 accept error"
                    );
                    // Continue accepting other streams - one bad stream doesn't kill the connection
                }
                None => {
                    tracing::debug!(
                        connection.id = self.config.metrics.id,
                        "h2 connection closed by peer"
                    );
                    break;
                }
            }
        }

        // Record connection close
        let _duration = self.config.metrics.close();

        Ok(())
    }
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
) -> Result<(), Box<dyn std::error::Error>> {
    // Read request body from H2 stream
    let body_bytes = read_h2_body(&mut request, max_body).await?;

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
) -> Result<Bytes, Box<dyn std::error::Error>> {
    let body = request.body_mut();
    let mut chunks = Vec::new();
    let mut total_len: usize = 0;

    while let Some(data) = body.data().await {
        let data = data.map_err(|e| format!("h2 body error: {}", e))?;
        let len = data.len();

        // Check body size limit
        if max_body > 0 && total_len + len > max_body {
            return Err("body too large".into());
        }

        total_len += len;
        chunks.push(data);

        // Release flow control capacity
        body.flow_control().release_capacity(len)?;
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
    let response = http::Response::builder()
        .status(parts.status)
        .version(http::Version::HTTP_2)
        .body(())
        .expect("valid response");

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
pub(crate) async fn handle_connection(
    stream: TcpStream,
    remote_addr: Option<SocketAddr>,
    shared: Arc<SharedState>,
    header_read_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    active_count: std::rc::Rc<std::cell::Cell<usize>>,
) {
    use crate::o11y::connection_span;
    use tracing::Instrument;

    // Create connection metrics - this increments the active connection gauge
    let metrics = ConnectionMetrics::new(active_count);
    let span = connection_span(metrics.id, remote_addr);

    let config = H2Config {
        shared,
        header_read_timeout,
        connection_timeout,
        remote_addr,
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
