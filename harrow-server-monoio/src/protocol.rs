//! Protocol abstraction types for HTTP/1.1 and HTTP/2.
//!
//! This module provides shared types and utilities used by both
//! HTTP protocol implementations.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};

use harrow_core::request::Body;

/// Convert a bytes buffer into a Harrow body.
///
/// This is a helper function used by both H1 and H2 implementations
/// to create a `Body` from raw bytes.
pub(crate) fn body_from_bytes(bytes: Bytes) -> Body {
    Full::new(bytes)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
        .boxed_unsync()
}

/// Error types that can occur at the protocol layer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProtocolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP parse error: {0}")]
    Parse(String),

    #[error("protocol violation: {0}")]
    ProtocolViolation(String),

    #[error("connection timeout")]
    Timeout,

    #[error("body too large")]
    BodyTooLarge,

    #[error("stream closed")]
    StreamClosed,
}
