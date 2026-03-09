# Harrow: A Thin, Macro-Free HTTP Framework over Hyper

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

Harrow aims to be the framework you reach for when you want Hyper's raw performance with a thin, explicit, zero-macro API surface that treats observability and route introspection as first-class features.

---

## 2. Goals

| Priority | Goal |
|----------|------|
| P0 | Zero proc-macros. All routing and handler wiring is plain Rust function calls. |
| P0 | Route table is a concrete, inspectable data structure available at runtime. |
| P0 | Built-in structured observability: tracing spans per request, latency histograms, error counters. |
| P0 | Minimal overhead over raw Hyper. Target < 1 us added latency per request on the hot path. |
| P0 | Continuous flamegraph profiling. Every milestone, PR, and CI run produces comparable flamecharts to catch regressions before they merge. |
| P1 | Compile times competitive with or better than Axum for equivalent service definitions. |
| P1 | Clear, human-readable compiler errors. No deeply nested generic bounds. |
| P1 | First-class health check, readiness, and liveness endpoints. |
| P2 | Optional OpenAPI route export from the route table. |
| P2 | Graceful shutdown with drain support. |

### Non-Goals

- Templating, server-side rendering, or asset serving.
- WebSocket support in v0.1 (may add later via an opt-in feature).
- Compatibility with Tower `Layer`/`Service` traits. Harrow defines its own middleware model. If Tower interop is needed, a thin adapter crate can bridge later.

---

## 3. Design Principles

1. **Explicit over implicit.** No hidden trait impls, no inference-dependent dispatch. If the user did not write it, it does not happen.
2. **Data over types.** Routes, middleware chains, and metadata are runtime values, not encoded in the type system.
3. **Observability is not optional.** Every request gets a trace span and basic metrics by default. You opt out, not in.
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
  hyper::conn::auto     │  │  - handler fn       │   │
                        │  │  - metadata         │   │
                        │  └────────┬───────────┘   │
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

### 4.1 Core Types

```rust
/// A plain async function that handles a request.
type HandlerFn = Box<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

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
async fn get_user(mut req: Request) -> Result<Response, AppError> {
    let user_id: u64 = req.param("id").parse()?;
    let db = req.get_state::<DbPool>()?;
    let user = db.find(user_id).await?;
    
    Ok(Response::json(&user))
}
```

No "magic" argument injection. No variadic traits. The `Request` wrapper provides ergonomic methods (`param`, `query`, `json`, `get_state`) that return `Result` types with clear errors, ensuring full IDE support and localizing error logic within the handler body.

### 4.3 Routing API

```rust
let app = App::new()
    .get("/health", health_handler)
    .get("/users/:id", get_user)
    .post("/users", create_user)
    .delete("/users/:id", delete_user)
    .group("/api/v1", |g| {
        g.get("/items", list_items)
         .get("/items/:id", get_item)
    })
    .with_metadata("/users/:id", |m| {
        m.name("user_detail").tag("users")
    });
```

### 4.4 Route Table Introspection

```rust
// Print all routes at startup
for route in app.route_table().iter() {
    println!("{} {} [{}]", route.method, route.pattern, route.metadata.name.as_deref().unwrap_or("-"));
}

// Export as JSON for external tooling
let json = serde_json::to_string_pretty(app.route_table())?;

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

### 4.6 Built-in Observability

Every request automatically gets:

| Feature | Implementation | Status |
|---------|---------------|--------|
| **Trace span** | `tracing::info_span!` wrapping the handler. | Implemented |
| **Request ID** | Generated or propagated via `x-request-id`. | Implemented |
| **Latency histogram** | Per-route histogram (`metrics` crate). | **v0.2 Target** |
| **Error counter** | Counts 4xx/5xx responses per route. | **v0.2 Target** |

Opt-out is handled by not registering the observability middleware.

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
    let db = req.get_state::<DbPool>()?; // Returns Result<&DbPool, Error>
    // ...
}
```

`get_state::<T>()` returns `Result<&T, Error>`. This ensures that if a dependency is missing, the error is handled gracefully via the `?` operator rather than a runtime panic. `try_state::<T>()` is also available for optional dependencies.

---

## 7. Error Handling

Handlers return `Result<Response, AppError>`. This enables the "Explicit Extractor" pattern and provides clear observability for middleware.

```rust
async fn get_user(mut req: Request) -> Result<Response, AppError> {
    let id: u64 = req.param("id").parse()?; // Error on this specific line
    let db = req.get_state::<DbPool>()?;
    let user = db.find_user(id).await?;
    Ok(Response::json(&user))
}
```

