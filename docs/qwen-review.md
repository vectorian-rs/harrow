# Principal Engineer Review: Harrow HTTP Framework

**Date:** 2026-03-31  
**Reviewer:** Qwen Code  
**Scope:** Architecture, code quality, API design, performance, security, observability, testing, ecosystem, release readiness

---

## Executive Summary

Harrow is a well-architected, opinionated HTTP framework that makes deliberate trade-offs favoring **explicitness over magic**, **transparency over ergonomics**, and **backend independence over ecosystem compatibility**. The codebase demonstrates strong engineering discipline with clear separation of concerns, thoughtful performance optimizations, and comprehensive test coverage including property-based testing. However, there are architectural inconsistencies, particularly around runtime abstraction leakage, that need addressing before v1.0.

---

## 1. Architecture & Design Quality

### Strengths

**1.1 Clean Separation of Concerns**
The crate structure is exemplary:
- `harrow-core`: Framework primitives (routing, middleware trait, request/response)
- `harrow-middleware`: Feature-gated middleware implementations
- `harrow-server-*`: Backend-specific server bindings
- `harrow-o11y`: Observability configuration

This separation enables independent evolution and clear ownership boundaries.

**1.2 Explicit Request Model**
The "Request-First" pattern (`async fn(Request) -> Response`) is refreshingly simple:
- No macro magic or trait gymnastics
- IDE-friendly with transparent type inference
- Predictable error localization (errors appear at extraction site, not route registration)
- Minimal cloning overhead compared to extractor-based frameworks

**1.3 Route Table as First-Class Data**
The `RouteTable` being an inspectable `Vec<Route>` rather than opaque type-level encoding is excellent:
- Enables OpenAPI generation from route metadata
- Startup diagnostics printing all routes
- Runtime introspection for monitoring/health checks
- Simpler mental model than Axum's type-encoded routing

**1.4 Middleware Model Simplicity**
```rust
trait Middleware {
    fn call(&self, req: Request, next: Next) -> BoxFuture;
}
```
This is significantly simpler than Tower's `Service` + `Layer` model:
- No `poll_ready` complexity
- No readiness/backpressure concerns for application middleware
- Easy to author and understand
- Blanket impl for `async fn` reduces boilerplate

**1.5 Performance-Conscious Design**
Multiple evidence-based optimizations:
- `PathMatch` uses `Vec<(String, String)>` instead of `HashMap` (faster for 0-2 params, zero allocation for empty)
- Request ID generation via atomic counter + base64 encoding (no RNG, no syscalls)
- Fast path optimization in dispatch when no middleware exists
- `Arc<str>` for route patterns to avoid repeated string allocations
- Query parsing capped at 100 pairs to prevent OOM attacks

### Concerns

**1.6 Runtime Abstraction Leakage (CRITICAL)**
The `harrow-middleware` crate has **direct Tokio dependencies** in supposedly portable middleware:

```rust
// timeout.rs
tokio::time::timeout(duration, next.run(req)).await

// rate_limit.rs (InMemoryBackend::start_sweeper)
tokio::spawn(async move { ... })
tokio::time::sleep(interval).await

// session.rs
tokio::spawn(async move { ... })
```

This is an **architectural inconsistency**. The `middleware.md` design doc explicitly calls this out as a design bug, but the code hasn't been refactored to match the stated architecture. This breaks the promise of backend independence.

**Impact:**
- `harrow-middleware` with `rate-limit` or `session` features cannot be used with Monoio backend
- Contradicts the framework's stated design principles
- Creates confusion for users about what is "portable"

**Recommendation:** This should be P0 to fix before v1.0. Options:
1. Move Tokio-specific helpers into `harrow-server-tokio` as backend-specific utilities
2. Provide runtime-agnostic traits (e.g., `Timer`, `Spawner`) with backend-specific implementations
3. Make sweepers explicit cleanup methods (`prune_expired()`) called by the application

**1.7 Middleware Scope Granularity**
Current middleware scoping:
- Global middleware (`App::middleware`) — runs on ALL requests including 404/405
- Group middleware (`Group::middleware`) — runs only on matched routes within group

