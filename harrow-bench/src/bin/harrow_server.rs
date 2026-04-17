//! Minimal Harrow server for framework comparison benchmarks.
//!
//! No o11y, no timeout middleware — raw framework overhead only.
//! Usage: harrow-server-tokio [--bind ADDR] [--port PORT]

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use harrow::{App, Request, Response};

async fn hello(_req: Request) -> Response {
    Response::text("hello, world")
}

async fn greet(req: Request) -> Response {
    let name = req.param("name");
    Response::text(format!("hello, {name}"))
}

async fn health(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "status": "ok" }))
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
                eprintln!("usage: harrow-server-tokio [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    (bind, port)
}

fn main() {
    let (bind, port) = parse_args();
    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().unwrap();

    let app = || {
        App::new()
            .get("/", hello)
            .get("/greet/:name", greet)
            .get("/health", health)
    };

    eprintln!("harrow listening on {addr}");
    harrow::runtime::tokio::serve_multi_worker(
        app,
        addr,
        harrow::runtime::tokio::ServerConfig {
            header_read_timeout: None,
            connection_timeout: None,
            ..Default::default()
        },
    )
    .unwrap();
}
