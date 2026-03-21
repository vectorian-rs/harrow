# Harrow

A thin, macro-free HTTP framework over Hyper with built-in observability.

## Features

- **No macros, no magic** -- handlers are plain `async fn(Request) -> Response` functions. No extractors, no trait bounds, no `#[debug_handler]`.
- **Route introspection** -- the route table is a first-class data structure you can enumerate at startup for OpenAPI generation, health checks, or monitoring config.
- **Built-in observability** -- structured logging, OTLP trace export, and request-id propagation are wired in with one call, powered by [rolly](https://github.com/l1x/rolly).
- **Feature-gated middleware** -- timeout, request-id, CORS, catch-panic, compression, and o11y are opt-in via Cargo features. Nothing compiles unless you ask for it.
- **Fast** -- built directly on Hyper 1.x and matchit routing. No Tower, no `BoxCloneService`, no deep type nesting.

## Quickstart

```toml
[dependencies]
harrow = { version = "0.2", features = ["timeout"] }
tokio = { version = "1", features = ["full"] }
```

```rust
use std::time::Duration;
use harrow::{App, Request, Response, timeout_middleware};

async fn hello(_req: Request) -> Response {
    Response::text("hello, world")
}

async fn greet(req: Request) -> Response {
    let name = req.param("name");
    Response::text(format!("hello, {name}"))
}

#[tokio::main]
async fn main() {
    let app = App::new()
        .middleware(timeout_middleware(Duration::from_secs(30)))
        .health("/health")
        .get("/", hello)
        .get("/greet/:name", greet)
        .group("/api", |g| g.get("/greet/:name", greet));

    let addr = "127.0.0.1:3000".parse().unwrap();
    harrow::serve(app, addr).await.unwrap();
}
```

## Probes And Error Responses

```rust
use harrow::{App, ProblemDetail, Request, Response};
use http::StatusCode;

async fn readiness(req: Request) -> Result<Response, ProblemDetail> {
    req.require_state::<String>().map_err(|_| {
        ProblemDetail::new(StatusCode::SERVICE_UNAVAILABLE)
            .detail("database unavailable"),
    })?;

    Ok(Response::text("ready"))
}

let app = App::new()
    .state("primary-db".to_string())
    .health("/health")
    .liveness("/live")
    .readiness_handler("/ready", readiness)
    .default_problem_details()
    .not_found_handler(|req| async move {
        ProblemDetail::new(StatusCode::NOT_FOUND)
            .detail(format!("no route for {}", req.path()))
    });
```

## Documentation

- [Design rationale](docs/prds/harrow-http-framework.md) -- why Harrow exists and what it optimises for
- [Explicit extractors philosophy](docs/explicit-extractors.md) -- the design choice behind plain function signatures
- [Performance notes](docs/performance.md) -- benchmark methodology and results

## Workspace layout

| Crate | Purpose |
|-------|---------|
| `harrow` | Public API -- re-exports core types and feature-gated middleware |
| `harrow-core` | Request, Response, routing, middleware trait, app builder |
| `harrow-middleware` | Timeout, request-id, CORS, compression, o11y middleware |
| `harrow-o11y` | O11yConfig and rolly integration types |
| `harrow-server` | Hyper server binding, TLS, graceful shutdown |
| `harrow-bench` | Criterion benchmarks and load testing tools |

## License

Licensed under MIT or Apache-2.0.
