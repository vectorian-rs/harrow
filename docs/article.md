# Harrow Performance Journal

**Status:** living document  
**Last updated:** 2026-04-19

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
- After the custom HTTP/1 backend work and the Monoio results, the larger
  architectural direction became clear: Harrow should move toward
  **local-worker runtimes, explicit dispatchers, and bounded payload
  backpressure** in the nginx/ntex sense.
- On 2026-04-17, the first Harrow-vs-`ntex` Tokio benchmark after the custom
  backend rewrite looked catastrophically slow, but the first diagnosis was not
  "Tokio is too slow." It was **benchmark shape mismatch**: Harrow was still
  being measured through the wrong server entrypoint.
- After switching the benchmark server to Harrow's actual local-worker path,
  the remaining gap shrank dramatically and the evidence narrowed to the
  **write path**: Harrow was still issuing about **2.3 `sendto()` syscalls per
  request** while `ntex` was closer to **1.1-1.2**.
- On 2026-04-19, the last big Tokio gap on `/text` turned out not to be "Tokio
  cannot match `ntex`" or even "the write runner is still too naive." The hot
  path was still being sent **chunked** because `Response::new(...)` did not set
  `Content-Length` for fully buffered bodies.
- Fixing that one framing bug moved Harrow Tokio from about
  **786k-841k rps** to about **1.72M-1.73M rps** on the non-`perf` `/text`
  benchmark, essentially eliminating the old baseline gap with `ntex`.
- We also benchmarked the middleware architecture directly instead of arguing in
  the abstract. Pure Tower-style static layers are effectively allocation-flat.
  Harrow's dynamic middleware costs about **+728 bytes and +2 allocs per noop
  layer**. Axum's ergonomic `middleware::from_fn` path is much steeper.
- That still does **not** justify rewriting `Next` yet. On a realistic
  `cors + compression + session` stack, the dominant cost is session and
  compression work, not middleware-chain plumbing.

## What Harrow Is Optimizing For

Harrow is trying to be a thin, macro-free HTTP framework with explicit
application APIs and backend-owned transport/runtime control. There are three
design constraints:

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
type BoxFuture = Pin<Box<dyn Future<Output = Response>>>;

pub trait Middleware {
    fn call(&self, req: Request, next: Next) -> BoxFuture;
}