`AppError` is user-defined and implements `IntoResponse`. Harrow provides a default `ProblemDetail` (RFC 9457) response builder but does not impose it.

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
  harrow-core/       # Route table, Request/Response wrappers, middleware trait
  harrow-o11y/       # Tracing + metrics integration (optional feature)
  harrow-server/     # Hyper binding, connection handling, graceful shutdown
  harrow-bench/      # Standalone load driver + criterion benchmarks (3 workloads)
  harrow/            # Facade crate re-exporting everything
  scripts/
    profile.sh       # Run all workloads under cargo-flamegraph, output SVGs
    profile-diff.sh  # Diff current flamegraphs against a saved baseline
  flamegraphs/       # .gitignore-d, local output directory
```

Feature flags on the facade crate:

| Feature | Default | Contents |
|---------|---------|----------|
| `o11y` | on | Tracing spans + metrics |
| `json` | on | `serde_json` body parsing/response helpers |
| `tls` | off | rustls integration |
| `http2` | on | HTTP/2 support via hyper |
| `profiling` | off | Adds `#[inline(never)]` markers on key functions for cleaner flamegraph frames |

---

## 10. Performance Targets

Measured on a simple JSON echo handler (`/echo` — parse JSON body, return it):

| Metric | Target |
|--------|--------|
| Added latency over raw Hyper | < 1 us p99 |
| Requests/sec (single core, 64 connections) | > 95% of raw Hyper throughput |
| Binary size (release, stripped, minimal features) | < 2 MB |
| Compile time (clean build) | < 30s on M-series Apple Silicon |

Benchmarks tracked in CI via `criterion`.

---

## 11. Flamegraph-Driven Performance Verification

Every change to Harrow must be provably non-regressing. Flamegraphs are not a debugging afterthought — they are a continuous verification artifact produced on every CI run and reviewable in every PR.

### 11.1 Toolchain

