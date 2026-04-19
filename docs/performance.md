# Harrow Performance Notes

**Date:** 2026-02-20
**Historical baseline version:** 0.1.0-dev
**Platform:** macOS (Darwin 24.6.0), Apple Silicon
**Rust:** edition 2024, release profile (`opt-level = 3`, `lto = "thin"`, `debug = true`)
**Benchmark tools:** criterion 0.5; `spinr` + `harrow-bench` for remote perf runs

This document is a historical baseline from Harrow's earlier Hyper/Tokio phase.
Current runtime-direction decisions live in
[`docs/strategy-local-workers.md`](./strategy-local-workers.md) and
[`docs/article.md`](./article.md).

---

## Current Benchmark Workflow

Use the benchmark surface in [mise.toml](/Users/l1x/code/home/vectorian-rs/harrow/mise.toml:1) for the current workflow. There is no single `bench:run` task anymore.

### Local Microbenchmarks

```bash
cargo bench
cargo bench --bench echo
cargo bench --bench middleware_chain
cargo bench --bench full_stack
cargo bench --bench route_groups
```

Criterion output is written to `target/criterion/`.

### Remote Harrow vs ntex

The shortest current path for the Tokio/local-worker comparison is:

```bash
mise run bench:verify
mise run bench:baseline:all
mise run bench:perf:all
mise run bench:compare:harrow-vs-ntex
```

What each task does:

- `bench:verify`: checks the benchmark hosts and OS tuning.
- `bench:baseline:all`: runs direct `harrow-remote-perf-test` baseline captures for Harrow Tokio and ntex Tokio using `spinr`.
- `bench:perf:all`: reruns the same direct comparison with `perf` capture enabled.
- `bench:compare:harrow-vs-ntex`: runs the broader suite comparison through `bench-compare` and `harrow-bench/suites/framework-comparison.toml`.

### Full Suite And Registry-Driven Runs

Use the `bench:run:*` tasks when you want the registry/suite workflow rather than the direct Harrow-vs-ntex runner:

```bash
mise run bench:run:harrow-tokio-mimalloc
mise run bench:run:ntex-tokio-mimalloc
mise run bench:run:all
```

These tasks use:

- [harrow-bench/implementations.toml](/Users/l1x/code/home/vectorian-rs/harrow/harrow-bench/implementations.toml:1)
- [harrow-bench/suites/framework-comparison.toml](/Users/l1x/code/home/vectorian-rs/harrow/harrow-bench/suites/framework-comparison.toml:1)
- [harrow-bench/src/bin/bench_single.rs](/Users/l1x/code/home/vectorian-rs/harrow/harrow-bench/src/bin/bench_single.rs:1)
- [harrow-bench/src/bin/bench_compare.rs](/Users/l1x/code/home/vectorian-rs/harrow/harrow-bench/src/bin/bench_compare.rs:1)

### Results And Re-rendering

Remote runs write results under `docs/perf/<instance-type>/<timestamp>/`.

To re-render a captured run:

```bash
cargo run -p harrow-bench --bin render-perf-summary -- docs/perf/<instance>/<timestamp>
```

### Direct Runner

The direct remote orchestrator used by `bench:baseline:*` and `bench:perf:*` is:

```bash
cargo run -p harrow-bench --release --bin harrow-remote-perf-test -- \
  --mode remote \
  --server-ssh <server-public-ip> \
  --client-ssh <client-public-ip> \
  --server-private <server-private-ip> \
  --instance-type c8gn.12xlarge \
  --framework harrow \
  --backend tokio \
  --allocator mimalloc \
  --duration 30 \
  --warmup 5 \
  --config harrow-bench/spinr/text-c128.toml
```

---

## Historical Reproduction Notes

```bash
cargo bench
cargo bench --bench echo
cargo bench --bench middleware_chain
cargo bench --bench full_stack
cargo bench --bench route_groups
```

Results are written to `target/criterion/`. Open `target/criterion/report/index.html` for interactive charts.

For remote perf capture and rendered summaries, use the Rust perf runner:

```bash
# Run the remote perf orchestrator directly
cargo run -p harrow-bench --bin harrow-remote-perf-test -- \
  --server-ssh <server-public-ip> \
  --client-ssh <client-public-ip> \
  --server-private <server-private-ip> \
  --instance-type c8g.12xlarge \
  --duration 20 \
  --warmup 2 \
  --os-monitors \
  --perf \
  --perf-mode both \
  --config harrow-bench/spinr/text-c128.toml \
  --config harrow-bench/spinr/json-1kb-c128.toml

# Re-render a results directory later if needed
cargo run -p harrow-bench --bin render-perf-summary -- docs/perf/<instance>/<timestamp>
```

By default, the runner writes results under `docs/perf/<instance-type>/<timestamp>/`.
Each run directory contains the raw JSON metrics, host-monitor logs, perf artifacts,
and rendered outputs such as `summary.md` and `summary.svg`.

---

## Remote Perf Runner

Remote perf sessions are driven by the Rust orchestrator in
`harrow-bench/src/bin/harrow_remote_perf_test.rs`.

