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

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ALLOCATOR_NAME: &str = if cfg!(feature = "mimalloc") {
    "mimalloc"
} else {
    "system"
};

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

fn parse_args() -> (String, u16) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
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
                eprintln!("usage: tako-perf-server [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    (bind, port)
}

#[tokio::main]
async fn main() {
    let (bind, port) = parse_args();
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
