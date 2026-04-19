# Harrow

A thin, macro-free HTTP framework with custom HTTP/1 backends, local-worker
runtime architecture, and opt-in observability.

## Features

- **No macros, no magic** -- handlers are plain `async fn(Request) -> Response` functions. No extractors, no trait bounds, no `#[debug_handler]`.
- **Route introspection** -- the route table is a first-class data structure you can enumerate at startup for OpenAPI generation, health checks, or monitoring config.
- **Opt-in observability** -- structured logging, OTLP trace export, and request-id propagation are wired in with one call, powered by [rolly](https://github.com/l1x/rolly).
- **Feature-gated middleware** -- request-id, CORS, catch-panic, compression, session, rate-limit, and o11y are opt-in via Cargo features. Nothing compiles unless you ask for it.
- **Fast** -- custom HTTP/1 transport with shared codec/dispatcher pieces, `matchit` routing, and no Tower or `BoxCloneService`.
- **Pluggable server backends** -- choose between Tokio/custom HTTP/1 (cross-platform) or Monoio/io_uring (Linux high-performance).

## Server Backends (Required)

Harrow requires you to explicitly select an HTTP server backend. There is no default — you must pick exactly one:

| Backend               | Feature  | Best For                                | Platform              |
| --------------------- | -------- | --------------------------------------- | --------------------- |
| **Tokio + custom HTTP/1** | `tokio`  | Cross-platform, development, containers | Linux, macOS, Windows |
| **Monoio + io_uring**     | `monoio` | Maximum throughput on Linux 6.1+        | Linux 6.1+ only       |

### Choosing a Backend

**Use Tokio** for:

- Cross-platform development (macOS, Windows)
- Container deployments (Docker, ECS Fargate, Lambda)
- When you need TLS support (`tls` feature)
- General-purpose HTTP services with the same local-worker/runtime direction as
  Harrow's other backends

**Use Monoio** for:

- High-throughput Linux servers (2-3x throughput at 16+ cores)
- Thread-per-core architecture with io_uring
- Bare metal or EC2 deployments with kernel 6.1+

### Configuration

```toml
# Tokio backend (cross-platform)
[dependencies]
harrow = { version = "0.9", features = ["tokio", "json"] }
tokio = { version = "1", features = ["full"] }  # Required for #[tokio::main] and tokio APIs

# io_uring backend (Linux 6.1+ only)
[dependencies]
harrow = { version = "0.9", features = ["monoio", "json"] }
# Harrow bootstraps monoio worker threads internally via `harrow::runtime::monoio::run(...)`
```

### Explicit Runtime Selection

When both features are enabled (e.g., during development with multiple examples), use the explicit runtime modules:

```rust
// Explicit Tokio
use harrow::runtime::tokio::serve;

// Explicit Monoio
use harrow::runtime::monoio::run;
```

See [`examples/monoio_hello.rs`](harrow/examples/monoio_hello.rs) for a complete Monoio example.

## Quickstart

```toml
[dependencies]
harrow = { version = "0.9", features = ["tokio"] }
tokio = { version = "1", features = ["full"] }  # Required for #[tokio::main]
```

```rust
use harrow::{App, Request, Response};

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
- [Local-worker runtime strategy](docs/strategy-local-workers.md) -- the current nginx/ntex-style runtime direction
- [HTTP/1 dispatcher design](docs/h1-dispatcher-design.md) -- how the shared custom backend is structured
- [Explicit extractors philosophy](docs/explicit-extractors.md) -- the design choice behind plain function signatures
- [Performance notes](docs/performance.md) -- current benchmark workflow, methodology, and historical results

## Workspace layout

| Crate                  | Purpose                                                          |
| ---------------------- | ---------------------------------------------------------------- |
| `harrow`               | Public API -- re-exports core types and feature-gated middleware |
| `harrow-core`          | Request, Response, routing, middleware trait, app builder        |
| `harrow-middleware`    | Request-id, CORS, compression, session, rate-limit, o11y        |
| `harrow-o11y`          | O11yConfig and rolly integration types                           |
| `harrow-server-tokio`  | Tokio custom HTTP/1 backend, local-worker runtime, TLS, graceful shutdown |
| `harrow-server-monoio` | Monoio/io_uring server for high-performance Linux                |
| `harrow-server-meguri` | Experimental direct io_uring backend                             |
| `harrow-bench`         | Criterion benchmarks and load testing tools                      |

## License

Licensed under the [MIT License](LICENSE).
