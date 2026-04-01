//! Axum performance benchmark server.
//!
//! Exposes the same flat route corpus as `harrow-perf-server`.
//!
//! Optional `--compression` flag enables response compression middleware.
//!
//! Usage: axum-perf-server [--bind ADDR] [--port PORT] [--compression]

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ALLOCATOR_NAME: &str = if cfg!(feature = "mimalloc") {
    "mimalloc"
} else {
    "system"
};

use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get};
use harrow_bench::{SMALL_PAYLOAD, USERS_10, USERS_100};

async fn text_handler() -> &'static str {
    "ok"
}

static TEXT_128KB: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| "A".repeat(128 * 1024));
static TEXT_256KB: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| "B".repeat(256 * 1024));
static TEXT_512KB: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| "C".repeat(512 * 1024));
static TEXT_1MB: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "D".repeat(1024 * 1024));

async fn text_128kb_handler() -> String {
    TEXT_128KB.clone()
}
async fn text_256kb_handler() -> String {
    TEXT_256KB.clone()
}
async fn text_512kb_handler() -> String {
    TEXT_512KB.clone()
}
async fn text_1mb_handler() -> String {
    TEXT_1MB.clone()
}

async fn echo_body_handler(body: axum::body::Bytes) -> axum::body::Bytes {
    body
}

async fn json_small_handler() -> impl IntoResponse {
    Json(&*SMALL_PAYLOAD)
}

async fn json_1kb_handler() -> impl IntoResponse {
    Json(&*USERS_10)
}

async fn json_10kb_handler() -> impl IntoResponse {
    Json(&*USERS_100)
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

fn parse_args() -> (String, u16, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut compression = false;
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
            "--compression" => {
                compression = true;
                i += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: axum-perf-server [--bind ADDR] [--port PORT] [--compression]");
                std::process::exit(1);
            }
        }
    }
    (bind, port, compression)
}

#[tokio::main]
async fn main() {
    let (bind, port, compression) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let mut app = Router::new()
        .route("/text", get(text_handler))
        .route("/text/128kb", get(text_128kb_handler))
        .route("/text/256kb", get(text_256kb_handler))
        .route("/text/512kb", get(text_512kb_handler))
        .route("/text/1mb", get(text_1mb_handler))
        .route("/echo", axum::routing::post(echo_body_handler))
        .route("/json/small", get(json_small_handler))
        .route("/json/1kb", get(json_1kb_handler))
        .route("/json/10kb", get(json_10kb_handler))
        .route("/msgpack/small", get(msgpack_small_handler))
        .route("/msgpack/1kb", get(msgpack_1kb_handler))
        .route("/msgpack/10kb", get(msgpack_10kb_handler))
        .route("/health", get(health));

    if compression {
        use tower_http::compression::CompressionLayer;
        app = app.layer(CompressionLayer::new());
        eprintln!("compression middleware enabled");
    }

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("axum-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}]");
    axum::serve(listener, app).await.unwrap();
}
