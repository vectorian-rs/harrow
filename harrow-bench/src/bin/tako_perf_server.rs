//! Tako performance benchmark server.
//!
//! Exposes the same routes as `axum-perf-server` for fair comparison.
//!
//! Routes:
//!   GET /text       -> "ok" (text/plain)
//!   GET /json/1kb   -> ~1KB JSON (10 user objects)
//!   GET /json/10kb  -> ~10KB JSON (100 user objects)
//!   GET /health     -> "ok" (text/plain)
//!
//! Usage: tako-perf-server [--bind ADDR] [--port PORT]

harrow_bench::setup_allocator!();

use harrow_bench::{USERS_10, USERS_100};
use tako::Method;
use tako::extractors::json::Json;
use tako::responder::Responder;
use tako::router::Router;
use tako::types::Request;
use tokio::net::TcpListener;

async fn text_handler(_: Request) -> impl Responder {
    "ok".into_response()
}

async fn json_1kb_handler(_: Request) -> Json<&'static Vec<harrow_bench::User>> {
    Json(&*USERS_10)
}

async fn json_10kb_handler(_: Request) -> Json<&'static Vec<harrow_bench::User>> {
    Json(&*USERS_100)
}

async fn health_handler(_: Request) -> impl Responder {
    "ok".into_response()
}

#[tokio::main]
async fn main() {
    let (bind, port) = harrow_bench::parse_bind_port();
    let addr = format!("{bind}:{port}");

    let listener = TcpListener::bind(&addr).await.unwrap();

    let mut router = Router::new();
    router.route(Method::GET, "/text", text_handler);
    router.route(Method::GET, "/json/1kb", json_1kb_handler);
    router.route(Method::GET, "/json/10kb", json_10kb_handler);
    router.route(Method::GET, "/health", health_handler);

    eprintln!("tako-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}]");
    tako::serve(listener, router).await;
}