**Missing:** Per-route middleware scope finer than groups. Axum users expect `route_layer()` that applies only to matched routes (preserving 404 vs 401 distinction). Current workaround requires creating a group for each route, which is verbose.

**Recommendation:** Add `.route_middleware()` method that attaches middleware to the most recently defined route.

**1.8 Error Handling Consistency**
The framework supports `Result<Response, E>` where `E: IntoResponse`, but there's no unified error type or error composition pattern documented. Users must define their own `AppError` enum with manual `From` impls.

**Recommendation:** Consider providing a standard `BoxError` type or error composition helper in `harrow-core`.

---

## 2. Code Quality

### Strengths

**2.1 Comprehensive Test Coverage**
- Unit tests for all core modules
- Property-based tests with `proptest` for:
  - Path matching agreement (`match_path` vs `matches`)
  - Param capture correctness
  - Middleware ordering
  - Short-circuit behavior
- Integration tests covering TCP and client-based scenarios
- Fuzzing strategy documented in `verification.md`

**2.2 Documentation Quality**
- `README.md` with clear quickstart and feature matrix
- `AGENTS.md` for AI-assisted navigation (excellent practice)
- Design docs (`docs/prds/harrow-http-framework.md`, `docs/middleware.md`) explaining rationale
- Inline comments explaining non-obvious optimizations
- Feature-gated API docs with clear backend selection guidance

**2.3 Type Safety**
- Strong use of typestate patterns (e.g., `App` builder)
- `MissingStateError` and `MissingExtError` provide clear compile-time feedback
- `IntoResponse` trait ensures all handler return types can become responses
- Compile-time feature validation (`compile_error!` if no backend selected)

**2.4 Memory Efficiency**
- `Arc<str>` for route patterns
- `Vec` instead of `HashMap` for small param sets
- Zero-allocation `matches()` method for 405 detection
- Query parsing returns owned `HashMap` but caps at 100 pairs

### Concerns

**2.5 Boxed Futures in Middleware Chain**
```rust
type BoxFuture = Pin<Box<dyn Future<Output = Response> + Send>>;
```
Every middleware layer allocates a boxed future. While the design doc mentions "one `Arc::clone` per middleware layer per request" as the only allocation, the boxed futures themselves are additional allocations.

**Mitigation:** The `profiling` feature adds `#[inline(never)]` markers for cleaner flamegraphs, but there's no zero-allocation middleware path.

**Recommendation:** Document this trade-off explicitly. Consider an alternative trait design using RPITIT (Return Position Impl Trait In Traits) when stable, or provide both boxed and unboxed variants.

**2.6 Unsafe Code Usage**
```rust
// o11y.rs
unsafe { String::from_utf8_unchecked(buf.to_vec()) }
```
This is **correct** (alphabet is ASCII), but:
- No `#[inline]` annotation despite being called per-request
- No comment explaining why it's safe
- Could use `debug_assert!` to verify in debug builds

**Recommendation:** Add safety comment and consider debug-build validation.

**2.7 Clone Bomb Risk in Route Metadata**
`RouteMetadata` and `Route` are `Clone`, and routes are cloned when groups are nested. For routes with many tags or custom metadata, this could be expensive.

**Recommendation:** Consider `Arc<RouteMetadata>` for shared metadata in group scenarios.

---

## 3. API Design & Ergonomics

### Strengths

**3.1 Builder Pattern Consistency**
```rust
App::new()
    .state(pool)
    .middleware(logging)
    .health("/health")
    .get("/users", handler)
    .group("/api", |g| g.get("/users", list))
```
Fluent, composable, and predictable.

**3.2 Explicit State Injection**
```rust
let db = req.require_state::<DbPool>()?;  // Result<&T, MissingStateError>
let opt = req.try_state::<Config>();       // Option<&T>
```
Clear error location, no magic.

**3.3 Route Metadata for OpenAPI**
```rust
.with_metadata("/users", |m| {
    m.name = Some("listUsers".into());
    m.tags.push("users".into());
})
```
Enables automatic OpenAPI generation without macros.

### Concerns

**3.4 No Request/Response Combinators**
The `middleware.md` doc acknowledges this gap. Axum users expect:
- `map_request(|req| ...)` — transform request before handler
- `map_response(|resp| ...)` — transform response after handler
- `around(|req, next| ...)` — custom middleware inline

