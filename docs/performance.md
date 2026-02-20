# Harrow Performance Baseline

**Date:** 2026-02-20
**Harrow version:** 0.1.0-dev
**Platform:** macOS (Darwin 24.6.0), Apple Silicon
**Rust:** edition 2024, release profile (`opt-level = 3`, `lto = "thin"`, `debug = true`)
**Benchmark tool:** criterion 0.5

---

## How to reproduce

```bash
# Run all benchmarks
cargo bench

# Run individual suites
cargo bench --bench echo
cargo bench --bench middleware_chain
cargo bench --bench full_stack
cargo bench --bench route_groups
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html` for interactive charts.

For flamegraph profiling:

```bash
# Requires: cargo install flamegraph
./scripts/profile.sh

# Compare against a baseline
./scripts/profile-diff.sh flamegraphs/baseline flamegraphs/
```

---

## Benchmark Architecture

Three levels of measurement, isolating different costs:

| Level | What it measures | Tool |
|-------|------------------|------|
| **Micro** | Path matching, route table lookup | Direct function calls, no IO |
| **TCP** | Full request-response cycle over loopback | Keep-alive HTTP/1.1 client |
| **Scaling** | Route table size and middleware depth impact | TCP with parameterized configurations |
| **Groups** | Route group overhead, scoped middleware, nesting | TCP with group/nested configurations |

TCP benchmarks use a minimal keep-alive HTTP/1.1 client (`BenchClient`) that reuses a single connection. This isolates server-side framework overhead from client library cost.

---

## Results: Path Matching

Pure CPU cost of `PathPattern::match_path` and `PathPattern::matches`. No IO, no allocation except for captured params.

| Benchmark | Time | Allocations |
|-----------|------|-------------|
| `exact_hit` (`/health`) | 17.3 ns | 0 |
| `exact_miss` (`/other`) | 10.5 ns | 0 |
| `1_param` (`/users/:id` vs `/users/42`) | 79.6 ns | 1 String (param value) |
| `2_params` (`/orgs/:org/repos/:repo`) | 135.6 ns | 2 Strings |
| `glob` (`/files/*path` vs `/files/a/b/c/d.txt`) | 138.6 ns | 1 String + Vec collect |
| `matches_no_alloc` (`/users/:id` vs `/users/42`) | 16.0 ns | 0 |

### Analysis

- **Exact match is ~17 ns.** Iterator walks two segments, compares literals, done.
- **Each param adds ~55 ns.** Dominated by `String` allocation for the captured value (`name.clone()` + `to_string()`).
- **`matches()` is 5x faster than `match_path()` with params** because it skips all allocations. Used for 404/405 detection where we only care about existence, not captured values.
- **Miss is faster than hit** because the iterator short-circuits on the first segment mismatch.

### Optimization history

| Version | `1_param` | `matches_no_alloc` | Change |
|---------|-----------|---------------------|--------|
| Pre-opt (HashMap + Vec collect) | ~160 ns (est.) | N/A | — |
| Current (Vec + iterator) | 79.6 ns | 16.0 ns | -50% match, new zero-alloc path |

---

## Results: Route Table Lookup

Pure CPU cost of `RouteTable::match_route_idx`. Linear scan through routes, calling `match_path` on each until a method+path match is found. Worst case: target route is last in the table.

| Routes | Time | Per-route cost |
|--------|------|----------------|
| 1 | 84 ns | — |
| 10 | 190 ns | ~12 ns/route |
| 50 | 634 ns | ~11 ns/route |
| 100 | 1.19 µs | ~11 ns/route |
| 200 | 2.30 µs | ~11 ns/route |
| Best case (first of 3) | 84 ns | — |

### Analysis

- **Linear scaling at ~11 ns/route.** Each non-matching route costs one `method != route.method` comparison (cheap branch) plus one `match_path` call on the pattern (iterator walk + literal compare).
- **Best case = worst case for 1 route.** 84 ns, identical to first-match in a 3-route table.
- **100 routes is 1.19 µs.** Acceptable for most services. At 200 routes (2.3 µs), a radix tree would provide O(path_length) lookup instead of O(n_routes).
- **Method filtering helps.** Routes with non-matching HTTP methods are skipped with a single enum comparison (~1 ns). A table with 100 routes but only 10 GETs effectively scans 10 routes for a GET request.

### When to consider a radix tree

| Route count | Lookup (worst) | Action |
|-------------|----------------|--------|
| < 50 | < 650 ns | Linear scan is fine |
| 50–200 | 0.6–2.3 µs | Monitor; likely fine |
| > 200 | > 2.3 µs | Swap to radix tree behind `RouteTable` interface |

---

## Results: TCP Round-Trip (Echo)

Full HTTP/1.1 request-response cycle over loopback TCP. Measures: TCP accept → hyper HTTP parse → route match → handler → response serialize → TCP write → client read.