pub struct Next {
    inner: Box<dyn FnOnce(Request) -> BoxFuture>,
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

## 2026-03-31: Monoio Backend — Thread-Per-Core and io_uring Reality Check

Harrow now has a second server backend: `harrow-server-monoio`, built on
monoio's io_uring/epoll fusion runtime with a thread-per-core (TPC)
architecture. This is the first serious test of whether io_uring delivers
on its promise for HTTP server workloads.

### What We Tested

Remote throughput on `c8gn.12xlarge` (48 vCPUs, 100 Gbps networking,
placement group) using spinr (Rust TPC load generator) at 128 connections.

Payload scaling ladder: tiny text (2 bytes), 128KB, 256KB, 512KB, 1MB.

Three server configurations:
- `harrow-server-monoio` with io_uring (seccomp=unconfined)
- `harrow-server-monoio` with epoll fallback (default Docker seccomp)
- `harrow-server-tokio` (hyper + tokio work-stealing)
- `axum` (baseline)

### Results

| Payload | harrow-monoio (io_uring) | harrow-monoio (epoll) | harrow-tokio | axum |
|---|---:|---:|---:|---:|
| tiny text | 1,287,198 | 1,360,143 | 1,043,859 | 1,085,218 |
| 128KB | 136,886 | — | 136,907 | 136,912 |
| 256KB | 68,288 | — | 68,302 | 68,300 |
| 512KB | 34,142 | — | 34,148 | 34,110 |
| 1MB | 17,083 | — | 17,087 | 17,080 |

All numbers are RPS, 100% success rate, 128 connections, 20s duration.

### What the Numbers Say

**Thread-per-core wins 19-29% on small payloads.** Monoio's TPC
architecture (one event loop per core, `SO_REUSEPORT`, no work-stealing)
beats tokio's work-stealing model when the workload is CPU-bound request
processing. At tiny payloads, the per-request cost is dominated by syscall
overhead and scheduler contention — exactly where TPC eliminates waste.

**Large payloads are NIC-bound.** At 128KB and above, all four
configurations produce identical RPS. The math:
128KB * 137K RPS = ~16.6 GB/s = ~133 Gbps, which saturates the 100 Gbps
NIC (with TCP overhead). The server runtime is irrelevant when the
bottleneck is the network card.

**io_uring is slower than epoll.** This was the surprise. Monoio with
io_uring (1,287K RPS) was 5% slower than monoio with epoll fallback
(1,360K RPS), with worse p99 latency (0.29ms vs 0.25ms).

### Why io_uring Lost

Docker's default seccomp profile blocks io_uring syscalls. Monoio's
`FusionDriver` silently falls back to epoll. We added runtime detection
so the server now logs which driver is active:

```
harrow-server-monoio listening on 0.0.0.0:3090 [allocator: mimalloc, io: io_uring]
harrow-server-monoio listening on 0.0.0.0:3090 [allocator: mimalloc, io: epoll (io_uring unavailable)]
```

When we forced io_uring via `--security-opt seccomp=unconfined`, it was
measurably slower. The reason is architectural:

1. **Monoio uses single-shot operations.** Each accept, read, write is a
   separate SQE submission + CQE completion. This means each operation
   does: write SQE to ring buffer → memory barrier → `io_uring_enter`
   syscall → wait for CQE. For single operations, epoll is cheaper
   because it avoids the ring buffer overhead.

2. **io_uring wins with batching.** The real advantage comes from
   submitting multiple operations in one `io_uring_enter` call:
   multishot accept (one SQE → many connections), multishot recv
   (one SQE → continuous data), batched sends. Monoio 0.2 does not
   expose any of these.

3. **No zero-copy.** `send_zc` (zero-copy send, kernel 6.0+) could
   eliminate the userspace→kernel buffer copy for responses. Monoio
   does not use it.

In short: monoio uses io_uring as a drop-in epoll replacement. The TPC
architecture delivers the performance gain, not io_uring itself.

### OS-Level Profiling

`vmstat` and `mpstat` during the tiny-text benchmark revealed:

- **0% usr, 0% sys, 58% iowait** — the server spends almost no time in
  computation. It is entirely I/O-bound, waiting for network data.
- **41,868 context switches in 25s** (1,674/sec) — extremely low for
  1.3M RPS, confirming TPC avoids cross-thread scheduling.
- **12 cpu-migrations** — effectively zero. Threads stay pinned to cores.

The iowait breakdown by core showed ~30 of 48 cores at 100% iowait
(busy serving connections) and ~18 cores at 100% idle (no connections
assigned). This uneven distribution is because 128 connections across
48 cores means some cores get 3 connections and some get 2.

### Load Generator Comparison

We also ran the same tests with vegeta (Go, single-process):

| Load generator | Max RPS (tiny text) |
|---|---:|
| spinr (Rust, TPC, 48 workers) | 1,360,143 |
| vegeta (Go, single-process, 128 workers) | 64,439 |

Vegeta was 21x slower — it was measuring its own limits, not the server's.
Spinr's TPC architecture on the client side is what makes these benchmarks
meaningful. At 1.3M RPS, spinr actually saturates the server CPU.

### What Would Make io_uring Worth It

For HTTP servers, io_uring batching features that monoio does not yet
expose:

- **Multishot accept** (`IORING_OP_ACCEPT_MULTISHOT`, kernel 5.19+):
  one SQE → many connections. Reduces accept-loop syscalls.
- **Multishot recv** (kernel 6.0+): one SQE → continuous data on a
  connection. Eliminates per-read submissions.
- **Provided buffer rings** (kernel 5.19+): kernel fills pre-registered
  buffers autonomously, zero userspace allocation per read.
- **`send_zc`** (kernel 6.0+): zero-copy send from userspace to NIC.
- **Fixed file descriptors**: register fds once, skip kernel fd lookup
  per operation.

These would require either contributing to monoio upstream or building
a thin io_uring layer directly on the `io-uring` crate. The TPC model
stays — we just need the kernel to do more work autonomously instead of
waking userspace for each operation.

For now, the pragmatic conclusion: **use monoio with epoll fallback.**
The TPC architecture delivers the real performance gain. io_uring
batching is a future optimization when the ecosystem catches up.

### Artifacts

- `docs/perf/c8gn.12xlarge/2026-03-31T13-29-59Z/` — monoio io_uring
  text + json
- `docs/perf/c8gn.12xlarge/2026-03-31T13-39-15Z/` — tokio text + json
- `docs/perf/c8gn.12xlarge/2026-03-31T17-32-12Z/` — payload scaling
  ladder (monoio io_uring)
- `docs/perf/c8gn.12xlarge/2026-03-31T17-42-34Z/` — payload scaling
  ladder (tokio)
- `docs/perf/c8gn.12xlarge/2026-03-31T18-29-36Z/` — with OS monitors

## 2026-03-31: body_read_timeout for Tokio Backend

Added `body_read_timeout` to `harrow-server-tokio`'s `ServerConfig`.
At that point the Tokio backend was still Hyper-based, so the implementation
wrapped Hyper's `Incoming` body in a `TimeoutBody` that enforced a per-frame
read deadline. A slow body sender (e.g.,
sending 1 byte/sec within the size limit) is terminated with a 400
error after the timeout expires.

The implementation is zero-cost when disabled: `body_read_timeout`
defaults to `None`, and the raw `box_incoming` path is used with no
wrapper. The `TimeoutBody` only exists in the hot path when explicitly
configured.

This closes a gap between the two backends — monoio already had
`body_read_timeout`. Combined with `header_read_timeout` (5s default)
and `connection_timeout` (300s default), the full slowloris defense
timeline is now:

1. Client opens connection → `header_read_timeout` starts (5s)
2. Headers received → handler invoked → `body_read_timeout` starts
   (if body is read and timeout is configured)
3. Connection lifetime → `connection_timeout` (300s)

The handler never sees a slowloris — it is killed at the connection
layer before the handler runs.

## Current Thesis (Updated 2026-03-31)

1. Harrow's local routing and dispatch costs are already small.
2. The big remote throughput regression was caused by per-connection
   timers. Fixed.
3. Middleware allocation slope is real, measurable, and still much
   cheaper than Axum's ergonomic `from_fn` path.
4. **Thread-per-core (monoio) wins 19-29% on small payloads** over
   tokio's work-stealing model. Large payloads are NIC-bound.
5. **io_uring does not help without batching.** Monoio's single-shot
   usage makes io_uring slower than epoll. The TPC architecture
   is what delivers the performance gain.
6. Connection-level timeouts (header read, body read, connection
   lifetime) are security features with measurable performance cost.
   Making them optional and configurable was the right call.
7. The next optimization worth chasing is io_uring batching
   (multishot accept/recv, provided buffers, send_zc) — but only
   when the monoio ecosystem or a custom io_uring layer supports it.
8. **Spinr (TPC load generator) is critical infrastructure.** At 1.3M
   RPS it saturates the server, making these benchmarks meaningful.
   Vegeta (Go) maxed out at 64K RPS — 21x slower.

## 2026-04-01: Middleware Cleanup — Backend-Neutral at Last

Three changes made `harrow-middleware` fully runtime-agnostic:

### 1. Removed `timeout_middleware`

The timeout middleware used `tokio::time::timeout` directly. It was
redundant now that both server backends have connection-level timeouts
in `ServerConfig`:

- `header_read_timeout` (default 5s)
- `body_read_timeout` (configurable, default None for zero overhead)
- `connection_timeout` (default 300s)

Per-route handler timeouts are application code — users wrap their
handler in their runtime's timeout directly.

### 2. Removed `InMemorySessionStore` and `InMemoryBackend`

Both used `tokio::spawn` + `tokio::time::sleep` for background sweeper
tasks. Both were single-node, no-persistence implementations unsuitable
for production.

The `SessionStore` trait and `RateLimitBackend` trait remain. Users
implement them with Redis, DynamoDB, or whatever distributed store
fits their deployment. The middleware functions (`session_middleware`,
`rate_limit_middleware`) work with any implementation.

Test-only in-memory implementations live in `harrow-bench` for
benchmarks and integration tests.

### 3. Added middleware combinators

Four composable middleware helpers that work with both backends:

```rust
// Transform request before handler
app.middleware(map_request(|mut req| { req.set_ext(start_time()); req }))

// Transform response after handler
app.middleware(map_response(|resp| resp.header("x-served-by", "harrow")))

// Apply middleware conditionally
app.middleware(when(|req| req.path().starts_with("/api"), auth))

// Skip middleware for specific routes
app.middleware(unless(|req| req.path() == "/health", logging))
```

These cover the `route_layer` use case from Axum without adding
per-route middleware dispatch complexity.

### Result

`harrow-middleware` no longer depends on `tokio` or `dashmap` in
production code. Every middleware feature works identically on both
the Tokio and Monoio backends.

## Migrating from Axum

This section is a practical migration reference for developers coming
from Axum. It shows Axum patterns and their Harrow equivalents
side-by-side, is honest about what migrates easily, and is explicit
about what Harrow deliberately does not support.

### Core Difference

Axum uses **extractors** — typed parameters in handler signatures that
the framework resolves automatically:

```rust
// Axum
async fn get_user(Path(id): Path<u32>, State(db): State<DbPool>) -> Json<User> {
    let user = db.find(id).await.unwrap();
    Json(user)
}
```

Harrow uses **explicit request access** — one `Request` parameter,
you pull what you need:

```rust
// Harrow
async fn get_user(req: Request) -> Response {
    let id: u32 = req.param("id").parse().unwrap();
    let db = req.require_state::<Arc<DbPool>>().unwrap();
    let user = db.find(id).await.unwrap();
    Response::json(&user)
}
```

More verbose, but errors appear at the extraction site, not at route
registration. No trait bound puzzles, no `#[debug_handler]`.

### Handlers

**Basic handler:**

```rust
// Axum
async fn hello() -> &'static str { "hello" }
app.route("/", get(hello));

// Harrow
async fn hello(_req: Request) -> Response { Response::text("hello") }
app.get("/", hello);
```

**JSON request/response:**

```rust
// Axum
async fn create_user(Json(user): Json<CreateUser>) -> (StatusCode, Json<User>) {
    let created = save(user).await;
    (StatusCode::CREATED, Json(created))
}

// Harrow
async fn create_user(req: Request) -> Result<Response, BodyError> {
    let user: CreateUser = req.body_json().await?;
    let created = save(user).await;
    Ok(Response::json(&created).status(201))
}
```

`BodyError` implements `IntoResponse`, so `?` works directly. Parse
errors return 400, body too large returns 413.

**Application state:**

```rust
// Axum — single state struct
let app = Router::new().route("/users", get(list_users)).with_state(state);
async fn list_users(State(state): State<AppState>) -> Json<Vec<User>> { ... }

// Harrow — each type is independent
let app = App::new().state(Arc::new(db)).state(Arc::new(config));
async fn list_users(req: Request) -> Response {
    let db = req.require_state::<Arc<DbPool>>().unwrap();
    ...
}
```

### Middleware

**Simple middleware** — nearly identical:

```rust
// Axum
async fn auth(req: Request, next: Next) -> Response {
    if req.headers().get("authorization").is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}
app.layer(middleware::from_fn(auth));

// Harrow
async fn auth(req: Request, next: Next) -> Response {
    if req.header("authorization").is_none() {
        return Response::new(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}
app.middleware(auth);
```

**Request/response transforms:**

```rust
// Axum
app.layer(SetResponseHeaderLayer::new(header::SERVER, HeaderValue::from_static("harrow")));

// Harrow
app.middleware(map_response(|resp| resp.header("server", "harrow")));
```

**Conditional middleware:**

```rust
// Axum — no built-in, use custom from_fn
// Harrow — built-in combinators
app.middleware(when(|req| req.path().starts_with("/api"), auth));
app.middleware(unless(|req| req.path() == "/health", logging));
```

**Scoped middleware:**

```rust
// Axum — route_layer applies only to matched routes
let api = Router::new().route("/users", get(list)).route_layer(from_fn(auth));

// Harrow — use groups or `when`
app.group("/api", |g| { g.middleware(auth).get("/users", list) });
// or:
app.middleware(when(|req| req.path().starts_with("/api"), auth));
```

**Tower layers — no Harrow equivalent:**

```rust
// Axum
app.layer(TimeoutLayer::new(Duration::from_secs(30)));

// Harrow — connection timeouts live in ServerConfig, not middleware
serve_with_config(app, addr, shutdown, ServerConfig {
    header_read_timeout: Some(Duration::from_secs(5)),
    body_read_timeout: Some(Duration::from_secs(30)),
    ..Default::default()
});
```

Tower layers do not work with Harrow. This is deliberate — Harrow
supports both Tokio and Monoio, and Tower assumes Tokio.

### Sessions and Rate Limiting

Both follow the same pattern: Harrow provides the **trait** and
**middleware**, you provide the **backend implementation**.

```rust
// Harrow — bring your own store
impl SessionStore for RedisSessionStore { ... }
app.middleware(session_middleware(RedisSessionStore::new(pool), config));

impl RateLimitBackend for RedisRateLimit { ... }
app.middleware(rate_limit_middleware(RedisRateLimit::new(pool), key_extractor));
```

Harrow does not ship in-memory stores. Use Redis, DynamoDB, or another
distributed store for production.

### What Harrow Does Not Have

| Feature | Reason |
|---|---|
| Tower `Layer`/`Service` ecosystem | Backend independence over ecosystem size |
| Extractor-based handlers | Explicit request API by design |
| `ServiceBuilder` composition | Use `.middleware()` chaining |
| Built-in WebSocket / SSE | Not yet implemented |
| `tower-http` compatibility | No Tower dependency |

### Migration Decision

**Migrate to Harrow if** you want backend independence (Tokio + Monoio),
prefer explicit request handling, and your middleware is mostly `from_fn`
style.

**Stay with Axum if** you depend on Tower middleware crates, need
WebSocket/SSE today, or your team is invested in the extractor pattern.

The full migration reference with additional examples is in
[`docs/migration-from-axum.md`](./migration-from-axum.md).

## 2026-04-05: WebSocket Support and First crates.io Release

Harrow 0.9.0 shipped WebSocket support and landed on crates.io for the first
time. This section covers the design decisions, a subtle bug that survived
unit tests, and an ergonomics improvement that required understanding how
Rust's `?` operator actually works.

### Design: thin wrapper, feature-gated, runtime-agnostic core

WebSocket is split across two crates:

- **harrow-core** (`ws` feature): handshake validation, accept key computation,
  `Message` enum, `Utf8Bytes` type, `WsError`, close code constants, subprotocol
  negotiation. No runtime dependency — this compiles for any backend.
- **harrow-server-tokio** (`ws` feature): `WebSocket` struct wrapping
  `tokio-tungstenite`, `upgrade()` / `upgrade_with_config()`, `Stream`/`Sink`
  impls, message conversion.

The `ws` feature pulls in `tokio-tungstenite` and `sha1`/`base64`. If you
don't enable it, zero WebSocket code compiles.

### The RFC 6455 GUID bug

The first implementation had the wrong GUID constant:

```
Wrong:   258EAFA5-E914-47DA-95CA-5AB53F3B86DB
Correct: 258EAFA5-E914-47DA-95CA-C5AB0DC85B11
```

This is a magic string from RFC 6455 used in the `Sec-WebSocket-Accept`
computation. Every accept key the server produced was wrong, and
`tokio-tungstenite` (the client in our integration tests) correctly rejected
it with `SecWebSocketAcceptKeyMismatch`.

The unit tests passed because they were self-referential — `upgrade_response()`
was compared against `accept_key()`, and both used the same wrong GUID. The
test checked that the output was deterministic and had the right length, not
that it matched the RFC's expected value.

The fix was two lines: correct the GUID, and add an RFC vector test that pins
the literal expected output (`s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`) for the RFC's
sample key. The `upgrade_response` test was also changed to assert the literal
value instead of calling `accept_key()` again.

**Lesson:** when implementing a protocol with fixed test vectors in the spec,
always include at least one test that asserts the spec's exact expected output.
Self-referential tests (compute A, compute B from the same code, check A == B)
cannot catch errors in shared constants or algorithms.

### serve_connection_with_upgrades — always on

The initial implementation gated `serve_connection_with_upgrades` behind
`#[cfg(feature = "ws")]`, falling back to `serve_connection` without it. This
created a subtle failure mode: if someone forgot the feature flag but wrote a
WebSocket handler, `OnUpgrade` would not be present in the request extensions,
and the upgrade function would silently return a 101 response without actually
upgrading the connection. The client would hang.

We checked how axum handles this. It always uses `serve_connection_with_upgrades`
unconditionally. Reading the hyper-util source confirmed there is zero per-poll
overhead for non-upgrade connections — the only difference materializes when a
101 response is returned, at which point you need the upgrade machinery anyway.

The fix: remove the `cfg` gate, always use `serve_connection_with_upgrades`,
and change the `OnUpgrade` extraction from a silent `if let Some(...)` to an
explicit `Err(WsError::NotUpgradable)`.

### Zero-copy Utf8Bytes

`Message::Text` originally held `String`. On every received text message,
tungstenite's `Utf8Bytes` (which wraps `bytes::Bytes`, already validated as
UTF-8) was converted to `String` via `.to_string()` — allocating and copying.

