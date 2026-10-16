# Verification Strategy

Harrow is a stateless, request-response HTTP framework. It has no replicas, no
consensus protocol, no on-disk durability, and no distributed state machine.
This means the techniques that shine in distributed databases (broad DST with
shadow-state oracles, broad TLA+ specifications, Stateright model checking,
Maelstrom linearizability tests) are largely inapplicable here. Applying them
too broadly would create maintenance burden without catching real bugs.

What harrow *does* have:

- A path-matching engine with three segment types (literal, param, glob).
- A recursive middleware dispatch chain with index arithmetic.
- A GCRA rate limiter with a lock-free CAS loop.
- Compression round-trip logic.
- Query-string parsing with percent-decoding.
- Body-size enforcement at two layers (Content-Length pre-check and frame accumulation).
- A finite shared connection/control state machine in the HTTP/1 backends.

These are the surfaces where bugs hide. The right tools are **property-based
testing** (proptest) for algebraic invariants, **fuzzing** (cargo-fuzz) for
parsing robustness, and **Kani** for bounded verification of small, pure
functions. A *small* deterministic lifecycle harness and a *small* formal model
also make sense where there is a genuine finite state machine to explore
(today: shared/backend-agnostic HTTP/1 connection lifecycle, not the framework
as a whole). Everything else is unit tests.

---

## Technique Applicability Matrix

| Technique | Applicable? | Where |
|-----------|-------------|-------|
| **proptest** | Yes | Path matching, routing, GCRA arithmetic, compression round-trip, middleware ordering |
| **cargo-fuzz** | Yes | Query-string parsing, path matching, Accept-Encoding parsing, HTTP/1 codec decoding |
| **Kani** | Maybe | GCRA single-step correctness, `ns_to_secs_ceil`, path segment classification |
| **DST** | Narrowly | Shared HTTP/1 lifecycle harness with replayable seeded scripts |
| **TLA+ / Quint** | Narrowly | Shared HTTP/1 connection/control lifecycle only (today: TLA+ only) |
| **Stateright** | No | No state-space explosion to explore |
| **Maelstrom** | No | Not a distributed system |

---

## Per-Crate Plan

### harrow-core

The most correctness-critical crate. Three areas warrant investment.

#### 1. Path matching (path.rs) — proptest + fuzz

**Properties to verify (proptest):**

- `match_path` and `matches` agree: if `match_path` returns `Some`, then
  `matches` returns `true`, and vice versa.
- Captured params round-trip: for pattern `/a/:x/:y` and path `/a/foo/bar`,
  `match.get("x") == Some("foo")` and `match.get("y") == Some("bar")`.
- Glob captures remainder: for pattern `/files/*rest` and any path starting
  with `/files/`, the glob value equals the suffix after `/files/`.
- No false positives on literals: a random path that does not start with the
  literal prefix never matches.
- Trailing-slash symmetry: `/users` and `/users/` both match pattern `/users`
  (current behavior — verify it stays consistent).

**Fuzz targets:**

- `fuzz_path_match(pattern: &[u8], path: &[u8])` — parse pattern, match path,
  no panics. Catches edge cases in segment splitting (empty segments, lone
  colons, lone asterisks, non-UTF-8 via lossy conversion).

#### 2. Middleware dispatch chain (dispatch.rs) — proptest

**Properties to verify:**

- Given N global middleware and M route middleware, the handler is called
  exactly once.
- Middleware execute in order: global[0], global[1], ..., route[0], ..., handler.
  Verify by having each middleware append its index to a shared `Vec<usize>`.
- Short-circuit: if middleware K returns early (does not call `next.run`), then
  middleware K+1 through N and the handler are never invoked.
- The fast path (no middleware) produces the same response as the slow path
  with an empty middleware vec.

These are integration-level proptests that construct an `App`, register N
identity middleware, and dispatch a synthetic request.

#### 3. Route table (route.rs) — proptest

**Properties to verify:**

- Any route registered with `app.get(pattern, handler)` is found by
  `match_route_idx(&GET, path)` for every path that the pattern matches.
- A path that matches a pattern but with the wrong method returns `None`
  from `match_route_idx` but `true` from `any_route_matches_path`.
- `allowed_methods` returns exactly the set of methods registered for a path.
- HEAD→GET fallback: a GET-only route is found when queried with HEAD.

#### 4. Query parsing (request.rs) — fuzz

**Fuzz targets:**

- `fuzz_query_pairs(raw_query: &[u8])` — exercise `query_pairs()` and
  `query_param()` with adversarial query strings. Verify no panics, result
  size <= MAX_QUERY_PAIRS (100), and that `query_param(k)` returns a value
  that is present in `query_pairs()`.

---

### harrow-middleware

#### 1. Rate limiter GCRA (rate_limit.rs) — proptest + Kani

**Properties to verify (proptest):**

- Burst property: for a backend with `burst=B`, the first B requests in a
  zero-elapsed-time window are all allowed.
- Rate property: over a long simulated time window, the number of allowed
  requests converges to `rate * elapsed_seconds` (within burst tolerance).
- Independence: two different keys never interfere with each other.
- Monotonicity: `remaining` is non-increasing for consecutive allowed requests
  without time advancement.

**Kani (bounded model checking):**

- `gcra_check` single-step: for all `(old_tat, now, t_ns, tau_ns)` in bounded
  ranges, verify:
  - If allowed, `new_tat > old_tat || old_tat == 0`.
  - `remaining <= burst`.
  - `retry_after_ns == 0` when allowed.
  - `retry_after_ns > 0` when denied.
