//! Meguri-based HTTP server for Harrow.
//!
//! This crate provides an io_uring-backed HTTP server using the `meguri`
//! library. It is a pure io_uring implementation without Tokio dependencies.
//!
//! # Platform
//!
//! **Linux only.** io_uring is a Linux kernel feature. This crate will not
//! compile on macOS, Windows, or BSD.
//!
//! # Example
//!
//! ```ignore
//! use harrow_core::route::App;
//! use harrow_server_meguri::serve;
//!
//! async fn hello(_req: harrow_core::request::Request) -> harrow_core::response::Response {
//!     harrow_core::response::Response::text("hello")
//! }
//!
//! fn main() {
//!     let app = App::new().get("/hello", hello);
//!     let addr = "127.0.0.1:3000".parse().unwrap();
//!     serve(app, addr).unwrap();
//! }
//! ```

// Linux only
#[cfg(not(target_os = "linux"))]
compile_error!("harrow-server-meguri requires Linux. io_uring is not available on this platform.");

#[cfg(target_os = "linux")]
mod connection;
#[cfg(target_os = "linux")]
mod h1;

#[cfg(target_os = "linux")]
use std::net::SocketAddr;

#[cfg(target_os = "linux")]
use harrow_core::route::App;

/// Server configuration.
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Ring size (number of SQ/CQ entries). Default: 4096.
    pub ring_entries: u32,
    /// Timeout for reading HTTP headers. Default: Some(5s).
    pub header_read_timeout: Option<std::time::Duration>,
    /// Maximum connection lifetime. Default: Some(5 min).
    pub connection_timeout: Option<std::time::Duration>,
    /// Drain timeout during shutdown. Default: 30s.
    pub drain_timeout: std::time::Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            ring_entries: 4096,
            header_read_timeout: Some(std::time::Duration::from_secs(5)),
            connection_timeout: Some(std::time::Duration::from_secs(300)),
            drain_timeout: std::time::Duration::from_secs(30),
        }
    }
}

/// Serve the application on the given address.
///
/// This is a blocking call that runs the io_uring event loop.
pub fn serve(app: App, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_config(app, addr, ServerConfig::default())
}

/// Serve with custom configuration.
pub fn serve_with_config(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let shared = app.into_shared_state();

    shared.route_table.print_routes();

    tracing::info!("harrow (meguri) listening on {addr}");

    // Create the io_uring ring.
    let mut ring = meguri::Ring::new(config.ring_entries)?;

    // Create a TCP listener.
    let listener = create_listener(addr)?;

    // Main event loop.
    // TODO: Implement the full accept/read/dispatch loop using meguri.
    // For now, this is a skeleton that sets up the ring and listener.

    tracing::info!("meguri ring created with {} entries", config.ring_entries);
    tracing::info!("server ready (skeleton — full implementation in progress)");

    // Placeholder: block forever. In production, this would be the accept loop.
    // The full implementation will:
    // 1. Submit Accept SQEs to the ring
    // 2. On completion, spawn a connection handler
    // 3. Connection handlers submit Read/Write SQEs
    // 4. poll_completions() dispatches to wakers
    // 5. On shutdown signal, drain in-flight connections
    loop {
        ring.submit_and_wait(1)?;
        ring.poll_completions();
    }
}

#[cfg(target_os = "linux")]
fn create_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use std::net::TcpListener;
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}