**Current workaround:** Must define a full middleware function/type.

**Recommendation:** Add lightweight combinators in `harrow-core`:
```rust
impl App {
    pub fn map_request<F>(self, f: F) -> Self
    where F: Fn(Request) -> Request + Send + Sync + 'static;
    
    pub fn map_response<F>(self, f: F) -> Self
    where F: Fn(Response) -> Response + Send + Sync + 'static;
}
```

**3.5 Group Middleware Ordering Ambiguity**
When nesting groups, middleware order is:
```rust
app.group("/api", |g| {
    g.middleware(auth)
        .group("/v1", |v1| {
            v1.middleware(rate_limit)
                .get("/users", list)  // Order: auth -> rate_limit -> handler
        })
})
```
This is documented but not obvious. The `middleware.md` doc explains it well, but users may be surprised.

**Recommendation:** Add startup log output showing middleware chain per route (currently shows count but not names).

**3.6 No Handler Name Inference**
Route metadata `name` must be set explicitly via `.with_metadata()`. Axum infers handler names from function names for error messages.

**Recommendation:** Consider a macro-free way to capture function names (e.g., `std::any::type_name` on the handler closure, though this is verbose).

---

## 4. Performance

### Strengths

**4.1 Benchmark-Driven Development**
- Criterion benches in `harrow-bench`
- `perf stat` / `perf record` integration
- Flamegraph generation support
- Performance targets documented (< 1μs overhead over raw Hyper)

**4.2 Fast Path Optimization**
```rust
// dispatch.rs
if shared.middleware.is_empty() && route.middleware.is_empty() {
    (route.handler)(req).await.into_inner()  // Direct call, no chain setup
} else {
    run_middleware_chain(...).await
}
```
Zero-cost abstraction when middleware isn't used.

**4.3 Efficient Request ID Generation**
- Atomic counter + base64 encoding (11 chars, URL-safe)
- No RNG, no syscalls
- ~100M IDs before collision risk (birthday paradox)

**4.4 Path Matching Performance**
- Uses `matchit` radix trie (O(path_length))
- `matches()` method is zero-allocation for 405 detection
- `PathMatch` uses `Vec` instead of `HashMap` (faster for small N)

### Concerns

**4.5 No Performance Regression Tests**
The `verification.md` doc describes manual perf review workflow, but there's no CI integration for performance regression detection.

**Recommendation:** Add CI job that runs micro-benches and fails if latency increases > 5% from baseline.

**4.6 Allocation in Middleware Chain**
Each middleware layer allocates:
- `Box<dyn Future>` for the boxed future
- `Arc::clone()` for shared state

For 10 middleware layers, that's 10+ allocations per request.

**Recommendation:** Document expected allocation count. Consider a `SmallVec` optimization for middleware chains under a threshold.

**4.7 Content-Length Pre-Check Only**
```rust
// dispatch.rs
if let Some(cl) = headers.get(CONTENT_LENGTH).and_then(|v| v.parse().ok()) {
    if cl > max_body_size { return 413; }
}
```
This rejects obviously oversized bodies early, but bodies without `Content-Length` (chunked encoding) are read fully before size check.

**Recommendation:** Add frame-by-frame size accumulation check (already done in `body_bytes()`, but could fail faster).

---

## 5. Security

### Strengths

**5.1 Query Parsing DoS Protection**
```rust
const MAX_QUERY_PAIRS: usize = 100;
```
Prevents OOM from pathological query strings.

**5.2 Body Size Limits**
- Default 2 MiB limit
- Configurable via `max_body_size()`
- Enforced at two layers (Content-Length pre-check and frame accumulation)

**5.3 Panic Recovery**
`catch_panic_middleware` prevents panics from killing connections:
```rust
match AssertUnwindSafe(next.run(req)).catch_unwind().await {
    Ok(response) => response,
    Err(_) => Response::new(INTERNAL_SERVER_ERROR, "internal server error"),
}
```

**5.4 No Implicit Logging of Sensitive Data**
Observability middleware logs request/response metadata but doesn't automatically log bodies or headers containing auth tokens.

### Concerns

