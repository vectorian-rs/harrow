//! Warp performance benchmark server.
//!
//! Exposes the same routes as `axum-perf-server` for fair comparison.
//!
//! Routes:
//!   GET /text       -> "ok" (text/plain)
//!   GET /json/1kb   -> ~1KB JSON (10 user objects)
//!   GET /json/10kb  -> ~10KB JSON (100 user objects)
//!   GET /health     -> "ok" (text/plain)
//!
//! Usage: warp-perf-server [--bind ADDR] [--port PORT]

harrow_bench::setup_allocator!();

use std::net::SocketAddr;

use harrow_bench::{USERS_10, USERS_100};
use warp::Filter;

#[tokio::main]
async fn main() {
    let (bind, port) = harrow_bench::parse_bind_port();
    let addr: SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let text = warp::path!("text").and(warp::get()).map(|| "ok");

    let json_1kb = warp::path!("json" / "1kb")
        .and(warp::get())
        .map(|| warp::reply::json(&*USERS_10));

    let json_10kb = warp::path!("json" / "10kb")
        .and(warp::get())
        .map(|| warp::reply::json(&*USERS_100));

    let health = warp::path!("health").and(warp::get()).map(|| "ok");

    let routes = text.or(json_1kb).or(json_10kb).or(health);

    eprintln!("warp-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}]");
    warp::serve(routes).run(addr).await;
}
