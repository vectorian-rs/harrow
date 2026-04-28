# Harrow: A Thin, Macro-Free HTTP Framework

> Historical product/design document. For the current product scope and support
> policy, see [harrow-1.0.md](./harrow-1.0.md).

**Status:** Draft
**Date:** 2026-02-19
**Author:** l1x

---

## 1. Problem Statement

Axum, the dominant Rust HTTP framework built on Hyper, introduces several pain points:

- **Macro magic and type-level gymnastics.** Extractors, handler trait bounds, and the `#[debug_handler]` escape hatch all stem from heavy use of generics and hidden trait implementations. Compile errors are notoriously opaque.
- **Route table opacity.** There is no first-class way to inspect, enumerate, or export the registered route table at runtime or build time. You cannot generate OpenAPI route listings, print a startup summary, or feed routes into monitoring config without external tooling.
- **Observability is bolted on.** Tracing, metrics, and health checks require layering Tower middleware, often with boilerplate that varies per project. There is no unified o11y story out of the box.
- **Abstraction cost.** Tower's `Service` trait, `Layer` composition, `BoxCloneService`, and the resulting deep type nesting add compile-time and cognitive overhead that is not justified for many services.

Harrow aims to be the framework you reach for when you want top-tier Rust HTTP performance with a thin, explicit, zero-macro API surface that treats observability and route introspection as first-class features.

---

## 2. Goals

| Priority | Goal |
|----------|------|
| P0 | Zero proc-macros. All routing and handler wiring is plain Rust function calls. |
| P0 | Route table is a concrete, inspectable data structure available at runtime. |
| P0 | Opt-in structured observability via first-party middleware and extension traits: tracing spans per request, latency histograms, error counters. |
| P0 | Minimal framework overhead over the backend/runtime baseline. Target < 1 us added latency per request on the hot path. |
| P0 | Targeted benchmark runs capture perf records and supporting visualizations so regressions are visible before releases and major changes. |
| P1 | Compile times competitive with or better than Axum for equivalent service definitions. |
| P1 | Clear, human-readable compiler errors. No deeply nested generic bounds. |
| P1 | First-class health check, readiness, and liveness endpoints. |
| P2 | ~~Optional OpenAPI route export from the route table.~~ Done — `openapi` feature generates OpenAPI 3.0.3 JSON from `RouteTable`. |
| P2 | Graceful shutdown with drain support. |

### Non-Goals

- Templating, server-side rendering, or asset serving.
- WebSocket support in v0.1 (may add later via an opt-in feature).
- Compatibility with Tower `Layer`/`Service` traits. Harrow defines its own middleware model. If Tower interop is needed, a thin adapter crate can bridge later.

---

## 3. Design Principles

1. **Explicit over implicit.** No hidden trait impls, no inference-dependent dispatch. If the user did not write it, it does not happen.
2. **Data over types.** Routes, middleware chains, and metadata are runtime values, not encoded in the type system.
3. **Observability is first-party, but explicit.** Harrow ships the middleware and extension traits, but applications opt in via feature flags and registration rather than getting telemetry by default.
4. **Compile-time is developer time.** Minimize generic instantiation. Prefer dynamic dispatch (`Box<dyn Handler>`) on cold paths, monomorphization only where it matters for hot-path throughput.
5. **Small API surface.** A developer should be able to read the entire public API in one sitting.

---

## 4. Architecture Overview

```
                        ┌──────────────────────────┐
                        │        harrow::App        │
                        │  ┌────────────────────┐   │
                        │  │    RouteTable       │   │
  Incoming              │  │  (Vec<Route>)       │   │
  HTTP request          │  │  - method           │   │
  ──────────────►       │  │  - path pattern     │   │
  server backend        │  │  - handler fn       │   │
  (tokio local workers  │  │  - metadata         │   │
   / monoio / meguri)   │  └────────┬───────────┘   │
                        │           │               │
                        │  ┌────────▼───────────┐   │
                        │  │   MiddlewareChain   │   │
                        │  │  (Vec<Middleware>)   │   │
                        │  └────────┬───────────┘   │
                        │           │               │
                        │  ┌────────▼───────────┐   │
                        │  │   O11y Core         │   │
                        │  │  - tracing span     │   │
                        │  │  - metrics          │   │
                        │  │  - request id       │   │
                        │  └────────────────────┘   │
                        └──────────────────────────┘
```

Detailed transport/backend architecture is documented separately in
[`docs/h1-dispatcher-design.md`](../h1-dispatcher-design.md). The runtime
direction is documented in
[`docs/strategy-local-workers.md`](../strategy-local-workers.md).