We created `harrow_core::ws::Utf8Bytes`, a thin wrapper around `bytes::Bytes`
that guarantees UTF-8 validity. It derefs to `&str`, implements `PartialEq`
with `str`/`String`/`&str`, and has `From<String>` and `From<&str>`.

The receive path is now zero-copy:

```
tungstenite::Utf8Bytes → bytes::Bytes (Into, zero-copy)
                       → harrow::Utf8Bytes (unsafe from_bytes_unchecked, zero-copy)
```

The `unsafe` is sound because tungstenite already validated the UTF-8 before
constructing its `Utf8Bytes`. We do not re-validate.

Why not use tungstenite's `Utf8Bytes` directly? harrow-core is
runtime-agnostic. It should not depend on tungstenite. The harrow `Utf8Bytes`
has the same layout (`Bytes` wrapper) but lives in the core crate.

### From<Error> for Response — making ? actually work

After adding `IntoResponse` for `MissingStateError`, `MissingExtError`,
`BodyError`, and `WsError`, we documented that handlers could "use `?`
directly." This was wrong.

Rust's `?` operator uses `From`, not arbitrary conversion traits. Given a
handler returning `Result<Response, Response>` and `require_state()` returning
`Result<&T, MissingStateError>`, the `?` needs `From<MissingStateError> for
Response`. Having `IntoResponse for MissingStateError` is irrelevant — `?`
does not know about `IntoResponse`.

