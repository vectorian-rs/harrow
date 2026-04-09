//! Comprehensive test server for load testing (Monoio/io_uring backend).
//!
//! This server exposes Harrow features for load testing with the io_uring backend:
//! - Basic routes (GET, POST, PUT, DELETE)
//! - Path parameters
//! - JSON/text responses
//! - Health/liveness/readiness probes
//! - Error responses (404, 405)
//!
//! Note: Some middleware (timeout, request-id, CORS) is not yet available for monoio.
//!
//! Run with: cargo run --example bench_target_monoio --features monoio,json --no-default-features

mod common;

use harrow::runtime::monoio::run;
use harrow::{App, Request, Response};

async fn root(_req: Request) -> Response {
    Response::text("hello from io_uring!")
}

fn main() {
    tracing_subscriber::fmt::init();

    let addr = common::parse_args("bench_target_monoio");

    let app = App::new()
        .default_problem_details()
        .health("/health")
        .liveness("/live")
        .readiness_handler("/ready", common::readiness)
        .get("/", root)
        .get("/users/:id", common::get_user)
        .post("/users", common::create_user)
        .get("/users/:user_id/posts/:post_id", common::get_user_posts)
        .post("/echo", common::echo)
        .put("/echo", common::echo)
        .delete("/echo", common::echo)
        .get("/cpu", common::cpu_intensive);

    tracing::info!("Monoio/io_uring server starting on http://{}", addr);

    if let Err(e) = run(app, addr) {
        tracing::error!("server error: {}", e);
        std::process::exit(1);
    }
}
