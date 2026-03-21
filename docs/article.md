# Harrow Performance Journal

**Status:** living document  
**Last updated:** 2026-03-21

This is not a launch post and not a benchmark victory lap. It is a dated
engineering log for Harrow's performance work: what we measured, what moved the
numbers, what did not, and which optimizations we deliberately refused to do.

The intended audience is technical readers on places like Hacker News and
Reddit, but the real goal is internal memory. Performance arguments decay
quickly. This file exists so future us can see the evidence chain instead of
reconstructing it from commit history and half-remembered flamegraphs.

## Executive Summary

- On 2026-03-19, Harrow was still about **2x slower than Axum** on a matched
  remote throughput test.
- The root cause was not routing, JSON, or Hyper. It was **per-connection
  Tokio timers** for header read timeout and connection lifetime.
- Making those timers optional and disabling them in the benchmark servers
  moved Harrow from **501,742 -> 1,041,052 RPS** on `/text` and
  **589,757 -> 967,501 RPS** on `/json/1kb` on `c8g.12xlarge`.
- We also benchmarked the middleware architecture directly instead of arguing in
  the abstract. Pure Tower-style static layers are effectively allocation-flat.
  Harrow's dynamic middleware costs about **+728 bytes and +2 allocs per noop
  layer**. Axum's ergonomic `middleware::from_fn` path is much steeper.
- That still does **not** justify rewriting `Next` yet. On a realistic
  `cors + compression + session` stack, the dominant cost is session and
  compression work, not middleware-chain plumbing.

## What Harrow Is Optimizing For

Harrow is trying to be a thin, macro-free HTTP framework on top of Hyper.
There are three design constraints:

1. Framework overhead should stay below transport and application noise.
2. The fast path should be understandable without Tower-level type archaeology.
3. We will accept some runtime indirection if it avoids compile-time and API
   complexity explosion, but only when the measured cost is small.

That third constraint matters. A lot of Rust performance discussions collapse
into "dynamic dispatch bad, monomorphization good." That is too simple to be
useful. The real question is always: **what did the extra flexibility cost on
the workload we actually care about?**

## Measurement Stack

We measure Harrow at four levels.

| Level | What it isolates | Typical tool |
|---|---|---|
| Micro | Route/path logic and other pure CPU work | Criterion |
| In-process dispatch | Request -> middleware -> handler without TCP | Criterion + custom alloc harness |
| Local TCP | Full HTTP/1.1 request-response over loopback | Criterion + `BenchClient` |
| Remote throughput | Real network, scheduler, allocator, kernel contention | Docker + `perf` + `sar` on AWS |

This split is important because different problems only appear at different
scales.

- Path matching bugs show up in micro benches.
- Middleware allocation slopes show up in in-process dispatch.
- Request/response overhead shows up in local TCP.
- Scheduler and timer contention only showed up in the remote throughput test.

### Tools

- **Criterion 0.5** for timing and regression detection.
- **`TrackingAllocator`** in
  `harrow-bench/src/bin/measure_allocs.rs` for `alloc_bytes` and
  `alloc_count` per operation.
- **Committed baselines** in `harrow-bench/benches/baseline.toml`.
- **Rendered summaries** from `harrow-bench/src/bin/render_baseline.rs`.
- **Remote artifacts** in `docs/perf/...` including `perf report`, `sar`, JSON
  output, and telemetry charts.

### Fairness Rules

We only trust comparison numbers when the workload is genuinely matched.

That means:

- same route corpus
- same client behavior
- same runtime shape
- same response semantics
- one experimental variable at a time

One concrete example: at one point the remote JSON benchmark was not matched.
Harrow served JSON through its high-level helper while Axum used a manual
`serde_json::to_vec` + raw bytes response. That is a benchmark bug, not a
framework comparison. We fixed the Axum perf server to use `axum::Json` so the
comparison now measures comparable response construction on both sides.

Another example: Harrow originally had per-connection timeouts enabled in the
server benchmark path while Axum's default `axum::serve` path did not configure
equivalent per-connection timers. Again, not a fair framework comparison.

