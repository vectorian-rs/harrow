# Harrow

A thin, macro-free HTTP framework with explicit server backends, local-worker
runtime architecture, and opt-in observability.

## Features

- **No macros, no magic** -- handlers are plain `async fn(Request) -> Response` functions. No extractors, no trait bounds, no `#[debug_handler]`.
- **Route introspection** -- the route table is a first-class data structure you can enumerate at startup for OpenAPI generation, health checks, or monitoring config.
- **Opt-in observability** -- structured logging, OTLP trace export, and request-id propagation are wired in with one call, powered by [rolly](https://github.com/l1x/rolly).
- **Feature-gated middleware** -- request-id, CORS, catch-panic, body-limit, compression, session, rate-limit, security headers, and o11y are opt-in via Cargo features. Nothing compiles unless you ask for it.
- **Fast** -- `matchit` routing, no Tower or `BoxCloneService`, and backend work focused on local-worker/thread-per-core performance.
- **Pluggable server backends** -- choose between Tokio/custom HTTP/1 (cross-platform today), Tokio/Hyper (`tokio-hyper`, prototype), or Monoio/io_uring (Linux high-performance).

## Server Backends

To run a server, explicitly select an HTTP backend. There is no default — the
public `harrow` crate exposes the application/core APIs without a backend, and
server entrypoints appear when you enable one:

| Backend               | Feature  | Best For                                | Platform              |
| --------------------- | -------- | --------------------------------------- | --------------------- |
| **Tokio + custom HTTP/1** | `tokio`  | Cross-platform today; stable 1.0 status under review | Linux, macOS, Windows |
| **Tokio + Hyper**         | `tokio-hyper` | Prototype to compare Hyper + thread-per-core vs custom H1 | Linux, macOS, Windows |
| **Monoio + io_uring**     | `monoio` | Maximum throughput on Linux 6.1+; parity evidence pending | Linux 6.1+ only       |

### Choosing a Backend

**Use Tokio** for:

- Cross-platform development (macOS, Windows)
- Container deployments (Docker, ECS Fargate, Lambda)
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
harrow = { version = "0.10", features = ["tokio", "json"] }
tokio = { version = "1", features = ["full"] }  # Required for #[tokio::main] and tokio APIs

# io_uring backend (Linux 6.1+ only)
[dependencies]
harrow = { version = "0.10", features = ["monoio", "json"] }
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

The custom H1 stack remains an important reference and performance path. Harrow also includes a Hyper-based Tokio prototype (`tokio-hyper`) to reduce protocol maintenance risk if performance is close enough. See [Protocol Backend Strategy](./docs/architecture/protocol/backend-strategy.md).

For advanced Monoio lifecycle control (`start`, `ServerHandle`, async `serve*`
entrypoints), depend on `harrow-server-monoio` directly. The root `harrow`
crate intentionally exposes only the smaller `run` / `run_with_config` surface.

## Quickstart

```toml
[dependencies]
harrow = { version = "0.10", features = ["tokio"] }
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

- [Docs index](./docs/) -- current docs map and reading order
- [Overview](./docs/overview/) -- product identity, feature status, and roadmap
- [Product](./docs/product/) -- Harrow 1.0 product scope and release criteria
- [Architecture](./docs/architecture/) -- runtime model, protocol strategy, and public API design
- [Local-worker webservers](./docs/architecture/runtime-model/local-worker-webservers.md) -- Harrow's nginx/`ntex`-inspired async/runtime direction
- [Server](./docs/server/) -- backend support, lifecycle, timeouts, limits, and connection safety
- [Middleware](./docs/middleware/) -- middleware model, security, and observability
- [Operations](./docs/operations/) -- deployment, performance workflow, and verification
- [History](./docs/history/) -- engineering journal and historical design archaeology

## Workspace layout

| Crate                  | Purpose                                                          |
| ---------------------- | ---------------------------------------------------------------- |
| `harrow`               | Public API -- re-exports core types and feature-gated middleware |
| `harrow-core`          | Request, Response, routing, middleware trait, app builder        |
| `harrow-middleware`    | Request-id, CORS, catch-panic, body-limit, compression, session, rate-limit, security headers, o11y |
| `harrow-o11y`          | O11yConfig and rolly integration types                           |
| `harrow-serde`         | JSON and MessagePack helpers behind feature flags                |
| `harrow-codec-h1`      | HTTP/1.1 codec: request parsing, response serialization, chunked encoding |
| `harrow-io`            | Runtime-agnostic I/O primitives: buffer pool and I/O traits      |
| `harrow-server`        | Shared server primitives: listener, workers, shutdown, config    |
| `harrow-server-tokio`  | Tokio custom HTTP/1 backend, local-worker runtime, graceful shutdown |
| `harrow-server-tokio-hyper` | Prototype Tokio backend with Hyper owning HTTP/1 parsing and framing |
| `harrow-server-monoio` | Monoio/io_uring server for high-performance Linux                |
| `harrow-server-meguri` | Experimental direct io_uring backend                             |
| `meguri`               | Tokio-free io_uring library used by `harrow-server-meguri`       |
| `harrow-bench`         | Criterion benchmarks and load testing tools                      |

## License

Licensed under the [MIT License](LICENSE).
