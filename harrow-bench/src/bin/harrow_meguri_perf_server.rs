//! Harrow Meguri (io_uring) performance benchmark server.
//!
//! Same route corpus as the tokio and monoio perf servers.
//! Linux only — requires io_uring (kernel 5.6+).
//!
//! Usage: harrow-meguri-perf-server [--bind ADDR] [--port PORT]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("harrow-server-meguri requires Linux (io_uring). Exiting.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod server {
    harrow_bench::setup_allocator!();

    use harrow::{App, Request, Response};
    use harrow_bench::{
        json_1kb_typed_handler, json_10kb_typed_handler, json_small_handler, msgpack_1kb_handler,
        msgpack_10kb_handler, msgpack_small_handler, text_handler,
    };

    fn parse_args() -> (String, u16) {
        let args: Vec<String> = std::env::args().collect();
        let mut bind = "127.0.0.1".to_string();
        let mut port: u16 = 3090;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--bind" => {
                    bind = args[i + 1].clone();
                    i += 2;
                }
                "--port" => {
                    port = args[i + 1].parse().expect("invalid port");
                    i += 2;
                }
                _ => i += 1,
            }
        }
        (bind, port)
    }

    pub fn run() {
        let (bind, port) = parse_args();
        let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

        let app = || {
            App::new()
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
                .get("/health", health)
        };

        eprintln!("harrow-meguri-perf-server listening on {addr} [allocator: {ALLOCATOR_NAME}]");
        harrow_server_meguri::run_with_config(
            app,
            addr,
            harrow_server_meguri::ServerConfig::default(),
        )
        .unwrap();
    }

    async fn health(_req: Request) -> Response {
        Response::text("ok")
    }
}

#[cfg(target_os = "linux")]
fn main() {
    server::run();
}