### What Gets Written Down

If a number matters, it should live somewhere durable:

- `harrow-bench/benches/baseline.toml` stores local timing and allocation
  baselines.
- `docs/perf/c8g.12xlarge/.../summary.md` stores remote server results.
- This file explains what changed and why.

## 2026-03-11: Local Baseline After the First Cleanup Pass

The first useful baseline was local, on Apple Silicon, after the initial route
and serialization cleanup work. This snapshot is still useful because it tells
us what Harrow looked like before the remote timer issue dominated the story.

### Micro and Dispatch Numbers

From `harrow-bench/benches/baseline.toml`:

| Benchmark | Time | Allocations |
|---|---:|---:|
| Exact path match (`/health`) | 14.2 ns | 0 B / 0 allocs |
| 1-param path match (`/users/:id`) | 66.8 ns | 196 B / 3 allocs |
| Glob match (`/files/*path`) | 130.9 ns | 271 B / 4 allocs |
| Route lookup, 100 routes worst case | 85.3 ns | 52 B / 3 allocs |
| In-process dispatch, text, 0 middleware | 324.8 ns | 1389 B / 6 allocs |
| In-process dispatch, text, 5 noop middleware | 863.8 ns | 5029 B / 16 allocs |
| In-process dispatch, JSON 1KB, 0 middleware | 2780.9 ns | 5381 B / 12 allocs |

The route lookup story changed dramatically once Harrow switched from a linear
scan to `matchit`. Route count stopped mattering in any meaningful way for the
local profile. That was a real win, but it was also a warning: once route
lookup is down in the tens of nanoseconds, it is no longer where to spend your
attention.

![Local baseline dashboard](./performance.svg)

### Local TCP Comparison Against Axum

From the same baseline snapshot:

| Benchmark | Harrow | Axum |
|---|---:|---:|
| Text echo | 22.74 us | 27.46 us |
| JSON echo | 23.21 us | 24.99 us |
| Param echo | 23.15 us | 24.88 us |
| 404 miss | 22.50 us | 24.40 us |

So locally, Harrow was already ahead on latency by about 7-17% depending on the
case.

This baseline was taken after Harrow's `BoxBody` refactor, so the local win was
not coming from a fantasy "everything is monomorphized and concretely typed"
response path. That distinction matters because it keeps the article honest
about which design choices were still present when these numbers were recorded.

The allocation story was more nuanced than the original article draft implied:

| Benchmark | Harrow | Axum |
|---|---:|---:|
| Text echo | 9898 B / 11 allocs | 9449 B / 17 allocs |
| JSON echo | 10686 B / 17 allocs | 10238 B / 23 allocs |
| Param echo | 9956 B / 14 allocs | 10143 B / 21 allocs |
| 404 miss | 8762 B / 9 allocs | 9030 B / 12 allocs |

Harrow was clearly doing **fewer allocations**, but not necessarily fewer bytes
in every case. That is worth writing down because it killed an easy but wrong
story. The local latency win was not "Harrow allocates 6x less memory than
Axum." The reality was narrower and more honest: Harrow was ahead on the local
TCP path while still sharing plenty of boxed and buffered machinery with the
rest of the Rust HTTP ecosystem.

### What Moved the Early Local Numbers

The important early improvements were straightforward:

- serializing JSON directly into a buffer instead of bouncing through extra
  temporary allocations
- using static header values where possible
- switching routing to `matchit`
- keeping the API and request path simple enough that we could still reason
  about what was actually happening

## Monomorphization vs Dynamic Dispatch

This is the section we kept having to rediscover in chat, so it belongs in the
article.

### Harrow's Middleware Model

Today Harrow's middleware API is intentionally dynamic:

```rust
type BoxFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

pub trait Middleware: Send + Sync {
    fn call(&self, req: Request, next: Next) -> BoxFuture;
}

pub struct Next {
    inner: Box<dyn FnOnce(Request) -> BoxFuture + Send>,
}
```

That buys a simple user-facing shape:

```rust
async fn my_middleware(req: Request, next: Next) -> Response
```