The fix was adding targeted `From` impls that delegate to `IntoResponse`:

```rust
impl From<MissingStateError> for Response {
    fn from(err: MissingStateError) -> Self {
        err.into_response()
    }
}
```

We considered a blanket `impl<E: IntoResponse> From<E> for Response` but it
is impossible in stable Rust — it conflicts with the standard library's
reflexive `impl<T> From<T> for T` (since `Response` itself implements
`IntoResponse`). The targeted impls for each error type are the correct
approach.

With this, the handler pattern is clean:

```rust
async fn handle(req: Request) -> Response {
    let db = req.require_state::<Arc<DbPool>>()?.clone();
    let body: CreateUser = req.body_json().await?;
    // ...
}
```

No `.unwrap()`, no `.map_err()`. Missing state returns 500, bad body returns
400/413, and the process never panics on a request path.

### What shipped in 0.9.x

- **0.9.0**: WebSocket support, `Utf8Bytes`, `bytes::Bytes` message types,
  `Stream`/`Sink` impls, `WsConfig` builder, subprotocol negotiation, auto
  close response, close code constants. First crates.io publish.
- **0.9.1**: `IntoResponse` for `MissingStateError` and `MissingExtError`.
- **0.9.2**: `From<Error> for Response` for all error types (the `?` fix).
- **0.9.3**: `From<ProblemDetail> for Response`, consistent import style,
  removed unused test imports, added README to crate packages.
