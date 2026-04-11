//! Harrow Monoio performance benchmark server.
//!
//! Exposes the same flat route corpus as `harrow-perf-server` so remote
//! framework comparisons stay structurally matched when the monoio backend is
//! selected.
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
//! Optional `--compression` flag enables response compression middleware.
//!
//! Usage: harrow-server-monoio [--bind ADDR] [--port PORT] [--o11y] [--session] [--compression]

harrow_bench::setup_allocator!();

use harrow::{App, Request, Response};
use harrow_bench::{
    json_1kb_typed_handler, json_10kb_typed_handler, json_small_handler, msgpack_1kb_handler,
    msgpack_10kb_handler, msgpack_small_handler, text_handler,
};

fn parse_args() -> (String, u16, bool, bool, bool) {
    let args: Vec<String> = std::env::args().collect();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut o11y = false;
    let mut session = false;
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
            "--o11y" => {
                o11y = true;
                i += 1;
            }
            "--session" => {
                session = true;
                i += 1;
            }
            "--compression" => {
                compression = true;
                i += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!(
                    "usage: harrow-server-monoio [--bind ADDR] [--port PORT] [--o11y] [--session] [--compression]"
                );
                std::process::exit(1);
            }
        }
    }
    (bind, port, o11y, session, compression)
}

fn main() {
    let (bind, port, o11y, session, compression) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let mut app = App::new()
        .get("/text", text_handler)
        .get("/text/128kb", harrow_bench::text_128kb_handler)
        .get("/text/256kb", harrow_bench::text_256kb_handler)
        .get("/text/512kb", harrow_bench::text_512kb_handler)
        .get("/text/1mb", harrow_bench::text_1mb_handler)
        .post("/echo", harrow_bench::echo_body_handler)
        .get("/json/small", json_small_handler)
        .get("/json/1kb", json_1kb_typed_handler)
        .get("/json/10kb", json_10kb_typed_handler)
        .get("/msgpack/small", msgpack_small_handler)
        .get("/msgpack/1kb", msgpack_1kb_handler)
        .get("/msgpack/10kb", msgpack_10kb_handler)
        .get("/health", health);

    if session {
        use harrow::session_middleware;
        use harrow_bench::{
            InMemorySessionStore, bench_session_config, seed_bench_session, session_get_handler,
            session_noop_handler, session_set_handler, session_write_handler,
        };

        let store = InMemorySessionStore::new();
        let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
            .enable_timer()
            .build()
            .expect("failed to create monoio runtime for session seeding");
        rt.block_on(async {
            seed_bench_session(&store).await;
        });

        app = app
            .middleware(session_middleware(store, bench_session_config()))
            .get("/session/noop", session_noop_handler)
            .get("/session/set", session_set_handler)
            .get("/session/get", session_get_handler)
            .get("/session/write", session_write_handler);

        eprintln!("session middleware enabled (4 session routes)");
    }

    if compression {
        app = app.middleware(harrow::compression_middleware);
        eprintln!("compression middleware enabled");
    }

    if o11y {
        let otlp_endpoint =
            std::env::var("OTLP_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4318".to_string());

        use harrow::AppO11yExt;
        use harrow::o11y::init_telemetry;
        use harrow_o11y::O11yConfig;

        let config = O11yConfig::default()
            .service_name("harrow-bench-o11y")
            .service_version("0.2.0")
            .environment("bench")
            .otlp_traces_endpoint(otlp_endpoint);

        // Leak the guard so it lives for the process lifetime.
        let guard = init_telemetry(config.clone());
        std::mem::forget(guard);

        app = app.o11y_middleware(config);
        eprintln!("o11y enabled");
    }

    let io_driver = harrow::runtime::monoio::detect_io_driver();
    eprintln!(
        "harrow-server-monoio listening on {addr} [allocator: {ALLOCATOR_NAME}, io: {io_driver}]"
    );
    harrow::runtime::monoio::run_with_config(
        app,
        addr,
        harrow::runtime::monoio::ServerConfig {
            header_read_timeout: None,
            connection_timeout: None,
            ..Default::default()
        },
    )
    .unwrap();
}

async fn health(_req: Request) -> Response {
    Response::text("ok")
}
