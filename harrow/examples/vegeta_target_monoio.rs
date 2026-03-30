//! Comprehensive test server for Vegeta load testing (Monoio/io_uring backend).
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
//! Run with: cargo run --example vegeta_target_monoio --features monoio,json --no-default-features

use harrow::runtime::monoio::serve;
use harrow::{App, Request, Response};
// http crate available through dev-dependencies

// Basic handlers
async fn root(_req: Request) -> Response {
    Response::text("hello from io_uring!")
}

async fn health(_req: Request) -> Response {
    Response::json(&serde_json::json!({
        "status": "ok",
        "backend": "monoio/io_uring",
    }))
}

async fn liveness(_req: Request) -> Response {
    Response::text("alive")
}

async fn readiness(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "ready": true }))
}

// Path parameter handlers
async fn get_user(req: Request) -> Response {
    let user_id = req.param("id");
    Response::json(&serde_json::json!({
        "id": user_id,
        "name": format!("User {}", user_id),
    }))
}

async fn get_user_posts(req: Request) -> Response {
    let user_id = req.param("user_id");
    let post_id = req.param("post_id");
    Response::json(&serde_json::json!({
        "user_id": user_id,
        "post_id": post_id,
        "title": format!("Post {} by user {}", post_id, user_id),
    }))
}

// POST handlers
async fn create_user(req: Request) -> Response {
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&serde_json::json!({
            "id": 123,
            "created": true,
            "data": body,
        }))
        .status(201),
        Err(_) => Response::text("invalid json").status(400),
    }
}

async fn echo(req: Request) -> Response {
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&body),
        Err(_) => Response::text("invalid json").status(400),
    }
}

// Error handlers
async fn not_found_handler(req: Request) -> Response {
    Response::text(format!("no route for {} {}", req.method(), req.path())).status(404)
}

// CPU-intensive handler - meaningful work to stress CPU
async fn cpu_intensive(_req: Request) -> Response {
    // Perform actual computation to stress CPU
    // Calculate fibonacci(35) recursively (about 29M operations)
    fn fib(n: u32) -> u64 {
        match n {
            0 => 0,
            1 => 1,
            _ => fib(n - 1) + fib(n - 2),
        }
    }

    let result = fib(35);
    Response::json(&serde_json::json!({ "fib": result }))
}

fn parse_args() -> (String, u16) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "0.0.0.0".to_string();
    let mut port: u16 = 3000;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args.get(i + 1).expect("--bind requires an address").clone();
                i += 2;
            }
            "--port" => {
                port = args
                    .get(i + 1)
                    .expect("--port requires a number")
                    .parse()
                    .expect("invalid port number");
                i += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: harrow-monoio-server [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    (bind, port)
}

fn main() {
    tracing_subscriber::fmt::init();

    let (bind, port) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    // Monoio requires its own runtime
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .enable_timer()
        .build()
        .expect("failed to create monoio runtime");

    rt.block_on(async {
        let app = App::new()
            // Default handlers
            .not_found_handler(not_found_handler)
            // Probes
            .health_handler("/health", health)
            .liveness_handler("/live", liveness)
            .readiness_handler("/ready", readiness)
            // Routes
            .get("/", root)
            // User API
            .get("/users/:id", get_user)
            .post("/users", create_user)
            .get("/users/:user_id/posts/:post_id", get_user_posts)
            // Echo/utility
            .post("/echo", echo)
            .put("/echo", echo)
            .delete("/echo", |_req| async move { Response::text("deleted") })
            // Load test scenarios
            .get("/cpu", cpu_intensive);

        tracing::info!("Monoio/io_uring server starting on http://{}", addr);
        tracing::info!("Endpoints:");
        tracing::info!("  GET  /                    - Root/hello");
        tracing::info!("  GET  /health              - Health check (JSON)");
        tracing::info!("  GET  /live                - Liveness probe");
        tracing::info!("  GET  /ready               - Readiness probe");
        tracing::info!("  GET  /users/:id           - Get user (path param)");
        tracing::info!("  POST /users               - Create user (JSON body)");
        tracing::info!("  GET  /users/:user_id/posts/:post_id - Nested params");
        tracing::info!("  POST /echo                - Echo JSON");
        tracing::info!("  PUT  /echo                - Echo JSON");
        tracing::info!("  DELETE /echo              - Delete");
        tracing::info!("  GET  /cpu                 - CPU intensive");

        if let Err(e) = serve(app, addr).await {
            tracing::error!("server error: {}", e);
            std::process::exit(1);
        }
    });
}