- `ns_to_secs_ceil`: for all `n: u64` up to 2^32, verify
  `ns_to_secs_ceil(n) * 1_000_000_000 >= n` and
  `(ns_to_secs_ceil(n) - 1) * 1_000_000_000 < n` (unless n == 0).

#### 2. Compression (compression.rs) — proptest + fuzz

**Properties to verify:**

- Round-trip: for any byte sequence `data` where `len >= MIN_COMPRESS_SIZE`,
  `decompress(compress(data)) == data` for gzip and deflate.
- Encoding negotiation: `pick_encoding` prefers br > gzip > deflate > identity.
  Generate random Accept-Encoding strings and verify preference order holds.
- No double-compress: a response with `content-encoding` already set is
  returned unchanged.

**Fuzz targets:**

- `fuzz_accept_encoding(raw_header: &[u8])` — exercise the public
  `compression_middleware` with adversarial `Accept-Encoding` values. Verify no
  panics, only supported `content-encoding` values are emitted, and compressed
  bodies round-trip back to the original payload.

#### 3. CORS (cors.rs) — unit tests sufficient

CORS is header manipulation with well-defined RFC behavior. The existing unit
tests cover preflight, simple requests, and credential handling. No additional
verification technique adds value here.

#### 4. Timeout, catch-panic, body-limit, request-id — unit tests sufficient

These are thin wrappers around Tokio/std primitives. The existing tests are
adequate.

---

### harrow-serde

Delegates entirely to `serde_json` and `rmp_serde`. No harrow-specific logic
to verify beyond the existing round-trip tests.

---

### harrow-server / shared HTTP/1 lifecycle

The runtime-specific read/write loops still belong in the backend crates, but
the lifecycle invariants around:

- request head/body/dispatch/write sequencing
- early response before request EOF
- pipelined keep-alive reuse
- timeout closure vs pending CQEs
- shutdown/drain terminal behavior

are now covered by a shared deterministic lifecycle harness in
`harrow-server::h1_lifecycle` plus the focused TLA+ model in
`specs/tla/ConnectionLifecycle.tla`.

That remains intentionally narrow. We are not trying to simulate Tokio,
Monoio, or Meguri end-to-end. We are modeling the shared protocol lifecycle
boundary that all of those backends rely on.

---

## First-Class Commands

- `cargo test -p harrow-core`
- `cargo test -p harrow-middleware --features rate-limit,compression`
- `cargo check --manifest-path harrow-core/fuzz/Cargo.toml`
- `cargo check --manifest-path harrow-middleware/fuzz/Cargo.toml`
- `cargo check --manifest-path harrow-codec-h1/fuzz/Cargo.toml`
- `mise run fuzz:check`
- `mise run fuzz:core:path`
- `mise run fuzz:core:query`
- `mise run fuzz:middleware:accept-encoding`
- `mise run fuzz:codec:parse-request`
- `mise run fuzz:codec:chunked-decode`
- `mise run fuzz:codec:response-head`
- `mise run fuzz:codec:payload-decoder`
- `mise run fuzz:codec:roundtrip`

---

### harrow-o11y

Configuration struct with no logic. Nothing to verify.

---

### harrow (umbrella)

Re-exports and the `AppO11yExt` trait. The only interesting behavior is that
the `TelemetryGuard` stays alive for the application's lifetime, which is
ensured by storing it in `TypeMap` state. Verified by existing integration
tests.

---

## Implementation Priority

Ordered by expected bug-finding value per hour of effort:

| Priority | Target | Technique | Effort | Why |
|----------|--------|-----------|--------|-----|
| 1 | `path.rs` match_path/matches agreement | proptest | Low | Two implementations of the same logic — disagreement is a real bug class |
| 2 | `rate_limit.rs` GCRA burst/rate properties | proptest | Low | Arithmetic-heavy, easy to get off-by-one |
| 3 | `request.rs` query parsing | cargo-fuzz | Low | Untrusted input, percent-decoding, edge cases |
| 4 | `dispatch.rs` middleware ordering | proptest | Medium | Integration-level, needs App scaffolding |
| 5 | `compression.rs` round-trip | proptest | Low | Straightforward but catches encoding bugs |
| 6 | `rate_limit.rs` gcra_check | Kani | Medium | Bounded model checking for the CAS loop |
| 7 | `path.rs` adversarial patterns | cargo-fuzz | Low | Defense against malformed route definitions |
| 8 | `route.rs` 404/405 correctness | proptest | Medium | Needs route table construction in test |

---

## What We Explicitly Skip

- **DST / shadow-state oracles**: Harrow processes independent HTTP requests
  with no cross-request state (except the rate limiter, which is covered by
  proptest). There is no command sequence whose interleaving could produce a
  consistency violation. A shadow oracle for "given this request, what should
  the response be" is just the handler itself.

- **TLA+ / Quint for the whole framework**: No. The middleware chain is a
  linear pipeline, not a protocol worth modeling end-to-end. The exception is
  the backend connection/control lifecycle, which is finite enough to justify a
  focused TLA+ state-machine model. A Quint mirror would currently duplicate
  that maintenance burden without adding coverage.

- **Maelstrom / Jepsen**: Not a distributed system.

- **Kani for the middleware chain**: The chain is recursive with dynamic
  dispatch (`dyn Middleware`). Kani cannot reason about trait objects or async
  in a useful way here. Proptest covers this better.

If harrow later grows more protocol-shaped state (sessions, distributed rate
limiting, request queuing), revisit DST and broader TLA+/Quint modeling at
that point.
