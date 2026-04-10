//! HTTP/1.1 protocol handler for the meguri server.
//!
//! Parses HTTP/1.1 requests and writes responses using io_uring.
//!
//! # TODO
//! This is a skeleton. The full implementation will:
//! 1. Read request data using meguri's Recv operation
//! 2. Parse HTTP/1.1 headers (reuse codec from harrow-server-monoio)
//! 3. Build a harrow_core::request::Request
//! 4. Dispatch through harrow_core::dispatch::dispatch()
//! 5. Write the response using meguri's Send operation

use std::sync::Arc;

use harrow_core::dispatch::SharedState;

/// Handle an HTTP/1.1 connection.
///
/// Reads requests, dispatches, writes response.
pub(crate) async fn handle_http1(
    _fd: std::os::fd::RawFd,
    _shared: Arc<SharedState>,
) -> std::io::Result<()> {
    // Placeholder: full implementation will use meguri ops + harrow dispatch.
    Ok(())
}
