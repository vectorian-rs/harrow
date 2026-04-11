use harrow::o11y::{init_telemetry, O11yConfig};
use harrow::{App, AppO11yExt, Request, Response};

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
    let addr = "127.0.0.1:3000".parse().unwrap();

    let config = O11yConfig {
        otlp_traces_endpoint: option_env!("OTLP_ENDPOINT").map(String::from),
        ..O11yConfig::default()
    };

    // Initialize the global tracing subscriber. Hold the guard for the
    // lifetime of the process so the OTLP exporter stays alive.
    let _guard = init_telemetry(config.clone());

    let app = App::new()
        .o11y_middleware(config)
        .get("/", hello)
        .get("/greet/:name", greet)
        .group("/api", |g| g.get("/health", health));

    tracing::info!("starting on {addr}");
    harrow::runtime::tokio::serve(app, addr).await.unwrap();
}