No macros. No Tower stack types in user errors. No giant generic router type.

### Tower's Model

Tower pushes much more of the stack to compile time:

```rust
trait Service<Request> {
    type Response;
    type Error;
    type Future: Future<Output = Result<Self::Response, Self::Error>>;
}
```

Layers wrap services into nested concrete types. That means the compiler can
see the entire stack, inline aggressively, and often eliminate per-request
allocation at the middleware boundary.

The trade-off is not imaginary:

- more monomorphization
- larger generated types
- worse compile times and diagnostics
- more pressure to shape the public API around Tower's abstraction model

### What We Measured Instead of Assuming

We built an allocation harness for three cases:

1. Harrow's dynamic noop middleware
2. Axum's ergonomic `middleware::from_fn`
3. A pure Tower-style generic noop layer

For the `text` path:

| Stack | Depth 0 | Depth 10 | Increment per layer |
|---|---:|---:|---:|
| Harrow | 1389 B / 6 allocs | 8669 B / 26 allocs | about `+728 B`, `+2 allocs` |
| Axum `from_fn` | 1052 B / 14 allocs | 14732 B / 164 allocs | about `+1368 B`, `+15 allocs` |
| Pure Tower | 693 B / 4 allocs | 693 B / 4 allocs | effectively `+0` |

Timing told the same story. Harrow's noop middleware slope is about
`100-125 ns` per layer. Pure Tower is flatter. Axum's ergonomic path is much
heavier.

This is the real trade-off:

- **Pure Tower** is the low-allocation ideal.
- **Axum `from_fn`** is the ergonomic dynamic end of the spectrum.
- **Harrow** sits in the middle on purpose.

That middle ground is defensible as long as the incremental cost stays small on
real workloads.

## 2026-03-19: Remote Throughput Said We Still Had a Serious Problem

The local story looked good. The remote story did not.

On `c8g.12xlarge`, before the timer fix, Harrow was getting crushed:

| Case | Harrow | Axum | Delta |
|---|---:|---:|---:|
| `/text`, c=128 | 501,742 RPS | 1,019,224 RPS | -50.77% |
| `/json/1kb`, c=128 | 589,757 RPS | 998,524 RPS | -40.94% |

The perf reports made the problem obvious.

From `docs/perf/c8g.12xlarge/2026-03-19T07-36-57Z/harrow_text_c128.server.perf-report.txt`:

- `7.64%` in `tokio::time::sleep::Sleep::poll`
- `6.96%` in `drop_in_place<tokio::time::sleep::Sleep>`
- `5.03%` in `parking_lot_core::word_lock::WordLock::lock_slow`

From the matching JSON run:

- `6.10%` in `Sleep::poll`
- `5.63%` in `drop_in_place<Sleep>`
- `3.07%` in `WordLock::lock_slow`

This was not a subtle micro-optimization issue. Harrow's benchmark server was
creating a Tokio timer and a `Sleep` per connection for:

- header read timeout
- connection lifetime timeout

At 500K+ RPS across 48 cores, that meant a lot of traffic through Tokio's timer
machinery and its internal locks. Axum's default server path was not paying the
same cost.

![Initial remote throughput dashboard](./perf/c8g.12xlarge/2026-03-19T07-36-57Z/summary.svg)

![Initial `/text` server telemetry](./perf/c8g.12xlarge/2026-03-19T07-36-57Z/text-c128.server.telemetry.svg)

### The Fix

We did not remove the safety features globally. We made them optional.

`ServerConfig` now keeps production defaults:

- `header_read_timeout: Some(5s)`
- `connection_timeout: Some(300s)`

But the benchmark servers use `serve_with_config` with both timeouts set to
`None`, which removes timer setup from the hot path and matches Axum's default
serving path more closely.

### What Changed

After disabling those timers in the benchmark servers:

| Case | Before | After | Improvement |
|---|---:|---:|---:|
| `/text`, c=128 | 501,742 RPS | 1,041,052 RPS | +107.5% |
| `/json/1kb`, c=128 | 589,757 RPS | 967,501 RPS | +64.0% |

