//! ntex performance benchmark server.
//!
//! Exposes the same routes as `axum-perf-server` for fair comparison.
//!
//! Routes:
//!   GET /text       -> "ok" (text/plain)
//!   GET /json/1kb   -> ~1KB JSON (10 user objects)
//!   GET /json/10kb  -> ~10KB JSON (100 user objects)
//!   GET /health     -> "ok" (text/plain)
//!
//! Usage: ntex-perf-server [--bind ADDR] [--port PORT]

harrow_bench::setup_allocator!();

use harrow_bench::{USERS_10, USERS_100, User};
use ntex::web;

#[web::get("/text")]
async fn text_handler() -> &'static str {
    "ok"
}

#[web::get("/json/1kb")]
async fn json_1kb_handler() -> web::types::Json<&'static Vec<User>> {
    web::types::Json(&*USERS_10)
}

#[web::get("/json/10kb")]
async fn json_10kb_handler() -> web::types::Json<&'static Vec<User>> {
    web::types::Json(&*USERS_100)
}

#[web::get("/health")]
async fn health_handler() -> &'static str {
    "ok"
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let (bind, port) = harrow_bench::parse_bind_port();
    let addr = format!("{bind}:{port}");

    eprintln!("ntex-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}]");

    web::HttpServer::new(async || {
        web::App::new()
            .service(text_handler)
            .service(json_1kb_handler)
            .service(json_10kb_handler)
            .service(health_handler)
    })
    .bind(&addr)?
    .run()
    .await
}
