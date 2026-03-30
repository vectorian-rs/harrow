//! Connection handling and protocol dispatch.
//!
//! This module is responsible for accepting TCP connections and dispatching
//! to the appropriate protocol handler.
//!
//! # Protocol Support
//!
//! Currently supports:
//! - HTTP/1.1 (default)
//! - HTTP/2 with prior knowledge (via `ServerConfig::enable_http2`)
//!
//! # Future Work
//!
//! - Automatic protocol detection (H1 vs H2 preface)
//! - HTTP/2 with TLS/ALPN
//! - HTTP/1.1 upgrade to H2

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use monoio::net::TcpStream;

use harrow_core::dispatch::SharedState;

/// HTTP protocol version.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ProtocolVersion {
    /// HTTP/1.0 or HTTP/1.1
    Http11,
    /// HTTP/2 with prior knowledge (direct H2 connection)
    Http2PriorKnowledge,
}

/// Handle a single TCP connection.
///
/// Dispatches to the appropriate protocol handler based on configuration.
///
/// # Cancellation Safety
/// When a connection timeout fires, the protocol handler is responsible
/// for properly cancelling any in-flight I/O operations.
pub(crate) async fn handle_connection(
    stream: TcpStream,
    remote_addr: Option<SocketAddr>,
    shared: Arc<SharedState>,
    header_read_timeout: Option<Duration>,
    body_read_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    max_h2_streams: u32,
    active_count: Rc<Cell<usize>>,
    protocol: ProtocolVersion,
) {
    match protocol {
        ProtocolVersion::Http11 => {
            tracing::debug!(
                remote_addr = ?remote_addr,
                "using HTTP/1.1"
            );
            crate::h1::handle_connection(
                stream,
                remote_addr,
                shared,
                header_read_timeout,
                body_read_timeout,
                connection_timeout,
                active_count,
            )
            .await;
        }
        ProtocolVersion::Http2PriorKnowledge => {
            tracing::debug!(
                remote_addr = ?remote_addr,
                "using HTTP/2 (prior knowledge)"
            );
            crate::h2::handle_connection(
                stream,
                remote_addr,
                shared,
                header_read_timeout,
                body_read_timeout,
                connection_timeout,
                max_h2_streams,
                active_count,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_enum() {
        assert_ne!(
            ProtocolVersion::Http11,
            ProtocolVersion::Http2PriorKnowledge
        );
    }
}