### 4.1 Core Types

```rust
/// A plain async function that handles a request.
type HandlerFn = Box<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response>>>>;

/// The application. Owns the route table, global middleware, and state.
/// Observability is added via middleware and extension traits.
struct App {
    route_table: RouteTable,
    middleware: Vec<Box<dyn Middleware>>,
    state: crate::state::TypeMap,
}
```

### 4.2 Handler Signatures

Handlers are plain async functions. Parameter extraction is explicit — the user destructures from `Request` using methods that return `Result`:

```rust
async fn get_user(req: Request) -> Result<Response, AppError> {
    let user_id: u64 = req.param("id").parse()?;
    let db = req.require_state::<DbPool>()?;
    let user = db.find(user_id).await?;
    
    Ok(Response::json(&user))
}
```

No "magic" argument injection. No variadic traits. The `Request` wrapper provides ergonomic methods (`param`, `query_pairs`, `query_param`, `body_json`, `require_state`, `try_state`) so parsing and dependency errors stay localized in the handler body with clear call sites.

### 4.3 Routing API

```rust
let app = App::new()
    .health("/health")
    .liveness("/live")
    .readiness_handler("/ready", readiness_handler)
    .get("/users/:id", get_user)
    .post("/users", create_user)
    .delete("/users/:id", delete_user)
    .group("/api/v1", |g| {
        g.get("/items", list_items)
         .get("/items/:id", get_item)
    })
    .with_metadata("/users/:id", |m| {
        m.name = Some("user_detail".into());
        m.tags.push("users".into());
    });
```

Probe helpers register ordinary `GET` routes with probe metadata attached, so they
show up in `route_table()` introspection the same way as user-defined routes.

### 4.4 Route Table Introspection

```rust
// Print all routes at startup
for route in app.route_table().iter() {
    println!("{} {} [{}]", route.method, route.pattern, route.metadata.name.as_deref().unwrap_or("-"));
}

// Export a minimal method + path snapshot
for route in app.route_table().summary() {
    println!("{route}");
}

// Filter routes by tag
let user_routes: Vec<&Route> = app.route_table()
    .iter()
    .filter(|r| r.metadata.tags.contains(&"users".into()))
    .collect();
```

### 4.5 Middleware Model

Middleware is a plain async function that wraps the next handler:

```rust
async fn logging_middleware(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let resp = next.run(req).await;
    tracing::info!(elapsed = ?start.elapsed(), status = resp.status().as_u16());
    resp
}

let app = App::new()
    .middleware(logging_middleware)
    .get("/ping", ping_handler);
```

No `Layer`. No `Service`. No `BoxCloneService`. A middleware is a function with a known signature.

### 4.6 Opt-In Observability

When the `o11y` feature is enabled and the observability wiring is registered, requests can get:

| Feature | Implementation | Status |
|---------|---------------|--------|
| **Trace span** | `tracing::info_span!` wrapping the handler. | Implemented |
| **Request ID** | Generated or propagated via `x-request-id`. | Implemented |
| **Latency histogram** | Per-route histogram (`metrics` crate). | **v0.2 Target** |
| **Error counter** | Counts 4xx/5xx responses per route. | **v0.2 Target** |

Typical application wiring is explicit:

```rust
#[cfg(feature = "o11y")]
use harrow::{App, AppO11yExt};
#[cfg(feature = "o11y")]
use harrow::o11y::O11yConfig;

#[cfg(feature = "o11y")]
let app = App::new().o11y(O11yConfig::default().service_name("svc"));
```

Without the `o11y` feature or middleware, Harrow runs without tracing spans or metrics.

### 4.7 Startup Diagnostics

On `app.serve(addr)`, Harrow logs:

```
harrow listening on 0.0.0.0:8080
  GET  /health             [health]
  GET  /users/:id          [user_detail]  tags: users
  POST /users              [create_user]  tags: users
  DEL  /users/:id          [delete_user]  tags: users
  GET  /api/v1/items       [list_items]   tags: items
  GET  /api/v1/items/:id   [get_item]     tags: items
  middleware: [logging, auth, o11y]
```

---

## 5. Path Matching

Harrow uses a compressed radix trie (via the `matchit` crate) for O(path_length) lookups. This provides high-performance routing even as the number of routes grows.

| Pattern | Matches | Captures |
|---------|---------|----------|
| `/users` | exact | — |
| `/users/:id` | single segment | `id` |
| `/files/*path` | tail glob | `path` (rest of URL) |

---

## 6. State / Dependency Injection

Application state is stored in a type-map on `App` and accessible via `Request`:

```rust
let pool = DbPool::connect("postgres://...").await?;

let app = App::new()
    .state(pool)
    .state(AppConfig::from_env())
    .get("/users/:id", get_user);

// Inside handler:
async fn get_user(req: Request) -> Result<Response, AppError> {
    let db = req.require_state::<DbPool>()?; // Returns Result<&DbPool, MissingStateError>
    // ...
}
```

`require_state::<T>()` returns `Result<&T, MissingStateError>`. This ensures that if a dependency is missing, the error is handled gracefully via the `?` operator rather than a runtime panic. `try_state::<T>()` is also available for optional dependencies.

---

## 7. Error Handling

Handlers return `Result<Response, AppError>`. This enables the "Explicit Extractor" pattern and provides clear observability for middleware.

```rust
async fn get_user(req: Request) -> Result<Response, AppError> {
    let id: u64 = req.param("id").parse()?; // Error on this specific line
    let db = req.require_state::<DbPool>()?;
    let user = db.find_user(id).await?;
    Ok(Response::json(&user))
}
```

`AppError` is user-defined and implements `IntoResponse`. Harrow provides a default `ProblemDetail` (RFC 9457) response builder but does not impose it.

Framework-generated 404 and 405 responses are configurable:

```rust
let app = App::new()
    .default_problem_details()
    .not_found_handler(|req| async move {
        ProblemDetail::new(StatusCode::NOT_FOUND)
            .detail(format!("no route for {}", req.path()))
    })
    .method_not_allowed_handler(|req, allowed| async move {
        let allow = allowed
            .iter()
            .map(|method| method.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        ProblemDetail::new(StatusCode::METHOD_NOT_ALLOWED)
            .detail(format!("allowed methods: {allow}"))
            .instance(req.path().to_string())
    });
```

---

## 8. Graceful Shutdown

```rust
app.serve_with_shutdown(addr, shutdown_signal()).await?;
```

On signal, Harrow:
1. Stops accepting new connections.
2. [Target v0.2] Waits for in-flight requests to complete (configurable timeout).
3. Returns from `serve_with_shutdown`.

Current v0.1 behavior terminates in-flight requests immediately.

---

## 9. Crate Structure

```
harrow/
  harrow-core/              # Route table, Request/Response wrappers, middleware trait
  harrow-codec-h1/          # Shared HTTP/1 parsing and serialization helpers
  harrow-server/            # Shared server bootstrap/config helpers
  harrow-o11y/              # Tracing + metrics integration (optional feature)
  harrow-server-tokio/      # Tokio backend, custom HTTP/1 handling, graceful shutdown
  harrow-server-monoio/     # Monoio backend, local-worker HTTP/1 handling
  harrow-server-meguri/     # Meguri backend, io_uring-focused path
  harrow-bench/             # Criterion benches, remote perf capture, summary rendering
  harrow/                   # Facade crate re-exporting everything
```

Feature flags on the facade crate:

| Feature | Default | Contents |
|---------|---------|----------|
| `o11y` | off | First-party observability wiring: tracing spans, request IDs, and `O11yConfig` integration |
| `json` | on | `serde_json` body parsing/response helpers |
| `tls` | off | rustls integration |
| `http2` | planned | HTTP/2 support via a future backend-specific implementation |
| `profiling` | off | Adds `#[inline(never)]` markers on key functions for cleaner flamegraph frames |

---

## 10. Performance Targets

Measured on a simple JSON echo handler (`/echo` — parse JSON body, return it):

| Metric | Target |
|--------|--------|
| Added latency over backend baseline | < 1 us p99 |
| Requests/sec (single core, 64 connections) | > 95% of matched backend baseline throughput |
| Binary size (release, stripped, minimal features) | < 2 MB |
| Compile time (clean build) | < 30s on M-series Apple Silicon |

Criterion microbenches and `harrow-bench` perf runs are executed on demand. CI currently focuses on correctness, formatting, and linting.

---

## 11. Performance Verification

Performance validation is driven by targeted benchmark sessions, not automatic PR gates. The current workflow captures benchmark metrics, perf artifacts, and supporting visualizations when profiling is enabled, then reviews them manually during focused performance work and before releases.

### 11.1 Toolchain

| Tool | Role |
|------|------|
| `criterion` | Local micro-benchmarks for isolated workloads. |
| `harrow-bench` | Remote perf runner and artifact collector for end-to-end benchmark sessions. |
| `perf stat` / `perf record` | Counter and sampled-stack capture during benchmark runs. |
| `perf_summary` | Renders markdown/SVG summaries and local flamegraphs from captured perf artifacts when the required tools are available. |