The runner:

1. connects to separate server and client machines over SSH
2. starts `harrow-perf-server` or `axum-perf-server` in Docker on the server host
3. renders `spinr` TOML templates by filling `{{ server }}`, `{{ duration }}`, and `{{ warmup }}`
4. uploads the rendered config to the client and runs `spinr bench ... -j`
5. optionally collects `vmstat`, `sar`, `iostat`, `pidstat`, `perf stat`, and `perf record`
6. copies artifacts into the local results directory and writes per-run `*.meta.json`
7. calls `perf_summary::render_results_dir()` to emit `summary.md`, `summary.svg`, telemetry SVGs, and local flamegraphs when the required tools are available

If you already have a populated results directory, `render_perf_summary` is the thin Rust wrapper around the same summary renderer.

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
| `text_no_mw` (baseline) | 24.4 µs | — |
| `json_no_mw` | 24.8 µs | +0.4 µs |
| `param_no_mw` (`/users/:id`) | 25.1 µs | +0.7 µs |
| `404_miss` | 24.1 µs | -0.3 µs |

### Analysis

- **Loopback TCP dominates at ~24 µs.** This includes kernel TCP stack, hyper's HTTP/1.1 parser, and the response write path. Harrow's routing overhead is invisible at this scale.
- **JSON serialization adds ~0.4 µs.** `serde_json::to_writer` into `BytesMut(128)` for a small `{"status":"ok","code":200}` payload.
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

The historical PRD target of "< 1 µs added latency over raw Hyper" was met for
the echo workload (param extraction + route match). The full-stack workload
with middleware and JSON is ~2 µs, which was within the spirit of that earlier
target given that middleware and serialization are user-chosen costs.

---

## Perf Review Workflow

Performance review is currently manual rather than CI-gated.

- Use `cargo bench` for local microbench and TCP benchmark changes.
- Use `harrow-remote-perf-test` when you need full remote captures with `perf stat`,
  `perf record`, host telemetry, and side-by-side Harrow/Axum results.
- The runner renders `summary.md`, `summary.svg`, telemetry SVGs, and local flamegraphs
  from captured `perf script` output when the required tools are available.
- Compare a fresh run against a known-good prior run before treating a hotspot shift as real.

---

## Framework Comparison: Harrow vs Axum