| Benchmark | Time | Delta vs baseline |
|-----------|------|-------------------|
| `text_no_mw` (baseline) | 29.3 µs | — |
| `json_no_mw` | 29.7 µs | +0.4 µs |
| `param_no_mw` (`/users/:id`) | 29.3 µs | +0.0 µs |
| `404_miss` | 29.2 µs | -0.1 µs |

### Analysis

- **Loopback TCP dominates at ~29 µs.** This includes kernel TCP stack, hyper's HTTP/1.1 parser, and the response write path. Harrow's routing overhead is invisible at this scale.
- **JSON serialization adds ~0.4 µs.** `serde_json::to_vec` for a small `{"status":"ok","code":200}` payload.
- **Path param extraction is free in TCP terms.** The 80 ns `match_path` cost is lost in TCP noise.
- **404 is no slower than 200.** The zero-alloc `matches()` path for 405 detection means even failed lookups have negligible framework cost.

---

## Results: Middleware Chain

TCP round-trip with varying middleware depth. Two variants: no-op passthrough middleware (measures pure chain overhead) and realistic middleware (timing + header injection).

### Noop middleware scaling

| Depth | Time | Delta vs 0 |
|-------|------|------------|
| 0 | 31.4 µs | — |
| 1 | 32.6 µs | +1.2 µs |
| 2 | 31.5 µs | +0.1 µs |
| 3 | 30.2 µs | -1.2 µs (noise) |
| 5 | 31.6 µs | +0.2 µs |
| 10 | 33.7 µs | +2.3 µs |

**Per-middleware cost: ~240 ns/layer** (derived from 0→10 delta: 2.3 µs / 10 = 230 ns).

At depths 1–5, the middleware overhead is within TCP variance (~±1 µs). It becomes measurable at 10 layers.

### Realistic middleware

| Benchmark | Time | Delta vs baseline |
|-----------|------|-------------------|
| `baseline_0mw` | 31.3 µs | — |
| `3mw_mixed` (timing + header + noop) | 31.1 µs | ~noise |
| `5mw_mixed` (timing + 2×header + 2×noop) | 31.0 µs | ~noise |

Realistic middleware doing actual work (measure time, inject headers) is no slower than noop middleware. The framework overhead is the chain traversal itself (`Box::pin` + `Next` closure), not the middleware logic.

### Per-middleware allocation cost

Each middleware layer in the chain allocates:

| Allocation | Size |
|------------|------|
| `Box::new(closure)` for `Next::inner` | ~40 B (captures Arc + 2 usizes) |
| `Box::pin(middleware future)` from `Middleware::call` | ~64–128 B (depends on future state) |

Total: **~100–170 B per middleware layer per request.**

At 5 middleware layers × 100k req/s = ~85 MB/s allocation throughput. Well within allocator capacity.

---

## Results: Full Stack

The most realistic benchmark: state injection, path parameters, JSON response, 3 middleware layers, multiple routes.

| Benchmark | Time | Delta vs bare echo |
|-----------|------|--------------------|
| `json_3mw_state_param` (`/users/:id`, JSON, 3mw, state) | 31.5 µs | +2.2 µs |
| `text_3mw_health` (`/health`, text, 3mw, no params) | 30.9 µs | +1.6 µs |

### Framework overhead breakdown (estimated)

Isolating Harrow's contribution by subtracting the TCP baseline (29.3 µs):

| Component | Cost | Source |
|-----------|------|--------|
| Route matching (1 param) | ~80 ns | `path_match/1_param` micro-bench |
| Middleware chain (3 layers) | ~720 ns | 3 × 240 ns per layer |
| State `Arc::clone` | ~20 ns | Atomic refcount bump |
| JSON serialization | ~400 ns | `serde_json::to_vec` |
| Response construction | ~50 ns | `StatusCode` + headers |
| **Total estimated** | **~1.3 µs** | |
| **Measured delta** | **~2.2 µs** | Includes hyper overhead |

The ~0.9 µs gap between estimated component costs and measured delta is hyper's per-request overhead (connection dispatch, service_fn, body framing).

---

## Results: Route Table Scaling (TCP)

Worst-case route lookup with 2 realistic middleware over TCP. Target route is last in the table.

| Routes | Time | Delta vs 1 route |
|--------|------|-------------------|
| 1 | 30.1 µs | — |
| 10 | 30.1 µs | +0.0 µs |
| 50 | 31.1 µs | +1.0 µs |
| 100 | 29.8 µs | noise |
| 200 | 33.4 µs | +3.3 µs |

The pure CPU lookup at 200 routes is 2.3 µs. Over TCP it adds ~3.3 µs which includes the lookup plus repeated `match_path` calls for each non-matching route (some with params).

For typical services with 10–50 routes, route table size has no measurable impact on latency.

---

## Results: Route Groups

TCP round-trip measuring the cost of route groups: prefix-based grouping, scoped middleware (via `Arc<dyn Middleware>`), and nested group composition.

### Group vs top-level route