### 11.2 What Gets Captured

Targeted benchmark runs can capture:

- request metrics such as throughput and latency percentiles
- host telemetry such as `vmstat`, `sar`, `iostat`, and `pidstat`
- `perf stat` counter output
- `perf record` samples, `perf report`, folded stacks, and flamegraphs when the toolchain is available

### 11.3 Review Workflow

1. Run the relevant benchmark or remote perf scenario.
2. Capture request metrics plus perf/host-monitor artifacts for the run.
3. Render summaries and visualizations from the recorded artifacts.
4. Compare against a known-good baseline manually when investigating regressions or preparing a release.

This keeps the perf workflow explicit and reproducible without making it a mandatory CI gate at the current stage of the project.

### 11.4 Cargo Configuration

```toml
# Cargo.toml — workspace root
[profile.bench]
debug = true          # Required for meaningful flamegraph symbols
opt-level = 3
lto = "thin"

[profile.release]
debug = 1             # Line-level debug info for production profiling
opt-level = 3
lto = "fat"
codegen-units = 1
```

### 11.5 What We Look For In Review

When reviewing a profiled run:

| Signal | Action |
|--------|--------|
| New `alloc::` frames in hot path | Investigate. Likely an unnecessary allocation introduced. |
| Wider `tracing` frames | Check if new spans/events were added. Acceptable if intentional o11y, flag if accidental. |
| `clone()` or `to_string()` appearing in dispatch | Likely a regression. Request path should be zero-copy where possible. |
| Middleware traversal frame growth | Check if `Next` chaining changed. Should be constant-cost per middleware layer. |
| `serde` frames growing | Check if serialization path changed. May indicate a schema change, not a regression. |
| Hotspots move without an intended code change | Re-check the run and compare against a previous baseline before merging. |

### 11.6 Milestone Checks

Each milestone (v0.1, v0.2, v0.3) should still include a focused perf review:

| Milestone | Check |
|-----------|-------|
| **v0.1** | Establish repeatable perf captures and a baseline set of benchmark artifacts. |
| **v0.2** | Re-run targeted scenarios for routing, middleware, serialization, and o11y changes; review hotspot shifts manually. |
| **v0.3** | Validate TLS and timeout overhead with targeted perf runs when those features are enabled. |

---

## 12. What Harrow Intentionally Omits

| Feature | Rationale |
|---------|-----------|
| Proc macros | Core design principle. |
| Tower compatibility | Adds type complexity for interop most services don't need. Adapter crate possible later. |
| Built-in templating | Not a web application framework. Use `askama` or `maud` externally. |
| Cookie/session management | Belongs in middleware, not core. |
| WebSocket (v0.1) | Can be added as a feature-gated module later. |
| ORM/database integration | Out of scope. Bring your own `sqlx`, `diesel`, etc. |

---

## 12. Milestones

### v0.1 — Foundation (Current)
- Core types: `App`, `RouteTable`, `Route`, `Request`, `Response`.
- High-performance routing via `matchit` radix trie.
- Route groups with shared middleware.
- Middleware chain with fast-path optimization.
- State injection via type-map.
- Initial o11y (tracing spans, request ID).
- Route table introspection and startup printing.
- Basic graceful shutdown (no drain).
- Criterion benchmark suite.

### v0.2 — Ergonomics & Hardening
- **Explicit Extractors:** `Result`-based handlers and `require_state()`.
- **Graceful Shutdown Drain:** Wait for in-flight requests to finish.
- **O11y Metrics:** Latency histograms and error counters.
- `RouteTable` introspection: `summary()` returns `Vec<RouteSummary>` with `Display`.
- `ProblemDetail` (RFC 9457) error response builder.
- Configurable 404/405 responses.

---

## 13. Decisions

1. **State Injection:** Prefer `require_state::<T>() -> Result<&T, MissingStateError>` for required state, while providing `try_state::<T>() -> Option<&T>` for optional state.
2. **Path Matching:** Radix tree (`matchit`) is the default implementation from v0.1.

---

## 14. Prior Art and Differentiation

| Framework | Macros | Route Introspection | O11y Model | Overhead |
|-----------|--------|---------------------|------------|----------|
| **Axum** | No proc macros, but heavy trait generics | No first-class API | External Tower layers | Low |
| **Actix-web** | Proc macros for routes | Limited | External middleware | Low |
| **Warp** | No macros, filter combinators | No | External filters/middleware | Low |
| **Poem** | Proc macros | OpenAPI integration | Partial built-in support | Low |
| **Harrow** | None | First-class, queryable | First-party opt-in | Minimal |