- **0.9.4**: MIT license file, CHANGELOG.md, README version updates.

## 2026-04-15: Chosen Runtime Direction — Local Workers

The Monoio numbers were already pointing in one direction: the gain was coming
from local ownership and the thread-per-core execution model, not from
`io_uring` by itself.

The custom HTTP/1 backend work made that direction actionable for Tokio too.
Once Harrow owned the connection loop, request-body pump, and response write
path, the remaining question was what runtime shape best fits that transport
design.

Reading `ntex` made the answer obvious. Its Tokio path still uses `LocalSet`,
`spawn_local`, the same HTTP/1 dispatcher, and the same local payload queue.
Tokio is not locked to a generic work-stealing server model. The useful pattern
is:

- keep connection ownership local to one worker
- keep parser state, payload state, and response state local to that worker
- stop reading request-body bytes when the handler-side buffer is full
- resume only when the handler drains enough buffered data

That is the part of the nginx/ntex design space Harrow should copy.

So the direction is now explicit:

- Tokio should move toward per-worker `current_thread` runtimes plus
  `LocalSet` and local tasks
- Monoio should keep the same local-worker structure and finish the
  byte-bounded request-body queue
- Meguri should not be forced into this shape until it has streaming request
  bodies

This also changes how to read the earlier benchmark sections. The old Tokio
numbers are still useful, but they are now baseline material for the
pre-custom-backend, work-stealing implementation rather than the end state
Harrow is aiming for.