And the matched comparison against Axum moved to:

| Case | Harrow | Axum | Delta |
|---|---:|---:|---:|
| `/text`, c=128 | 1,041,052 RPS | 1,055,730 RPS | -1.39% |
| `/json/1kb`, c=128 | 967,501 RPS | 1,017,350 RPS | -4.90% |

For `/text`, Harrow was effectively at parity. The old "Harrow is 2x slower"
statement stopped being true the moment the timer issue was removed.

![Post-fix remote throughput dashboard](./perf/c8g.12xlarge/2026-03-19T13-34-49Z/summary.svg)

![Post-fix `/text` server telemetry](./perf/c8g.12xlarge/2026-03-19T13-34-49Z/text-c128.server.telemetry.svg)

### Important Lesson

The remote bottleneck was not where local Criterion pointed us.

Local benchmarks were still useful. They told us routing and JSON were already
pretty tight. But the actual throughput collapse turned out to be a scheduler
and timer-wheel problem that only appeared at real server concurrency.

That is why the measurement stack has multiple levels.

## Benchmark Fairness Matters More Than Clever Explanations

The remote JSON comparison had another trap: we initially were not comparing the
same response construction strategy.

Harrow's perf server served JSON through Harrow's normal JSON response path.
Axum's perf server was manually doing `serde_json::to_vec` and returning raw
bytes. That mixes framework overhead with application policy.

We fixed the Axum perf server so the JSON routes now use `axum::Json(...)`,
which is the fairer comparison for "what does the framework's normal JSON
response path cost?"

This is a broader rule we are trying to keep:

- compare framework to framework
- compare helper to helper
- compare manual bytes to manual bytes

Anything else is storytelling.

## 2026-03-20: Should Harrow Rewrite `Next`?

Once the timer bug was fixed, the next obvious question was middleware.

Harrow's middleware path still has two dynamic pieces:

- boxed middleware futures
- a boxed `Next` continuation

It is tempting to look at that and jump straight to a Tower-style rewrite. We
did not do that. We measured it first.

### Noop Middleware Slope

In-process dispatch showed a clean linear Harrow slope:

- about `+728 bytes` per layer
- about `+2 allocs` per layer
- about `+100-125 ns` per layer

That is real overhead, but it is not automatically a problem.

### What Would a Rewrite Buy?

The most conservative rewrite would remove the boxed `Next` continuation while
keeping the public middleware shape the same. That would likely reduce some of
the per-layer slope without forcing a Tower-style API onto users.

But complexity has to be justified. We set a bar:

- either the realistic stack has to show middleware plumbing as a meaningful
  share of cost
- or an A/B implementation has to win clearly, not by a handful of nanoseconds

So we built a realistic stack benchmark before touching `Next`.

## 2026-03-20: Realistic Stack - `cors + compression + session`

This is the most useful recent addition to the benchmark suite.

The new session middleware adds request extensions, signed cookies, session
store access, and `set-cookie` emission. We benchmarked both session in
isolation and a more realistic stack with CORS and compression.

### Session-Only Criterion Results

Fresh local run from `cargo bench -p harrow-bench --bench session -- --noplot`:

| Scenario | Time |
|---|---:|
| `baseline_0mw` | about 32.0 us |
| `session_noop` | about 32.7 us |
| `session_existing_read` | about 32.1 us |
| `session_existing_write` | about 33.6 us |
| `session_new` | about 35.3 us |
| `session_read_plus_noop` | about 32.3 us |

The key comparison is `session_existing_read` vs `session_read_plus_noop`:

- read: about `32.1 us`
- read + one noop middleware: about `32.3 us`

That is effectively flat.

### Session Allocation Results

From the allocation harness:

| Scenario | Allocations |
|---|---:|
| `session_noop` | 11214 B / 19 allocs |
| `session_existing_read` | 12108 B / 31 allocs |
| `session_existing_write` | 13422 B / 53 allocs |
| `session_new` | 12996 B / 37 allocs |
| `session_read_plus_noop` | 12836 B / 33 allocs |

