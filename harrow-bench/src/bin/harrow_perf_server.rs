//! Harrow performance benchmark server.
//!
//! Exposes the same flat route corpus as `axum-perf-server` so routing and
//! serialization comparisons are structurally matched.
//!
//! Routes:
//!   /text
//!   /json/small
//!   /json/1kb
//!   /json/10kb
//!   /msgpack/small
//!   /msgpack/1kb
//!   /msgpack/10kb
//!   /health
//!
//! With `--session`:
//!   /session/noop
//!   /session/set
//!   /session/get
//!   /session/write
//!
//! Optional `--o11y` flag enables observability middleware globally.
//!
//! Usage: harrow-perf-server [--bind ADDR] [--port PORT] [--o11y] [--session]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use harrow::{App, Request, Response};
use harrow_bench::{
    json_1kb_typed_handler, json_10kb_typed_handler, json_small_handler, msgpack_1kb_handler,
    msgpack_10kb_handler, msgpack_small_handler, text_handler,
};

fn parse_args() -> (String, u16, bool, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut o11y = false;
    let mut session = false;
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
            "--session" => {
                session = true;
                i += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!(
                    "usage: harrow-perf-server [--bind ADDR] [--port PORT] [--o11y] [--session]"
                );
                std::process::exit(1);
            }
        }
    }
    (bind, port, o11y, session)
}

#[tokio::main]
async fn main() {
    let (bind, port, o11y, session) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let mut app = App::new()
        .get("/text", text_handler)
        .get("/json/small", json_small_handler)
        .get("/json/1kb", json_1kb_typed_handler)
        .get("/json/10kb", json_10kb_typed_handler)
        .get("/msgpack/small", msgpack_small_handler)
        .get("/msgpack/1kb", msgpack_1kb_handler)
        .get("/msgpack/10kb", msgpack_10kb_handler)
        .get("/health", health);

    if session {
        use harrow::{InMemorySessionStore, session_middleware};
        use harrow_bench::{
            bench_session_config, seed_bench_session, session_get_handler, session_noop_handler,
            session_set_handler, session_write_handler,
        };

        let store = InMemorySessionStore::new();
        seed_bench_session(&store).await;

        app = app
            .middleware(session_middleware(store, bench_session_config()))
            .get("/session/noop", session_noop_handler)
            .get("/session/set", session_set_handler)
            .get("/session/get", session_get_handler)
            .get("/session/write", session_write_handler);

        eprintln!("session middleware enabled (4 session routes)");
    }

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

    eprintln!("harrow-perf-server listening on {addr}");
    harrow::serve_with_config(
        app,
        addr,
        std::future::pending(),
        harrow::ServerConfig {
            header_read_timeout: None,
            connection_timeout: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn health(_req: Request) -> Response {
    Response::text("ok")
}