**5.5 Rate Limiter Key Extractor Safety**
```rust
pub trait KeyExtractor {
    fn extract(&self, req: &Request) -> Option<String>;
}
```
Users might extract from untrusted headers (e.g., `X-Forwarded-For`) without validation, enabling IP spoofing attacks.

**Recommendation:** Add documentation warning about trusted vs untrusted key sources. Consider a `TrustedHeader` wrapper type.

**5.6 No Built-In CSRF Protection**
Session middleware exists but no CSRF token generation/validation.

**Recommendation:** Document that CSRF protection must be implemented at application level or add a `csrf` feature.

**5.7 Panic Messages Leaked in Tests**
```rust
// catch_panic.rs tests
panic!("{}", format!("detailed error: code={}", 42));
```
Test verifies panic is caught, but in production, panic payloads should never be logged or returned (security best practice).

**Current behavior:** Response body is generic "internal server error" — correct. But ensure panic payloads aren't logged by default.

---

## 6. Observability

### Strengths

**6.1 W3C Trace ID Compliance**
```rust
fn derive_trace_id(request_id: &str) -> String {
    blake3::Hasher::new()
        .update(request_id.as_bytes())
        .finalize_xof()
        .fill(&mut trace_bytes);
    // 16 bytes → 32-char hex
}
```
Deterministic trace ID from request ID, W3C-compliant format.

**6.2 Metrics Integration**
- Latency histogram with sensible boundaries
- Error counter for 4xx/5xx
- Labels: method, status, route pattern
- Optional (only recorded if `otlp_metrics_endpoint` configured)

**6.3 Request ID Propagation**
- Extracts from incoming header (e.g., CloudFront `x-amz-cf-id`)
- Generates if absent
- Echoed in response
- Available in handler via `req.request_id()`

### Concerns

**6.4 No Metrics Cardinality Protection**
Route pattern is used as a label, but if users define routes with path parameters in the pattern (e.g., `/users/:id` instead of `/users/:id`), this could cause high cardinality.

**Current mitigation:** Route pattern comes from `RouteTable`, not raw path — correct. But document this clearly.

**6.5 No Distributed Tracing Propagation**
Request ID is propagated, but there's no W3C Trace Context (`traceparent`/`tracestate`) header support for cross-service tracing.

**Recommendation:** Add optional `traceparent` header parsing/generation in `o11y` middleware.

---

## 7. Testing Strategy

### Strengths

**7.1 Property-Based Testing**
Excellent use of `proptest`:
- Path matching agreement property
- Param capture correctness
- Middleware ordering invariants
- Short-circuit behavior

**7.2 Multiple Test Surfaces**
- Unit tests in each module
- Client-based tests (no TCP, uses `Client` wrapper)
- TCP-based integration tests (true end-to-end)
- Proptest for edge cases

**7.3 Verification Strategy Document**
`docs/verification.md` explicitly states what techniques apply and why others don't:
- proptest: Yes (path matching, GCRA arithmetic)
- fuzzing: Yes (query parsing)
- Kani: Maybe (GCRA single-step)
- DST/TLA+/Maelstrom: No (not a distributed system)

This is **mature engineering judgment**.

### Concerns

**7.4 No Fuzz Targets Implemented**
The `verification.md` doc describes fuzzing strategy, but there are no `cargo-fuzz` targets in the repo.

**Recommendation:** Implement at least the query-string fuzz target as described.

**7.5 Limited Concurrency Testing**
Tests run on single-threaded Tokio runtime. No stress tests for:
- Concurrent route table modifications (not possible in current design, but worth documenting)
- High-concurrency middleware execution
- Race conditions in rate limiter (DashMap is used, but no stress tests)

**Recommendation:** Add `tokio::test(flavor = "multi_thread")` tests with concurrent requests.

---

## 8. Ecosystem & Migration

### Strengths

**8.1 Clear Migration Path from Axum (for Simple Cases)**
The `explicit-extractors.md` and `middleware.md` docs provide honest assessment:
- `from_fn` middleware → easy migration
- `Layer`/`Service` crates → hard migration
- `ServiceBuilder` composition → medium friction

**8.2 Feature-Gated Dependencies**
Users only compile what they need:
- `tokio` or `monoio` (exactly one required)
- `json`, `msgpack` (optional)
- Individual middleware features