The gain we are chasing is broader than "Tokio timers were expensive." The real
target is local ownership, explicit transport state machines, and bounded
payload backpressure.

## 2026-04-17: Slow -> Reason -> Solution on Tokio

This was the first serious benchmark pass after the custom HTTP/1 rewrite and
the move toward local-worker runtimes. It is worth documenting in the
slow -> reason -> solution format because the first number looked disastrous and
the wrong explanation would have sent the project in the wrong direction.

### Slow

The first remote Harrow-vs-`ntex` Tokio baseline on `c8gn.12xlarge` looked
awful:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | `168,648.67 rps` | `0.94 ms` |
| `ntex` Tokio | `1,775,624.97 rps` | `0.15 ms` |

If those numbers had been real, the conclusion would have been that Harrow's
Tokio backend was still fundamentally wrong.

It was not.

### Reason

The first reason was not transport overhead. It was a benchmark bug.

Harrow's benchmark server was still being launched through the single-runtime
Tokio path, while the server backend had already moved to the local-worker
runtime shape. In other words, we were benchmarking the wrong Harrow server.

Switching the benchmark binary to `serve_multi_worker(...)` immediately changed
the picture:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | about `801k-828k rps` | about `0.26-0.27 ms` |
| `ntex` Tokio | about `1.78M-1.81M rps` | about `0.15 ms` |

