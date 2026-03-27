//! Monoio-based HTTP server example.
//!
//! This example demonstrates using the io_uring backend with thread-per-core.
//!
//! # Requirements
//! - Linux kernel 6.1+ for full io_uring support
//! - Run with: `cargo run --example monoio_hello --features monoio --no-default-features`
//!
//! # Limitations
//! - Linux only (io_uring is a Linux kernel interface)
//! - Blocked by default in Docker/containers (needs custom seccomp profile)
//! - Different runtime setup than Tokio (see main() below)

// Use the explicit runtime module for monoio
use harrow::runtime::monoio::serve;
use harrow::{App, Request, Response};

async fn hello(_req: Request) -> Response {
    Response::text("hello from io_uring!")
}

async fn health(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "status": "ok" }))
}

fn main() {
    // Monoio requires a specific runtime setup.
    // FusionDriver tries io_uring first, falls back to epoll if unavailable.
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .enable_timer()
        .build()
        .expect("failed to create monoio runtime");

    rt.block_on(async {
        // Initialize logging
        tracing_subscriber::fmt::init();

        let app = App::new().get("/", hello).get("/health", health);

        let addr = "127.0.0.1:3000".parse().unwrap();

        tracing::info!("starting monoio server on http://{}", addr);
        tracing::info!("note: this requires Linux kernel 6.1+");

        // Use the explicit runtime::monoio module when both server features
        // might be enabled, or when only monoio is enabled
        if let Err(e) = serve(app, addr).await {
            tracing::error!("server error: {}", e);
            std::process::exit(1);
        }
    });
}
