//! Tokio-based HTTP server for Harrow.
//!
//! Uses harrow-codec-h1 for HTTP parsing (no hyper), tokio `current_thread`
//! per worker (no work-stealing), and thread-local buffer pooling
//! (no per-request allocation).

#[cfg(feature = "ws")]
pub mod ws;

mod connection;
mod h1;
mod server;

pub use harrow_server::ServerConfig;
pub use server::{serve, serve_multi_worker, serve_with_config, serve_with_shutdown};

#[doc(hidden)]
pub use connection::handle_connection;
