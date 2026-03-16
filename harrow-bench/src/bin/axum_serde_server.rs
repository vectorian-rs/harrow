//! Axum serde benchmark server.
//!
//! Same endpoints and payloads as serde_bench_server for framework comparison.
//! No o11y — raw serialization throughput only.
//!
//! Usage: axum-serde-server [--bind ADDR] [--port PORT]

use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get};
use harrow_bench::{SMALL_PAYLOAD, USERS_10, USERS_100};

async fn text_handler() -> &'static str {
    "ok"
}

async fn json_small_handler() -> Json<serde_json::Value> {
    Json(serde_json::to_value(&*SMALL_PAYLOAD).unwrap())
}

async fn json_1kb_handler() -> Json<serde_json::Value> {
    Json(serde_json::to_value(&*USERS_10).unwrap())
}

async fn json_10kb_handler() -> Json<serde_json::Value> {
    Json(serde_json::to_value(&*USERS_100).unwrap())
}

async fn msgpack_small_handler() -> impl IntoResponse {
    let bytes = rmp_serde::to_vec(&*SMALL_PAYLOAD).unwrap();
    ([(CONTENT_TYPE, "application/msgpack")], bytes)
}

async fn msgpack_1kb_handler() -> impl IntoResponse {
    let bytes = rmp_serde::to_vec(&*USERS_10).unwrap();
    ([(CONTENT_TYPE, "application/msgpack")], bytes)
}

async fn msgpack_10kb_handler() -> impl IntoResponse {
    let bytes = rmp_serde::to_vec(&*USERS_100).unwrap();
    ([(CONTENT_TYPE, "application/msgpack")], bytes)
}

async fn health() -> &'static str {
    "ok"
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
                eprintln!("usage: axum-serde-server [--bind ADDR] [--port PORT]");
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

    let bare_routes = Router::new()
        .route("/text", get(text_handler))
        .route("/json/1kb", get(json_1kb_handler))
        .route("/msgpack/1kb", get(msgpack_1kb_handler));

    let app = Router::new()
        .route("/text", get(text_handler))
        .route("/json/small", get(json_small_handler))
        .route("/json/1kb", get(json_1kb_handler))
        .route("/json/10kb", get(json_10kb_handler))
        .route("/msgpack/small", get(msgpack_small_handler))
        .route("/msgpack/1kb", get(msgpack_1kb_handler))
        .route("/msgpack/10kb", get(msgpack_10kb_handler))
        .route("/health", get(health))
        .nest("/bare", bare_routes);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("axum-serde-server listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