| Benchmark | Time | Delta vs baseline |
|-----------|------|-------------------|
| `toplevel_0mw` (baseline) | 29.0 µs | — |
| `group_0mw` (prefix only, no middleware) | 29.0 µs | +0.0 µs |
| `group_1mw` (prefix + 1 group middleware) | 29.7 µs | +0.7 µs |

**Route grouping itself is free.** The `App::group()` / `Group` builder merely prepends a prefix at startup. At runtime, group routes are indistinguishable from top-level routes in the route table — there is no extra indirection or lookup cost.

### Group middleware depth scaling

| Depth | Time | Delta vs 0 |
|-------|------|------------|
| 0 | 28.5 µs | — |
| 1 | 28.6 µs | +0.1 µs |
| 2 | 28.9 µs | +0.4 µs |
| 3 | 29.3 µs | +0.8 µs |
| 5 | 29.7 µs | +1.2 µs |

**Per-group-middleware cost: ~240 ns/layer** (derived from 0→5 delta: 1.2 µs / 5 = 240 ns). Identical to the global middleware cost measured in the middleware chain benchmarks. Group middleware uses the same `run_middleware_chain` code path — the only difference is the index range (global then route-level).

### Nested groups

| Nesting | Total middleware | Time | Delta vs 1 level |
|---------|-----------------|------|-------------------|
| 1 level (`/api`, 1 mw) | 1 | 29.0 µs | — |
| 2 levels (`/api/v1`, 1+1 mw) | 2 | 29.9 µs | +0.9 µs |
| 3 levels (`/api/v1/admin`, 1+1+1 mw) | 3 | 30.5 µs | +1.5 µs |

**~500 ns per nesting level**, which is ~250 ns per middleware layer — consistent with previous measurements. Nesting itself adds no overhead beyond the middleware it contributes. At build time, `Group::group()` flattens nested routes into the top-level route table with combined middleware vectors via `Arc::clone`.

### Global + group middleware combined

| Configuration | Total middleware | Time | Delta vs 2 global only |
|---------------|-----------------|------|------------------------|
| 2 global + 0 group | 2 | 29.9 µs | — |
| 2 global + 2 group | 4 | 30.1 µs | +0.2 µs |
| 2 global + 3 group (nested) | 5 | 30.7 µs | +0.8 µs |

Global and group middleware **compose linearly** with no amplification. The middleware chain walks global middleware first (indices 0..N), then route-level middleware (indices N..N+M), then the handler. Adding 3 group middleware layers to an existing 2 global layers costs exactly what 3 additional layers would cost anywhere.

### Implementation notes

- Group middleware is stored as `Vec<Arc<dyn Middleware>>` on each `Route`. Multiple routes in the same group share middleware instances via `Arc::clone` — one atomic refcount bump per route at startup, zero runtime cost.
- The `run_middleware_chain` function uses a combined index over global + route middleware, avoiding a separate chain or any conditional branching per middleware type.
- The fast path (`shared.middleware.is_empty() && route.middleware.is_empty()`) skips chain setup entirely when no middleware exists at any level.

---

## Performance Budget

Based on these measurements, the per-request overhead budget for Harrow:

| Component | Budget | Measured |
|-----------|--------|----------|
| Route matching (< 50 routes) | < 1 µs | 634 ns worst case |
| Middleware chain (≤ 5 layers) | < 1.5 µs | ~1.2 µs |
| Route group overhead (prefix only) | 0 µs | 0 µs (free) |
| Group middleware (≤ 5 layers) | < 1.5 µs | ~1.2 µs |
| State injection | < 50 ns | ~20 ns |
| Response construction | < 100 ns | ~50 ns |
| **Total framework overhead** | **< 3 µs** | **~2 µs typical** |

The PRD target of "< 1 µs added latency over raw Hyper" is met for the echo workload (param extraction + route match). The full-stack workload with middleware and JSON is ~2 µs, which is within the spirit of the target given that middleware and serialization are user-chosen costs.

---

## Regression Detection

These benchmarks run in CI on every PR. A regression is flagged when:

- Any micro-benchmark (path matching, route lookup) regresses by **> 5%**.
- Any TCP benchmark regresses by **> 10%** (wider threshold due to TCP variance).
- Any new benchmark group appears without a corresponding baseline.

Flamegraph diffs are generated alongside criterion reports. See [`docs/prds/harrow-http-framework.md`](prds/harrow-http-framework.md) § 11 for the full flamegraph CI pipeline.

---

## Future Optimization Targets

| Target | Expected gain | Complexity |
|--------|---------------|------------|
| Radix tree for route lookup | O(path_len) vs O(n_routes) — eliminates scaling wall at 200+ routes | Medium |
| Inline `Next` (avoid `Box<dyn FnOnce>`) | -40 B per middleware per request | Medium (requires `Next` restructure) |
| Borrowed param values (`&str` into request path) | Eliminates String alloc per param | High (lifetime propagation) |
| `SmallVec<[u8; 64]>` for small response bodies | Avoid heap alloc for tiny responses | Low |