That did not close the gap, but it killed the false story. The huge slowdown
was not "custom Harrow transport is broken." It was "the benchmark is not
using the same runtime shape Harrow was designed for."

Once the runtime path was corrected, the next diagnosis came from `perf stat`,
`strace -c`, and flamegraphs:

- scheduler-level signals were no longer dramatically different
- request-body backpressure was not the dominant remaining cost
- the write path still was

The cleanest signal was syscall shape. Harrow was still at roughly
`2.33 sendto()/request`, while `ntex` was much closer to `1.1-1.2`.

That made the remaining problem narrower: not routing, not parsing, not the
basic local-worker design, but **too many write escapes on the Tokio response
path**.

### Solution

The solution was not one patch. It was a sequence of increasingly specific
write-path changes.

1. **Move the benchmark to the real Harrow Tokio runtime path.**
   This was the biggest correction because it turned a misleading result into a
   useful one.

2. **Add a connection-local write buffer and coalesce small fixed responses.**
   This removed the obvious "head write, body write, temp chunk vec" shape from
   the common path.

3. **Add a dedicated local write runner.**
   This matched the `ntex` structure better, but by itself it did not
   materially change throughput.

4. **Encode response heads directly into the connection-local buffer.**
   This removed an allocate-then-copy step for the header path.

The important part is what actually moved:

- perf-mode Harrow improved from roughly `22k rps` to roughly `29k rps`
- Harrow `sendto()` count dropped from about `3.57 sendto()/request` to about
  `2.33 sendto()/request`

That means the response writer work was real and worth doing. It also means we
can now say something precise about the remaining gap.

### What Did Not Work

Removing `PreparedResponse.head: Vec<u8>` and writing headers directly into the
connection-local buffer was a good cleanup, but it did **not** materially move
the benchmark. That is useful because it rules out a tempting but too-small
explanation.