In addition to internal micro-benchmarks, Harrow includes an external load-test comparison against [Axum](https://github.com/tokio-rs/axum) using the `mcp-load-tester` bench binary.

### What it measures

Both frameworks serve identical endpoints with no middleware:

| Endpoint | Response |
|----------|----------|
| `GET /` | `"hello, world"` (text) |
| `GET /greet/:name` | `"hello, bench"` (text with path param) |
| `GET /health` | `{"status":"ok"}` (JSON) |

Each combination is tested at concurrency levels 1, 4, 8, and 16 with 10-second runs and 3-second warmup.

### How to run

**Criterion micro-benchmarks** (same BenchClient, TCP round-trip):

```bash
# Harrow echo benchmarks
cargo bench --bench echo

# Axum echo benchmarks (same test patterns)
cargo bench --bench axum_echo
```

**External load-test comparison** (Rust runner around the `bench` binary):

```bash
# Option 1: auto-discover bench binary
cargo run -p harrow-bench --bin compare_frameworks --

# Option 2: explicit path
cargo run -p harrow-bench --bin compare_frameworks -- \
  --bench-bin /path/to/bench

# Option 3: remote target host
cargo run -p harrow-bench --bin compare_frameworks -- \
  --remote \
  --server-host 10.0.1.5 \
  --bench-bin /path/to/bench
```

Results are written to `target/comparison/`:
- Per-test JSON files with HdrHistogram percentiles
- `comparison-report.md` — markdown summary table

### Fairness principles

- Same Tokio runtime, same allocator, same `--release` profile
- Byte-identical response bodies where possible
- Sequential testing (never both servers under load simultaneously)
- Same warmup period, duration, and concurrency levels
- Same `BenchClient` for Criterion benchmarks

### Server binaries

The comparison uses two minimal server binaries in `harrow-bench`:

```bash
# Build both
cargo build --release --bin harrow-server-tokio --bin axum-server

# Run individually (default port 3000)
target/release/harrow-server-tokio --port 3001
target/release/axum-server --port 3002
```

---

## Statistical Significance Testing

Criterion benchmarks report point estimates with confidence intervals, but these can mislead when comparing two frameworks — a 5% gap may be noise from thermal throttling, OS scheduling, or CPU cache state. The `stat-bench` binary provides rigorous paired statistical testing.

### Methodology

**Paired t-test with bias cancellation.** Each trial measures both Harrow and Axum under identical conditions. The order alternates every trial (even trials: Harrow first; odd trials: Axum first) to cancel systematic bias from thermal effects and cache warming.

Each trial consists of 50 rounds of (32 connections × 10 requests each) = 16,000 requests per trial. Three handler types are tested independently:

| Handler | What it isolates |
|---------|------------------|
| **Text** (`"ok"`) | Pure framework overhead — routing, connection handling, response framing |
| **JSON 1KB** (10-user array) | Framework + serialization cost |
| **Simulated I/O** (100µs sleep + JSON 1KB) | Framework under async contention (models DB queries) |

### Metrics reported

| Metric | What it means |
|--------|---------------|
| **Mean ± StdDev** | Average ms/round for each framework |
| **Diff** | Paired mean difference (Harrow − Axum), absolute and relative |
| **95% CI** | Confidence interval for the true mean difference |
| **t-statistic, p-value** | Paired t-test; p < 0.05 = statistically significant |
| **Cohen's d** | Effect size: < 0.2 = negligible, 0.2–0.5 = small, 0.5–0.8 = medium, > 0.8 = large |
| **Required n** | Sample size needed for 80% power to detect the observed effect at α = 0.05 |

### How to run

```bash
# Default: 30 trials (takes ~5 minutes)
cargo run --release --bin stat-bench

# Custom trial count
cargo run --release --bin stat-bench -- 50
```

Run on AC power with minimal background activity. Battery mode introduces ~30% variance.

### Interpreting results

A result is **actionable** when all three conditions hold:

1. **p < 0.05** — the difference is statistically significant
2. **Cohen's d > 0.2** — the effect size is at least small (not just detectable noise)
3. **95% CI excludes zero** — the direction of the difference is certain

If "Required n" exceeds ~100, the effect is too small to matter in practice — even if p < 0.05 with enough trials, a 0.1% difference is not worth optimizing.

### Key findings (Apple Silicon, AC power, 30 trials)

| Handler | Diff | p-value | Cohen's d | Significant? |
|---------|------|---------|-----------|--------------|
| Text | ~0% | 0.83 | ~0.04 | No — frameworks are identical |
| JSON 1KB | −4.3% (Harrow faster) | < 0.0001 | 0.89 (large) | **Yes** — Harrow borrows JSON; Axum clones |
| Sim I/O | ~−2% | 0.04 | ~0.38 | Marginal — needs ~57 trials for 80% power |

The JSON 1KB advantage is architectural: `Response::json(&*JSON_1KB)` borrows the static value, while Axum's `Json(JSON_1KB.clone())` must clone the `serde_json::Value` tree. For the text and simulated I/O handlers, both frameworks are thin wrappers around the same hyper + tokio stack with no measurable difference.

### Concurrent profiling

For deeper investigation when criterion shows an unexpected gap, use the profiling binary:

```bash
# Sweep concurrency levels (8, 32, 64, 128, 256) across all handler types
cargo run --release --bin profile-concurrent

# Profile one framework only (for flamegraph)
cargo run --release --bin profile-concurrent -- harrow
cargo run --release --bin profile-concurrent -- axum
```

This runs 100 rounds per concurrency level with 5-round warmup, printing a formatted comparison table. Use it to confirm whether a criterion gap is real before investigating further.

---

## Optimization History

### Hot-path allocation elimination (2026-02-25)

Closed the ~7% gap vs Axum on JSON responses (26.3 µs → 24.8 µs). Three changes:

| Change | File(s) | What it eliminated |
|--------|---------|-------------------|
| `serde_json::to_writer` into `BytesMut(128)` | `response.rs` | Intermediate `Vec<u8>` allocation in `to_vec()` |
| `set_header_static` with `HeaderValue::from_static` | `response.rs` | Per-request header name parsing + value validation |
| `PathPattern.raw`: `String` → `Arc<str>` | `path.rs`, `request.rs`, `lib.rs` | Per-request `to_string()` heap allocation for route pattern |

**Result:** Harrow JSON is now within noise of Axum (~24.8 µs vs ~24.7 µs).

---

## Future Optimization Targets

| Target | Expected gain | Complexity | When to pursue |
|--------|---------------|------------|----------------|
| Radix tree for route lookup | O(path_len) vs O(n_routes) — ~800 ns at 100 routes | Medium | When exceeding ~100 routes |
| Inline `Next` (avoid `Box<dyn FnOnce>`) | ~10 ns per middleware layer | Very high | Diminishing returns at current scale |
| Borrowed param values (`&str` into request path) | ~40 ns per parameterized route | Very high (lifetime propagation) | Not recommended without major API refactor |
| ~~`SmallVec<[u8; 64]>` for small response bodies~~ | ~~Avoid heap alloc for tiny responses~~ | ~~Low~~ | Superseded by `BytesMut` + `to_writer` approach |
| ~~Zero-cost static headers~~ | ~~Skip header name/value parsing~~ | ~~Low~~ | Done (2026-02-25) |
| ~~Eliminate route pattern `to_string()`~~ | ~~Skip per-request String alloc~~ | ~~Low~~ | Done (2026-02-25) |

**Status:** Framework overhead is ~2 µs on a ~22 µs TCP baseline. At parity with Axum. Remaining optimizations offer sub-50 ns gains except for radix tree routing (relevant only at >100 routes). Diminishing returns reached for typical workloads.
