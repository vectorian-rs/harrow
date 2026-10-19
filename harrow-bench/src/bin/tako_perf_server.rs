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
//! Modes:
//!   default        Tako's `Server` on a multi-threaded Tokio runtime
//!   --per-thread   `tako::serve_per_thread`: one current-thread runtime per
//!                  core behind `SO_REUSEPORT`, Tako's thread-per-core
//!                  reference for the Hyper prototype comparison (#88)
//!
//! Usage: tako-perf-server [--bind ADDR] [--port PORT] [--per-thread]

harrow_bench::setup_allocator!();

use harrow_bench::{USERS_10, USERS_100};
use tako::Method;
use tako::extractors::json::Json;
use tako::responder::Responder;
use tako::router::Router;
use tako::types::Request;

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

fn router() -> Router {
    let mut router = Router::new();
    router.route(Method::GET, "/text", text_handler);
    router.route(Method::GET, "/json/1kb", json_1kb_handler);
    router.route(Method::GET, "/json/10kb", json_10kb_handler);
    router.route(Method::GET, "/health", health_handler);
    router
}

fn parse_args() -> (String, u16, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut per_thread = false;
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
            "--per-thread" => {
                per_thread = true;
                i += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: tako-perf-server [--bind ADDR] [--port PORT] [--per-thread]");
                std::process::exit(1);
            }
        }
    }
    (bind, port, per_thread)
}

fn main() {
    let (bind, port, per_thread) = parse_args();
    let addr = format!("{bind}:{port}");
    let mode = if per_thread { "per-thread" } else { "default" };

    eprintln!("tako-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}, mode: {mode}]");
    if per_thread {
        tako::serve_per_thread(&addr, router(), tako::PerThreadConfig::default())
            .expect("tako per-thread server");
    } else {
        tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(async {
                let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
                tako::Server::builder()
                    .build()
                    .try_spawn_http(listener, router())
                    .expect("tako server")
                    .shutdown_on_signal()
                    .await
                    .expect("tako signal handler");
            });
    }
}
