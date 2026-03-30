//! Minimal Axum server for framework comparison benchmarks.
//!
//! Identical endpoints to harrow_server_tokio — raw framework overhead only.
//! Usage: axum-server [--bind ADDR] [--port PORT]

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::{Json, Router, extract::Path, routing::get};
use serde_json::{Value, json};

async fn hello() -> &'static str {
    "hello, world"
}

async fn greet(Path(name): Path<String>) -> String {
    format!("hello, {name}")
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

fn parse_args() -> (String, u16) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
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
                eprintln!("usage: axum-server [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    (bind, port)
}

#[tokio::main]
async fn main() {
    let (bind, port) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let app = Router::new()
        .route("/", get(hello))
        .route("/greet/{name}", get(greet))
        .route("/health", get(health));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("axum listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
