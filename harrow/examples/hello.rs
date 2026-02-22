use std::time::Duration;

use harrow::{timeout_middleware, App, AppO11yExt, Request, Response};
use harrow::o11y::O11yConfig;

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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    let addr = "127.0.0.1:3000".parse().unwrap();

    let app = App::new()
        .o11y(O11yConfig::default())
        .middleware(timeout_middleware(Duration::from_secs(30)))
        .get("/", hello)
        .get("/greet/:name", greet)
        .group("/api", |g| {
            g.get("/health", health)
        });

    tracing::info!("starting on {addr}");
    harrow::serve(app, addr).await.unwrap();
}
