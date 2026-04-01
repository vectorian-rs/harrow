//! Observability integration for the monoio server.
//!
//! This module provides server-level metrics and tracing integration.
//! Request-level observability is handled by the `o11y` middleware in
//! `harrow-middleware`, which is automatically invoked via `harrow_core::dispatch`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// --- Connection Metrics ------------------------------------------------------

/// Global connection counter for generating connection IDs.
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Connection metrics tracked per connection.
pub struct ConnectionMetrics {
    /// Unique connection ID.
    pub id: u64,
    /// When the connection was accepted.
    pub start_time: Instant,
    /// Active connection gauge reference.
    gauge: Rc<Cell<usize>>,
    /// Whether close() was called (prevents double-decrement in Drop).
    closed: bool,
}

impl ConnectionMetrics {
    /// Create new connection metrics and increment the gauge.
    pub fn new(gauge: Rc<Cell<usize>>) -> Self {
        let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let current = gauge.get();
        gauge.set(current + 1);

        tracing::debug!(
            connection.id = id,
            connection.active = current + 1,
            "connection accepted"
        );

        Self {
            id,
            start_time: Instant::now(),
            gauge,
            closed: false,
        }
    }

    /// Record connection closed and return duration.
    pub fn close(mut self) -> std::time::Duration {
        let duration = self.start_time.elapsed();

        // Decrement gauge and mark as already closed to prevent double-decrement in Drop
        let current = self.gauge.get();
        if current > 0 {
            self.gauge.set(current - 1);
        }
        // Set gauge to 0 as a sentinel to prevent Drop from decrementing again
        // This is a bit of a hack - we use usize::MAX as a sentinel since we can't use Option
        // A cleaner approach would be to use an Option<usize> in the struct, but that adds overhead
        // Instead, we just accept that Drop might decrement again if the value is still > 0
        // Actually, let's just use a flag
        self.closed = true;

        tracing::debug!(
            connection.id = self.id,
            connection.duration_ms = duration.as_millis() as u64,
            connection.active = self.gauge.get(),
            "connection closed"
        );

        duration
    }
}

impl Drop for ConnectionMetrics {
    fn drop(&mut self) {
        // Only decrement if close() wasn't called
        if !self.closed {
            let current = self.gauge.get();
            if current > 0 {
                self.gauge.set(current - 1);
            }
        }
    }
}

// --- Server Lifecycle Tracing ------------------------------------------------

/// Record server startup with configuration details and I/O driver detection.
pub fn record_server_start(addr: std::net::SocketAddr, config: &super::ServerConfig) {
    let io_driver = super::kernel_check::detect_io_driver();
    tracing::info!(
        server.addr = %addr,
        server.io_driver = %io_driver,
        server.max_connections = config.max_connections,
        server.max_h2_streams = config.max_h2_streams,
        server.workers = config.workers,
        server.header_read_timeout_ms = config.header_read_timeout.map(|d| d.as_millis() as u64),
        server.body_read_timeout_ms = config.body_read_timeout.map(|d| d.as_millis() as u64),
        server.connection_timeout_ms = config.connection_timeout.map(|d| d.as_millis() as u64),
        server.drain_timeout_ms = config.drain_timeout.as_millis() as u64,
        "harrow-monoio server starting"
    );
    if io_driver == super::kernel_check::IoDriver::Epoll {
        tracing::warn!(
            "io_uring unavailable — falling back to epoll. For io_uring, run with --security-opt seccomp=unconfined or a custom seccomp profile."
        );
    }
}

/// Record server shutdown initiation.
pub fn record_server_shutdown() {
    tracing::info!("harrow-monoio server shutting down");
}

/// Record successful drain completion.
pub fn record_drain_complete(active_connections: usize) {
    if active_connections == 0 {
        tracing::info!("all connections drained successfully");
    } else {
        tracing::warn!(
            connections.still_active = active_connections,
            "drain incomplete, connections still active"
        );
    }
}

/// Record drain timeout.
pub fn record_drain_timeout(timeout_secs: u64, active_connections: usize) {
    tracing::warn!(
        drain.timeout_secs = timeout_secs,
        connections.still_active = active_connections,
        "drain timeout exceeded"
    );
}

/// Record connection limit reached.
pub fn record_connection_limit_rejected(max_connections: usize) {
    tracing::warn!(
        server.max_connections = max_connections,
        "connection rejected: limit reached"
    );
}

/// Record accept error.
pub fn record_accept_error<E: std::fmt::Display>(error: E) {
    tracing::error!(error = %error, "accept failed");
}

/// Record TCP_NODELAY setting failure.
pub fn record_tcp_nodelay_error<E: std::fmt::Display>(error: E) {
    tracing::warn!(error = %error, "failed to set TCP_NODELAY");
}

// --- Request-Level Context ---------------------------------------------------

/// Create a tracing span for a connection.
///
/// Note: Request-level spans are created by the `o11y` middleware in
/// `harrow-middleware` via `harrow_core::dispatch`. This span covers the
/// entire connection lifecycle.
pub fn connection_span(
    connection_id: u64,
    remote_addr: Option<std::net::SocketAddr>,
) -> tracing::Span {
    let span = tracing::info_span!(
        "http_connection",
        connection.id = connection_id,
        connection.remote_addr = remote_addr.map(|a| a.to_string()),
    );
    span
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_metrics() {
        let gauge = Rc::new(Cell::new(0));

        // Create first connection
        let metrics1 = ConnectionMetrics::new(gauge.clone());
        assert_eq!(gauge.get(), 1);
        let id1 = metrics1.id;

        // Create second connection
        let metrics2 = ConnectionMetrics::new(gauge.clone());
        assert_eq!(gauge.get(), 2);
        let id2 = metrics2.id;

        // IDs should be unique
        assert_ne!(id1, id2);

        // Close first connection
        let duration1 = metrics1.close();
        assert_eq!(gauge.get(), 1);
        assert!(duration1.as_secs() < 1); // Should be very fast in test

        // Close second connection
        let duration2 = metrics2.close();
        assert_eq!(gauge.get(), 0);
        assert!(duration2.as_secs() < 1);
    }

    #[test]
    fn test_connection_metrics_drop() {
        let gauge = Rc::new(Cell::new(0));

        {
            let _metrics = ConnectionMetrics::new(gauge.clone());
            assert_eq!(gauge.get(), 1);
        }

        // After drop, gauge should be decremented
        assert_eq!(gauge.get(), 0);
    }
}
