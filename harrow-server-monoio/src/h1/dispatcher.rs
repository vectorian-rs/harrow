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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use monoio::net::TcpStream;

use harrow_core::dispatch::SharedState;
use harrow_io::BufPool;
use harrow_server::h1::{
    EarlyResponseMode, ErrorResponse, RequestBodyDecision, decide_request_body_progress,
    early_response_control,
};
use harrow_server::h1_lifecycle::{Event, trace::Trace};

use crate::h1::request_body;
use crate::o11y::ConnectionMetrics;
use crate::protocol::ProtocolError;

/// Configuration for H1 connections.
#[allow(dead_code)]
pub(crate) struct H1Config {
    /// Shared application state.
    pub shared: Arc<SharedState>,
    /// Timeout for reading request headers.
    pub header_read_timeout: Option<Duration>,
    /// Timeout for reading request bodies.
    pub body_read_timeout: Option<Duration>,
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
    pub(crate) stream: TcpStream,
    pub(crate) config: H1Config,
    pub(crate) buf: BytesMut,
    pub(crate) write_buf: BytesMut,
    pub(crate) connection_deadline: Option<Instant>,
    /// Lifecycle events for `h1_lifecycle::Machine`, recorded when the
    /// connection is dropped (only with the `lifecycle-trace` feature).
    pub(crate) trace: Trace,
}

impl H1Connection {
    /// Create a new H1 connection handler.
    pub(crate) fn new(stream: TcpStream, config: H1Config) -> Self {
        Self {
            stream,
            config,
            buf: BytesMut::with_capacity(8192),
            write_buf: BufPool::acquire_write(),
            connection_deadline: None,
            trace: Trace::new(),
        }
    }

    /// Run the HTTP/1.1 connection to completion.
    ///
    /// Handles sequential request-response cycles until the connection
    /// closes, times out, or encounters an error.
    pub(crate) async fn run(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_deadline = self
            .config
            .connection_timeout
            .map(|timeout| Instant::now() + timeout);
        let result = self.run_inner().await;

        if let Err(ref e) = result {
            tracing::debug!(error = %e, "h1 connection error");
        }

        let write_buf = std::mem::take(&mut self.write_buf);
        BufPool::release_write(write_buf);

        // Record connection close
        let _duration = self.config.metrics.close();

        result
    }

    /// Inner connection loop.
    async fn run_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let max_body = self.config.shared.max_body_size;
        self.trace.record(Event::Accept);

        'connection: loop {
            // Connection lifetime reached while waiting for the next request:
            // the pending read is abandoned (`Timeout`, then `ClosedCqe`).
            if let Err(err) = self.check_connection_deadline() {
                self.trace.record(Event::Timeout);
                self.trace.record(Event::ClosedCqe);
                return Err(err);
            }

            // Read headers
            let parsed = match self.read_headers().await {
                Ok(parsed) => parsed,
                Err(ProtocolError::StreamClosed) => {
                    self.trace.record(Event::IoError);
                    return Ok(());
                }
                // Monoio answers a header timeout with 408 and a malformed or
                // truncated head with 400: error responses, in the model.
                Err(ProtocolError::Timeout) => {
                    self.write_error_traced(ErrorResponse::RequestTimeout).await;
                    return Ok(());
                }
                Err(ProtocolError::Rejected(error)) => {
                    self.write_error_traced(error).await;
                    return Ok(());
                }
                Err(e) => {
                    self.write_error_traced(ErrorResponse::BadRequest).await;
                    return Err(Box::new(e));
                }
            };
            let keep_alive = parsed.keep_alive;
            let version = parsed.version;
            let is_head_request = parsed.method == http::Method::HEAD;

            // Early reject: Content-Length exceeds limit
            if harrow_server::h1::request_exceeds_body_limit(parsed.content_length, max_body) {
                if self
                    .write_error_traced(ErrorResponse::PayloadTooLarge)
                    .await
                {
                    break;
                }
                return Err(Box::new(ProtocolError::BodyTooLarge));
            }