Again, `session_existing_read -> session_read_plus_noop` is exactly the
expected Harrow noop slope:

- `+728 bytes`
- `+2 allocs`

So the middleware plumbing overhead is present, but small and unsurprising.

### Realistic Stack Results

For a 1 KB text response with realistic request headers plus
`session + cors + compression`:

Criterion:

| Scenario | Time |
|---|---:|
| `realistic_stack_baseline` | about 30.7 us |
| `realistic_stack_read` | about 44.7 us |
| `realistic_stack_write` | about 47.2 us |

Allocation harness:

| Scenario | Allocations |
|---|---:|
| `realistic_stack_baseline` | 10298 B / 18 allocs |
| `realistic_stack_read` | 369043 B / 69 allocs |
| `realistic_stack_write` | 370357 B / 91 allocs |

Those are not typo-level numbers. The realistic stack is far heavier than the
noop middleware slope.

### Why the Realistic Stack Is Heavy

The code explains a lot of it.

The compression middleware currently:

1. collects the full body
2. copies it into a `Vec<u8>`
3. compresses into another `Vec<u8>`
4. rebuilds the response and copies headers back

That is visible directly in `harrow-middleware/src/compression.rs`.

The session middleware also does real work:

- parse and verify cookies
- load and clone session data from the store
- snapshot session data again on write
- append `set-cookie` headers on mutation

So for the realistic stack, the numbers say something very specific:

**do not rewrite `Next` yet.**

On this workload, `Next` is not where the money is.

## What We Are Deliberately Not Doing Yet

### Not Rewriting the Middleware Stack Into Pure Tower

Yes, a pure Tower-style static middleware stack can eliminate a lot of
per-request allocation. We measured the upside.

We are still not doing it yet because:

- Harrow's current user-facing middleware API is much simpler
- the realistic-stack bottleneck is elsewhere
- a hybrid dynamic/static design would create a second conceptual model to own
- compile-time complexity and code-size growth are real costs, not imaginary

If we ever take this step, it should be because realistic workloads justify it,
not because "zero allocations" sounds cleaner in a design doc.

### Not Pooling Futures or Continuations

Pooling is the kind of optimization that looks clever long before it looks
correct. Async cancellation, lifetime management, and reuse semantics can make
it ugly fast. We would rather remove an allocation structurally than build a
pool around it.

### Not Mixing In Unmeasured Allocator Changes

`mimalloc` is a separate experiment. We intentionally did not fold allocator
changes into the timer story because that would have muddied attribution. If an
allocator change helps, we want to know that separately.

## Verification, Not Just Benchmarking

Performance work is easy to cargo-cult unless it sits next to correctness work.

We now keep a separate verification strategy in `docs/verification.md`. The
short version:

- `proptest` for path matching, middleware ordering, compression round-trips,
  and rate-limiter arithmetic
- fuzzing for parsing surfaces
- ordinary unit and integration tests for the thin wrapper middleware

We are explicitly **not** pretending this project needs TLA+, Jepsen, or other
distributed-systems machinery. Harrow is an HTTP framework, not a consensus
engine.

## Current Thesis

As of 2026-03-21, the strongest performance conclusions are:

1. Harrow's local routing and dispatch costs are already small.
2. The big remote throughput regression was caused by per-connection timers.
3. Middleware allocation slope is real, measurable, and still much cheaper than
   Axum's ergonomic `from_fn` path.
4. Pure Tower-style static layers are still the lower-allocation ideal.
5. The next optimization worth chasing is probably in compression/session work,
   not in rewriting `Next`.
6. Connection-level timeouts (header read, connection lifetime) are a security
   feature that has a measurable performance cost. Making them optional and
   configurable was the right call — it keeps production safe by default while
   letting benchmark and proxy-shielded deployments remove the overhead.

That last point is the one most worth preserving. We now have enough data to
avoid a complexity explosion for a marginal win.

## How To Reproduce The Current Evidence

Local timing and allocation:

```bash
cargo bench
cargo run --bin update-baseline
cargo run --release -p harrow-bench --bin measure-allocs
cargo run --bin render-baseline
```

