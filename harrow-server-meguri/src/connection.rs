//! Connection handling for the meguri server.
//!
//! Manages accepted TCP connections and dispatches to protocol handlers.

use std::sync::Arc;

use harrow_core::dispatch::SharedState;

/// Handle a single TCP connection.
///
/// Reads HTTP requests, dispatches through the Harrow pipeline,
/// and writes responses back.
///
/// # TODO
/// This is a skeleton. The full implementation will:
/// 1. Use meguri's Read/Recv operations to read HTTP data
/// 2. Parse requests using the shared HTTP codec
/// 3. Dispatch through the Harrow middleware + routing pipeline
/// 4. Use meguri's Write/Send operations to write responses
pub(crate) async fn handle_connection(
    _fd: std::os::fd::RawFd,
    _shared: Arc<SharedState>,
) -> std::io::Result<()> {
    // Placeholder: full implementation will use meguri ops.
    Ok(())
}