            let (mut request_body_state, body) =
                match request_body::RequestBodyState::start(&mut self.stream, &parsed, max_body)
                    .await
                {
                    Ok(state) => state,
                    Err(err) => {
                        // Writing the interim `100 Continue` failed.
                        self.trace.record(Event::HeadersParsed { has_body: true });
                        self.trace.record(Event::IoError);
                        return Err(Box::new(err));
                    }
                };

            let mut response_fut = std::pin::pin!(request_body::dispatch_request(
                Arc::clone(&self.config.shared),
                &parsed,
                body,
            ));

            let mut body_complete = request_body_state.is_complete();
            self.trace.record(Event::HeadersParsed {
                has_body: !body_complete,
            });
            let mut connection_reusable = keep_alive;

            let response = loop {
                if body_complete {
                    let response = response_fut.await;
                    self.trace.record(Event::DispatchDone);
                    break response;
                }

                monoio::select! {
                    response = &mut response_fut => {
                        // The handler answered before the request body ended.
                        self.trace.record(Event::EarlyResponse);
                        let control = early_response_control(EarlyResponseMode::DropRequestBody);
                        connection_reusable = control.keep_alive;
                        request_body_state.abort();
                        break response;
                    }
                    pump = request_body_state.pump_once(self) => {
                        match decide_request_body_progress(
                            pump,
                            connection_reusable,
                            EarlyResponseMode::DropRequestBody,
                        ) {
                            RequestBodyDecision::Continue => self.trace.record(Event::BodyNeedMore),
                            RequestBodyDecision::BodyComplete { keep_alive, .. } => {
                                self.trace.record(Event::BodyDone);
                                body_complete = true;
                                connection_reusable = keep_alive;
                            }
                            RequestBodyDecision::WriteError(error) => {
                                if self.write_error_traced(error).await {
                                    break 'connection;
                                }
                                return Err(Box::new(ProtocolError::StreamClosed));
                            }
                            RequestBodyDecision::CloseConnection => {
                                self.trace.record(Event::IoError);
                                break 'connection;
                            }
                        }
                    }
                }
            };

            // A streaming response to HTTP/1.0 ends by closing the connection.
            if harrow_server::h1::response_closes_connection(version, &response, is_head_request) {
                connection_reusable = false;
            }

            // Write response
            if let Err(err) = self
                .write_response(response, version, connection_reusable, is_head_request)
                .await
            {
                self.trace.record(Event::IoError);
                return Err(err);
            }
            self.trace.record(Event::WriteDone {
                keep_alive: connection_reusable,
                buffered_next_request: !self.buf.is_empty(),
            });

            if !connection_reusable {
                break;
            }
        }

        Ok(())
    }
}

/// Handle a single TCP connection with keep-alive support.
///
/// This is the public entry point that creates an H1Connection and runs it.
pub(crate) async fn handle_connection(stream: TcpStream, conn: crate::connection::ConnConfig) {
    let remote_addr = conn.remote_addr;
    let shared = conn.shared;
    let header_read_timeout = conn.header_read_timeout;
    let body_read_timeout = conn.body_read_timeout;
    let connection_timeout = conn.connection_timeout;
    let active_count = conn.active_count;
    use crate::o11y::{ConnectionMetrics, connection_span};
    use tracing::Instrument;

    // Create connection metrics - this increments the active connection gauge
    let metrics = ConnectionMetrics::new(active_count);
    let span = connection_span(metrics.id, remote_addr);

    let config = H1Config {
        shared,
        header_read_timeout,
        body_read_timeout,
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

impl H1Connection {
    /// Write an error response and close: the model's `ProtocolError` /
    /// `BodyLimitExceeded`, then `WriteDone` (or `IoError` if the write
    /// fails). Returns whether the write succeeded.
    async fn write_error_traced(&mut self, error: ErrorResponse) -> bool {
        self.trace.record(match error {
            ErrorResponse::PayloadTooLarge => Event::BodyLimitExceeded,
            _ => Event::ProtocolError,
        });
        let written = self
            .write_status(error.status(), error.body())
            .await
            .is_ok();
        self.trace.record(if written {
            Event::WriteDone {
                keep_alive: false,
                buffered_next_request: false,
            }
        } else {
            Event::IoError
        });
        written
    }
}