### Concerns

**8.3 No Tower Interop Layer**
The `middleware.md` doc discusses optional Tower interop but none exists. This is a **deliberate trade-off** but limits ecosystem reuse.

**Recommendation:** If user demand justifies it, add a `tower-interop` feature in `harrow-server-tokio` that provides:
```rust
impl App {
    pub fn layer<L>(self, layer: L) -> Self
    where L: Layer<SharedStateService> + 'static;
}
```
Make it explicitly Tokio-only.

**8.4 No Middleware Publishing Guide**
Users wanting to publish Harrow middleware crates have no template or best-practices guide.

**Recommendation:** Add `docs/publishing-middleware.md` with:
- Crate structure template
- Feature flag conventions
- Testing recommendations
- Versioning strategy

---

## 9. Monoio Backend

### Strengths

**9.1 Cancellation Safety Documentation**
The `harrow-server-monoio` crate has excellent documentation about io_uring cancellation risks and mitigation strategies. This is **critical** for safe io_uring usage.

**9.2 Thread-Per-Core Architecture**
Proper io_uring exploitation with thread-per-core design for Linux 6.1+.

### Concerns

**9.3 Feature Parity with Tokio Backend**
The Monoio backend appears to have fewer features (e.g., no TLS mentioned). Document current gaps clearly.

**9.4 Limited Testing Surface**
Most integration tests are in `harrow-server-tokio/tests/integration.rs`. Monoio tests are minimal.

**Recommendation:** Add Monoio-specific integration tests or ensure existing tests run with both backends.

---

## 10. Release Readiness

### Blockers for v1.0

1. **Runtime abstraction leakage** — `harrow-middleware` uses Tokio directly in portable code
2. **Incomplete verification** — Fuzz targets described but not implemented
3. **Middleware scope limitations** — No per-route middleware (finer than groups)

### Recommended Before v1.0

1. Add request/response combinators (`map_request`, `map_response`)
2. Implement at least one fuzz target
3. Add concurrency stress tests
4. Document panic payload handling (ensure not logged)
5. Add CSRF protection guidance
6. Clarify Monoio feature gaps

### Post-v1.0 Candidates

1. Tower interop layer (Tokio-only)
2. W3C Trace Context propagation
3. Kani verification for GCRA
4. Performance regression CI
5. Middleware publishing guide

---

## Final Assessment

**Overall Grade: B+ (Strong foundation, needs polish for v1.0)**

Harrow is a **thoughtfully designed framework** that makes principled trade-offs. The explicit request model, route introspection, and backend independence are genuine differentiators. The codebase demonstrates strong engineering discipline with comprehensive testing and clear documentation.

**Critical Issues:**
1. Runtime abstraction leakage in middleware (Tokio usage in portable code)
2. Incomplete verification strategy (fuzzing not implemented)

**Strengths Outweigh Weaknesses:**
- Clean architecture with clear separation of concerns
- Excellent documentation and design rationale
- Property-based testing for critical invariants
- Performance-conscious design with benchmarks

**Recommendation:** Address the Tokio abstraction leakage as P0 before v1.0 release. This is an architectural inconsistency that undermines the framework's core value proposition of backend independence.

---

## Summary Action Items

| Priority | Issue | Recommendation |
|----------|-------|----------------|
| P0 | Runtime abstraction leakage | Move Tokio-specific helpers to backend crates or provide runtime-agnostic traits |
| P0 | Missing fuzz targets | Implement query-string fuzz target as described in `verification.md` |
| P1 | Middleware scope granularity | Add `.route_middleware()` for per-route middleware |
| P1 | No request/response combinators | Add `map_request`, `map_response`, `around` helpers |
| P1 | Limited concurrency testing | Add multi-threaded stress tests |
| P2 | No Tower interop | Consider optional Tokio-only adapter layer |
| P2 | No CSRF protection | Document application-level patterns or add feature |
| P2 | No middleware publishing guide | Add `docs/publishing-middleware.md` |
| P3 | W3C Trace Context | Add `traceparent` header support in o11y middleware |
| P3 | Performance regression CI | Add automated perf benchmark gating |
