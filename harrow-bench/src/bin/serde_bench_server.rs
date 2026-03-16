//! Harrow serde benchmark server.
//!
//! Serves text, JSON, and MessagePack endpoints at various payload sizes,
//! organized into feature-isolated groups for per-middleware benchmarking.
//!
//! Groups:
//!   /bare/*          — no middleware (routing + serialization baseline)
//!   /timeout/*       — timeout(30s) middleware
//!   /request-id/*    — request-id middleware
//!   /cors/*          — cors(permissive) middleware
//!   /compression/*   — compression(gzip) middleware
//!   /full/*          — all 4 middleware stacked
//!   /health          — top-level health check
//!
//! Optional `--o11y` flag enables observability middleware globally.
//!
//! Usage: serde-bench-server [--bind ADDR] [--port PORT] [--o11y]

use std::time::Duration;

use harrow::{App, Group, Request, Response};
use harrow::{
    compression_middleware, cors_middleware, request_id_middleware, timeout_middleware, CorsConfig,
};
use harrow_bench::{
    json_1kb_typed_handler, json_10kb_typed_handler, json_small_handler, msgpack_1kb_handler,
    msgpack_10kb_handler, msgpack_small_handler, text_handler,
};

fn parse_args() -> (String, u16, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut o11y = false;
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
            "--o11y" => {
                o11y = true;
                i += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: serde-bench-server [--bind ADDR] [--port PORT] [--o11y]");
                std::process::exit(1);
            }
        }
    }
    (bind, port, o11y)
}

/// Register the standard bench endpoints on a group.
fn register_group(g: Group) -> Group {
    g.get("/text", text_handler)
        .get("/json/small", json_small_handler)
        .get("/json/1kb", json_1kb_typed_handler)
        .get("/json/10kb", json_10kb_typed_handler)
        .get("/msgpack/small", msgpack_small_handler)
        .get("/msgpack/1kb", msgpack_1kb_handler)
        .get("/msgpack/10kb", msgpack_10kb_handler)
}

#[tokio::main]
async fn main() {
    let (bind, port, o11y) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let cors = CorsConfig::default();

    let mut app = App::new()
        // Backward-compat: flat routes (no middleware)
        .get("/text", text_handler)
        .get("/json/small", json_small_handler)
        .get("/json/1kb", json_1kb_typed_handler)
        .get("/json/10kb", json_10kb_typed_handler)
        .get("/msgpack/small", msgpack_small_handler)
        .get("/msgpack/1kb", msgpack_1kb_handler)
        .get("/msgpack/10kb", msgpack_10kb_handler)
        .get("/health", health)
        // Feature-isolated groups
        .group("/bare", register_group)
        .group("/timeout", |g| {
            register_group(g.middleware(timeout_middleware(Duration::from_secs(30))))
        })
        .group("/request-id", |g| {
            register_group(g.middleware(request_id_middleware))
        })
        .group("/cors", |g| {
            register_group(g.middleware(cors_middleware(cors)))
        })
        .group("/compression", |g| {
            register_group(g.middleware(compression_middleware))
        })
        .group("/full", |g| {
            register_group(
                g.middleware(timeout_middleware(Duration::from_secs(30)))
                    .middleware(request_id_middleware)
                    .middleware(cors_middleware(CorsConfig::default()))
                    .middleware(compression_middleware),
            )
        });

    if o11y {
        let otlp_endpoint =
            std::env::var("OTLP_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4318".to_string());

        use harrow::AppO11yExt;
        use harrow_o11y::O11yConfig;

        app = app.o11y(
            O11yConfig::default()
                .service_name("harrow-bench-o11y")
                .service_version("0.2.0")
                .environment("bench")
                .otlp_traces_endpoint(otlp_endpoint),
        );
        eprintln!("o11y enabled");
    }

    eprintln!("serde-bench-server listening on {addr}");
    harrow::serve(app, addr).await.unwrap();
}

async fn health(_req: Request) -> Response {
    Response::text("ok")
}
