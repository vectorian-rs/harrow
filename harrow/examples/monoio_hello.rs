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

// Use the explicit runtime module for monoio
use harrow::runtime::monoio::run;
use harrow::{App, Request, Response};

async fn hello(_req: Request) -> Response {
    Response::text("hello from io_uring!")
}

async fn health(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "status": "ok" }))
}

fn main() {
    tracing_subscriber::fmt::init();

    let app = App::new().get("/", hello).get("/health", health);
    let addr = "127.0.0.1:3000".parse().unwrap();

    tracing::info!("starting monoio server on http://{}", addr);
    tracing::info!("note: this requires Linux kernel 6.1+");

    if let Err(e) = run(app, addr) {
        tracing::error!("server error: {}", e);
        std::process::exit(1);
    }
}
