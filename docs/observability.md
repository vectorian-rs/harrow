# Observability

Harrow's observability model is opt-in. Applications choose the middleware and
runtime integration they need instead of compiling a telemetry stack by default.

## Goals

- structured request logs/traces;
- request ID propagation;
- route labels where possible;
- service/version/environment metadata;
- low overhead when observability features are disabled.

## Feature Flags

Common feature flags:

```toml
harrow = { version = "0.10", features = [
  "tokio",
  "o11y",
  "request-id",
] }
```

`o11y` wires Harrow to the observability support crate and `rolly` integration.
`request-id` adds request ID propagation middleware.

## Minimal Setup

```rust,ignore
use harrow::{App, Request, Response};

async fn hello(_req: Request) -> Response {
    Response::text("hello")
}

#[tokio::main]
async fn main() {
    // Initialize tracing/telemetry according to your runtime and deployment.
    // See the public `harrow::o11y` exports when the `o11y` feature is enabled.

    let app = App::new()
        .middleware(harrow::request_id_middleware)
        .get("/", hello);

    harrow::runtime::tokio::serve(app, "127.0.0.1:3000".parse().unwrap()).await.unwrap();
}
```

## Middleware Ordering

A typical production ordering is:

1. catch panic / error boundary;
2. request ID;
3. observability/tracing;
4. body limits;
5. CORS/security headers/compression as appropriate;
6. application handlers.

Exact ordering depends on what you want traced and which headers should appear
on early error responses.

## Request IDs

The request ID middleware:

- preserves an existing request ID header;
- generates one when missing;
- writes it to the response;
- supports custom header names.

```rust,ignore
let app = App::new()
    .middleware(harrow::request_id_middleware_with_header("x-request-id"));
```

## Route Labels

Harrow keeps route metadata and matched route patterns available. Observability
middleware should prefer route patterns such as `/users/:id` over raw paths such
as `/users/123` to avoid high-cardinality labels.

## Metrics Status

Harrow currently provides observability hooks and middleware integration points,
but it should not claim a full built-in metrics backend. Treat metrics export as
application/runtime configuration: emit structured events/spans from Harrow and
connect them to your collector stack.

## Backend Notes

- Tokio is the best-documented observability path today.
- Monoio should use the same high-level Harrow middleware where possible, with
  backend-specific lifecycle logs from `harrow-server-monoio`.
- Meguri is experimental; observability behavior should not be treated as a
  stable product guarantee.

See also:

- [Deployment](./deployment.md)
- [Server Lifecycle](./server-lifecycle.md)
- [Middleware](./middleware.md)