| Tool | Role |
|------|------|
| [`cargo-flamegraph`](https://github.com/flamegraph-rs/flamegraph) | Generates SVG flamegraphs from `perf` (Linux) or `dtrace` (macOS) profiles. |
| [`inferno`](https://github.com/jonhoo/inferno) | Rust-native folded-stack processing. Used in CI where SVG diffing is needed. `inferno-flamegraph` and `inferno-diff-folded` are the key binaries. |
| `criterion` | Micro-benchmarks that serve as the workloads being profiled. |
| Custom `harrow-bench` binary | A standalone load driver (wrk2-style) that sends sustained traffic to a running Harrow server for macro-level profiling. |

### 11.2 What Gets Profiled

Three standard workloads, each producing its own flamegraph:

| Workload | Description | What it catches |
|----------|-------------|-----------------|
| **echo** | JSON echo handler, no middleware, no state. Pure routing + serialization hot path. | Overhead in core request dispatch, path matching, response construction. |
| **middleware-chain** | 5-deep middleware stack (logging, auth check, request ID, rate limit stub, compression stub) around a trivial handler. | Cost of middleware traversal, `Next` chaining, per-middleware allocations. |
| **full-stack** | Realistic service: state injection, path params, JSON body parse, DB stub (async sleep), structured error responses, all o11y enabled. | End-to-end overhead under realistic conditions. Allocation pressure, span creation cost, metrics recording. |

### 11.3 CI Pipeline Integration

```
┌─────────────┐     ┌──────────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  PR opened  │────►│  cargo bench     │────►│  Profile each    │────►│  Diff against   │
│             │     │  (criterion)     │     │  workload with   │     │  main baseline  │
│             │     │                  │     │  cargo-flamegraph │     │  flamegraphs    │
└─────────────┘     └──────────────────┘     └──────────────────┘     └────────┬────────┘
                                                                               │
                                                                    ┌──────────▼──────────┐
                                                                    │  Post artifacts to  │
                                                                    │  PR as comment:     │
                                                                    │  - SVG flamegraphs  │
                                                                    │  - Diff flamegraph  │
                                                                    │  - criterion report │
                                                                    │  - Pass/fail gate   │
                                                                    └─────────────────────┘
```

**Steps in detail:**

1. **Baseline capture.** On every merge to `main`, CI runs all three workloads and stores the folded stacks and SVG flamegraphs as versioned artifacts (e.g., `flamegraphs/main/<commit-sha>/echo.folded`).

2. **PR profiling.** On every PR, CI runs the same workloads on the PR branch.

3. **Differential flamegraph.** `inferno-diff-folded` compares the PR's folded stacks against the `main` baseline, producing a red/blue differential SVG:
   - **Red** = frames that got hotter (more samples).
   - **Blue** = frames that got cooler (fewer samples).

4. **Regression gate.** CI fails the PR if:
   - Any criterion benchmark regresses by more than **3%** (configurable via `HARROW_PERF_THRESHOLD`).
   - Any new frame appears in the hot path that was not present in the baseline (flagged for manual review, not auto-fail).

5. **Artifact posting.** A CI bot comments on the PR with:
   - Links to the three workload flamegraphs (SVG, viewable in-browser).
   - The differential flamegraph.
   - A summary table of criterion results (mean, stddev, change %).

### 11.4 Local Developer Workflow

Developers can reproduce CI profiling locally:

```bash
# Generate a flamegraph for the echo workload
cargo flamegraph --bench echo_bench -o flamegraphs/echo.svg

# Run all three workloads and generate flamegraphs
./scripts/profile.sh

# Compare against a saved baseline
./scripts/profile-diff.sh baseline/main flamegraphs/
# Outputs: flamegraphs/diff-echo.svg, diff-middleware-chain.svg, diff-full-stack.svg
```

The `scripts/profile.sh` script:
- Builds in release mode with debug symbols (`profile.release.debug = true` in `Cargo.toml`).
- Runs each criterion benchmark under `cargo-flamegraph`.
- Launches `harrow-bench` against a local server for the macro-level profile.
- Outputs all SVGs to `flamegraphs/`.

### 11.5 Cargo Configuration

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

### 11.6 Flamegraph Storage and History

- `flamegraphs/` directory is `.gitignore`-d. CI artifacts are stored externally (S3, GCS, or GitHub Actions artifact storage).
- A lightweight manifest (`flamegraphs/manifest.json`) tracks which commit produced which baseline, enabling historical comparison across releases.
- On tagged releases (v0.1, v0.2, ...), flamegraphs are archived permanently and linked from the release notes, giving a visual performance history of the project.

### 11.7 What We Look For in Review

When reviewing a PR's flamegraph diff:

| Signal | Action |
|--------|--------|
| New `alloc::` frames in hot path | Investigate. Likely an unnecessary allocation introduced. |
| Wider `tracing` frames | Check if new spans/events were added. Acceptable if intentional o11y, flag if accidental. |
| `clone()` or `to_string()` appearing in dispatch | Likely a regression. Request path should be zero-copy where possible. |
| Middleware traversal frame growth | Check if `Next` chaining changed. Should be constant-cost per middleware layer. |
| `serde` frames growing | Check if serialization path changed. May indicate a schema change, not a regression. |
| Differential flamegraph is entirely blue | Celebrate. |

### 11.8 Milestone Gates

Each milestone (v0.1, v0.2, v0.3) has a performance gate:

| Milestone | Gate |
|-----------|------|
| **v0.1** | Flamegraph of `echo` workload shows Harrow frames occupy < 5% of total samples (95%+ is Hyper/tokio/kernel). Baseline flamegraphs established for all three workloads. |
| **v0.2** | No workload regresses by more than 3% vs v0.1 baseline. Route groups and serialization do not introduce new hot-path allocations. |
| **v0.3** | TLS and timeout handling do not appear in the `echo` workload flamegraph (they should only activate when configured). Full-stack workload remains within 5% of v0.2. |

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
- **Explicit Extractors:** `Result`-based handlers and `get_state()`.
- **Graceful Shutdown Drain:** Wait for in-flight requests to finish.
- **O11y Metrics:** Latency histograms and error counters.
- `RouteTable` serialization (JSON, TOML).
- `ProblemDetail` (RFC 9457) error response builder.
- Configurable 404/405 responses.

---

## 13. Decisions

1. **State Injection:** Prefer `get_state::<T>() -> Result<&T, Error>` for required state, while providing `try_state::<T>() -> Option<&T>` for optional state.
2. **Path Matching:** Radix tree (`matchit`) is the default implementation from v0.1.

---

## 14. Prior Art and Differentiation

| Framework | Macros | Route Introspection | Built-in O11y | Overhead |
|-----------|--------|---------------------|---------------|----------|
| **Axum** | No proc macros, but heavy trait generics | No first-class API | No (Tower layers) | Low |
| **Actix-web** | Proc macros for routes | Limited | No | Low |
| **Warp** | No macros, filter combinators | No | No | Low |
| **Poem** | Proc macros | OpenAPI integration | Partial | Low |
| **Harrow** | None | First-class, queryable | Built-in | Minimal |