The remaining Harrow-vs-`ntex` Tokio gap is still mostly about the **overall
write/flush/syscall pattern**, not the cost of one header allocation.

### What This Means

The evidence chain is now much better than it was at the start of the day:

- the old gigantic gap was mostly a benchmark-entrypoint mistake
- the local-worker/nginx-`ntex` direction was the right architectural move
- the remaining gap is real, but much narrower
- the remaining target is the write side, especially flush policy and syscall
  shape

That is exactly the kind of outcome this document should preserve. We want the
next optimization pass to start from the true remaining problem, not from the
ghost of a bad benchmark setup.

## 2026-04-19: The Hot Path Was Still Chunked

This is the correction that matters most. The 2026-04-17 write-path work was
not wasted, but it was also not the full answer.

### Slow

After the runtime-shape fix and the first response-writer cleanup pass, Harrow
Tokio was still well behind `ntex` on the remote `/text` baseline:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | about `786k-841k rps` | about `0.27 ms` |
| `ntex` Tokio | about `1.74M-1.79M rps` | about `0.16 ms` |

The profiled runs had also improved, but Harrow was still behind there too:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | about `27.8k-28.9k rps` | about `7.8-10.6 ms` |
| `ntex` Tokio | about `40.4k-41.4k rps` | about `3.6-3.7 ms` |

At that point the working theory was still "Tokio output/syscall shape."

### Reason

The key realization was embarrassingly simple: `text-c128` means **128
connections**, not "a 128-byte body." The hot benchmark route is just `/text`,
and in Harrow that path returns a fully buffered `Response::text(...)`.

The bug was in Harrow's core response construction:

- `Response::new(status, body)` boxed a full body
- but it did **not** set `Content-Length`
- shared HTTP/1 response planning therefore treated those bodies as **chunked**
  instead of fixed-length

So the microbenchmark was not mostly measuring the residual cost of Harrow's
write runner. It was measuring the cost of emitting chunked framing on the
hot path where `ntex` was sending a tiny fixed-length response.

That also explained why the earlier syscall diagnosis looked so stark. Harrow's
remaining `sendto()` excess was real, but a big part of it came from choosing
the wrong wire format for a fully known body.

### Solution

The fix was to make `Response::new(...)` set `Content-Length` automatically for
fully buffered bodies.

That immediately changed the `/text` hot path from chunked to fixed-length
across backends that use Harrow's shared response type and shared HTTP/1
planning logic.

The numbers moved dramatically on the very next rerun:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | `1,731,499 rps` | `0.16 ms` |
| `ntex` Tokio | `1,819,009 rps` | `0.15 ms` |

Confirmation run:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | `1,722,245 rps` | `0.16 ms` |
| `ntex` Tokio | `1,802,656 rps` | `0.16 ms` |

The profiled runs moved too:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | `34,618 rps` | `6.92 ms` |
| `ntex` Tokio | `40,452 rps` | `3.68 ms` |

Confirmation run:

| Framework | Throughput | p99 |
|---|---:|---:|
| Harrow Tokio | `34,253 rps` | `8.02 ms` |
| `ntex` Tokio | `41,365 rps` | `3.62 ms` |

Most importantly, the syscall picture converged too. On the new profiled run,
Harrow was at about **1.17 `sendto()` per request**, which is effectively the
same range as `ntex` on this workload.

### What This Means

This is the stronger lesson than "write buffering matters":

- the nginx/`ntex` local-worker architecture work was still the right
  direction
- the response-writer cleanup was still useful
- but the last dramatic gap was dominated by a **wire-format correctness bug**
  on the hottest path

That is exactly why performance work has to stay tied to protocol correctness.
If you send the wrong HTTP framing, you can spend a long time "optimizing the
writer" when the real problem is that the writer is doing extra work because
the response was described incorrectly in the first place.

## What This Document Should Become

Every time we touch performance-critical code, this file should answer four
questions:

1. What changed?
2. What numbers moved?
3. Why do we believe that explanation?
4. Why did we reject the obvious more-complicated alternative?

If we keep doing that, the performance story stays technical instead of turning
into mythology.