Session and realistic-stack work:

```bash
cargo bench -p harrow-bench --bench session -- --noplot
cargo run --release -p harrow-bench --bin profile-session -- read
cargo run --release -p harrow-bench --bin profile-session -- write
cargo run --release -p harrow-bench --bin profile-session -- stack-read
cargo run --release -p harrow-bench --bin profile-session -- stack-write
```

Remote throughput artifacts:

- initial slow run:
  `docs/perf/c8g.12xlarge/2026-03-19T07-36-57Z/`
- post-timer fix matched run:
  `docs/perf/c8g.12xlarge/2026-03-19T13-34-49Z/`

## 2026-03-21: Connection Safety — Timers as a Security Feature, Not Just Overhead

The timer story from March 19 had a sequel.

After fixing the benchmark regression by making timers optional, we stepped
back and asked: what do those timers actually protect against? And what
connection-level threats does Harrow handle versus leave to the reverse proxy?

### The Threat Model

HTTP servers face four connection-level attacks that happen before the
application handler ever runs:

1. **Slow-loris:** client trickles headers at 1 byte/sec, never completing.
2. **Idle connections:** client opens TCP, sends nothing, holds a slot.
3. **Slow-read:** client reads the response body at 1 byte/sec.
4. **Connection flood:** thousands of simultaneous connections exhaust FDs.

These are distinct from handler timeouts. `TimeoutMiddleware` only activates
after the full request is received. Connection-level attacks happen before
the request is parsed.

### What Harrow Covers Today

| Threat | Defense | Default |
|---|---|---|
| Slow-loris | `header_read_timeout` via hyper | 5s |
| Idle + slow-read | `connection_timeout` (hard lifetime cap) | 5 min |
| Connection flood | `max_connections` (semaphore) | 8192 |
| Shutdown stall | `drain_timeout` | 30s |

All of these are configurable through `ServerConfig` and passed to
`serve_with_config()`.

### What Harrow Cannot Cover

**HTTP/1 keep-alive idle timeout:** hyper's HTTP/1 builder only exposes
`keep_alive(bool)` — on or off. There is no per-idle-gap timeout. A client
that finishes a request and then sits on the connection without sending
another request is protected only by the 5-minute `connection_timeout`.
This is a hyper limitation, not a Harrow design choice.

**Write timeout (slow-read):** hyper does not expose a server-side write
timeout. A client that accepts the response at 1 byte/sec will hold the
connection until `connection_timeout` fires. Proper slow-read protection
would require wrapping the I/O stream in a custom `AsyncWrite` that
enforces minimum throughput. That is real work and not yet justified.

### Why This Matters for the Performance Story

The timer decision is not purely a performance optimization. It is a
security trade-off with measurable performance impact:

- **Timers on** (production default): safer, costs ~2x throughput at 500K+
  RPS on 48 cores due to Tokio timer-wheel contention.
- **Timers off** (behind proxy): maximum throughput, relies on the proxy
  for connection-level safety.

That is a real, defensible split. It is not "we removed safety for speed."
It is "we made safety configurable because not every deployment needs the
same defense boundary."

### Comparison

| Feature | Harrow | Axum (default) | Actix-web |
|---|---|---|---|
| Header read timeout | Yes (5s) | No | Yes (5s) |
| Connection lifetime | Yes (5min) | No | Yes |
| Max connections | Yes (semaphore) | No built-in | Yes (25K) |
| Keep-alive idle timeout | No (hyper limitation) | No | Yes (5s) |
| Write timeout | No | No | No |

Harrow ships **safer defaults** than Axum's default serving path. That is
a deliberate choice. The penalty is measurable at extreme throughput. The
defense is real.

Full details are in `docs/connection-safety.md`.

## What This Document Should Become

Every time we touch performance-critical code, this file should answer four
questions:

1. What changed?
2. What numbers moved?
3. Why do we believe that explanation?
4. Why did we reject the obvious more-complicated alternative?

If we keep doing that, the performance story stays technical instead of turning
into mythology.
