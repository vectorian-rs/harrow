# Harrow Project Review

**Date:** 2026-03-09
**Reviewer:** Claude Opus 4.6
**Scope:** PRD, explicit-extractors doc, open GitHub issues, full codebase audit

---

## State of the Project

### Issues vs. Reality

| Issue | Status | Action |
|---|---|---|
| #7 Radix tree | **Done** — matchit just landed | Close it |
| #4 and #12 | **Duplicates** — both are graceful shutdown drain | Close #4, keep #12 |
| #11 Explicit extractors | Design agreed, not started | Next up |
| #3 Body size limits | Not started | Critical for production |
| #2 Custom error responses | Not started | Enables #11 cleanly |
| #5 Body streaming | Not started | Can wait |
| #6 CORS | Not started | Can wait |
| #13 Latency histograms | Not started | Can wait (tracing spans work) |

### Are we moving in the right direction?

**Yes, with one sequencing correction.** Assessment per dimension below.

---

## Correctness

**What's right:**
- Zero panics on the hot path (dispatch, routing, middleware chain)
- Zero unsafe code in the entire codebase
- matchit gives correct O(path_length) routing with proper priority (literal > param > glob)
- `IntoResponse` trait exists and handles `Result<Response, E>` — the foundation for #11 is there
- Integration tests cover routing, middleware ordering, groups, o11y, and timeouts — real TCP, not mocks

**What's wrong:**

1. **`accept()` error kills the server** (`harrow-server-tokio/src/lib.rs:34`, `lib.rs:78`). A transient `EMFILE` (too many open FDs) propagates via `?` and terminates the entire listener loop. This is a **correctness bug**, not a feature gap — the server should not die from a transient OS error. Fix: log and `continue`.

2. **No `Allow` header on 405** — RFC 9110 §15.5.6 requires it. The information exists in the `MethodMap` but isn't surfaced. This is a spec violation.

3. **No HEAD handling** — RFC 9110 §9.3.2. A HEAD to a GET route returns 405 instead of the GET response with body stripped.

4. **`query_pairs()` doesn't percent-decode** — `hello%20world` stays encoded. This will silently produce wrong results for any real-world query string with spaces, unicode, or special characters.

5. **PRD §8 claims graceful shutdown drains in-flight requests** — it doesn't. The code `break`s immediately. The doc is lying about the behavior.

---

## Performance

**What's right:**
- Hot path allocates <1 KB per request (typical)
- matchit routing is O(path_length), method dispatch is Vec linear scan (1-3 entries)
- Fast path skips middleware chain entirely when no middleware registered
- Response::json() writes directly into BytesMut — no intermediate Vec/String
- Zero-copy where possible: `Request` borrows from hyper, `param()` returns `&str`

**What's fine but worth watching:**
- ~100-170 bytes per middleware layer per request (Box closure + Box::pin future) — acceptable for 3-5 layers
- `Arc::clone` per middleware layer in `run_middleware_chain` — atomic refcount, not a real allocation
- `query_pairs()` allocates a HashMap on every call — if called multiple times per request, this is wasteful. A `query_param(name)` that scans without allocating would be better for the common case (1-2 lookups)

**What's actually problematic:**
- Path param extraction allocates `String` per captured param (`params_to_path_match` in route.rs). With borrowed lifetimes this could be zero-alloc, but that would require `PathMatch` to borrow from the matchit `Match`, which has lifetime implications throughout the stack. Not worth doing now — the 80-140ns cost is noise vs network RTT.

**Benchmarks gap:** The performance.md numbers are from the old trie implementation. They should be re-run post-matchit to establish a new baseline. The matchit comparison benchmark (`matchit_compare.rs`) exists but the main `performance.md` doc hasn't been updated.

---

## Ease of Use / No Magic

**What's right:**
- The explicit extractor philosophy is correct and well-argued
- Handler signatures are plain async functions — no trait bounds, no generics, no macros
- Middleware is a plain async function — `async fn(Request, Next) -> Response`
- Route groups with `.group()` are clean and composable
- `App` builder reads naturally: `.get()`, `.post()`, `.middleware()`, `.state()`

**The gap is small but important — #11:**

The PRD promises `Result<Response, AppError>` handlers but the code requires `Response`. The fix is the `wrap()` change (2 lines). After that, users can write:

```rust
// This works today
async fn handler(req: Request) -> Response { ... }

// This works after the wrap() change
async fn handler(req: Request) -> Result<Response, MyError> { ... }

// Both work — no magic, no migration pressure
```

This is the single highest-impact ergonomics improvement. It's also the smallest change.

**`req.state()` panicking is a real usability problem.** Not because panics are wrong during development (they give a clear error), but because in production a panic in a handler kills that connection with no HTTP response — the client sees a TCP RST, not a 500. The `try_state()` API exists but users default to `state()` because it's shorter. Changing `state()` to return `Result` is the right call, but it's a breaking change that should happen alongside #11.

**`req.param()` returning `""` for missing params is correct.** The router guarantees params exist if the route matched. Returning `&str` with `.parse::<u64>()?` is the right pattern — the `?` is on parse, not on param lookup.

**What's missing but shouldn't be built yet:**
- `req.json::<T>()` (alias for `body_json`) — nice naming, low priority
- `req.extract::<T>()` — this is in the explicit-extractors doc but should be removed. It reintroduces the extractor-trait pattern that harrow explicitly rejects.

---

## Security

**Critical:**

1. **No body size limit** (#3) — `body_bytes()` and `body_json()` will buffer a multi-GB body into memory. This is the #1 security issue. Wrap with `http_body_util::Limited`. Add a default max (e.g., 2 MB) configurable via `App::max_body_size()`.

2. **Unbounded connection concurrency** — No `Semaphore`, no `max_connections`. Every `accept()` spawns a `tokio::spawn` with no limit. Slow-loris or connection flood → OOM / FD exhaustion.

3. **`accept()` error kills the server** — Already listed under correctness, but it's also a security issue: an attacker who can trigger `EMFILE` (by opening many connections) permanently kills the listener.

**Medium:**

4. **`query_pairs()` unbounded allocation** — A query string with millions of `&`-separated pairs creates a million-entry HashMap. No limit on pair count or key/value size. Less critical than body limits because query strings are bounded by URL length in practice (most proxies enforce ~8 KB), but if harrow is used without a reverse proxy, this is exploitable.

5. **No connection-level timeouts** — No read timeout (waiting for headers), no idle timeout. A client that opens a connection and sends nothing holds it forever. `TimeoutMiddleware` only covers handler execution time, not the connection phase.

**Low:**

6. **O11yConfig uses `&'static str`** — Not a security issue per se, but `otlp_traces_endpoint: Option<&'static str>` means users can't set endpoints from environment variables without `Box::leak`. This encourages hardcoding secrets/endpoints.

---

## Recommended Sequencing

### Batch 1 — Low-effort correctness/security fixes (no API changes)

| Change | Effort | Issue |
|---|---|---|
| `accept()` error: log and continue | 5 min | No issue filed |
| `Allow` header on 405 | 30 min | No issue filed |
| Close #7 (done) and #4 (dup of #12) | 2 min | #7, #4 |

### Batch 2 — The explicit extractor upgrade (#11, #2, #3)

These three are coupled — do them together:

| Change | Effort | Issue |
|---|---|---|
| `wrap()` accepts `IntoResponse` | 10 min | #11 |
| Add `harrow_core::error::Error` enum (MissingState, BodyTooLarge, BodyRead, JsonParse) with `IntoResponse` | 1 hr | #11 |
| `state()` returns `Result<&T, Error>` | 30 min | #11 |
| Body size limit middleware/wrapper | 1 hr | #3 |
| Custom error response handlers on App | 1 hr | #2 |
| Update tests and examples | 1 hr | — |

### Batch 3 — Production hardening

| Change | Effort | Issue |
|---|---|---|
| Graceful shutdown with connection drain | 2 hr | #12 |
| Connection concurrency limit (Semaphore) | 30 min | No issue filed |
| HEAD auto-handling | 30 min | No issue filed |
| `query_pairs()` percent-decoding + bounds | 30 min | No issue filed |

### Batch 4 — Features (can happen in any order)

| Change | Issue |
|---|---|
| CORS middleware | #6 |
| Body streaming | #5 |
| Latency histograms | #13 |

### Defer or drop

- `req.extract::<T>()` — remove from the explicit-extractors doc. It's axum's `FromRequest` with a different name. Against harrow's philosophy.
- TLS — the feature stub is fine. Most deployments terminate TLS at the load balancer.

### Docs to update after batch 2

- PRD §4.1, §4.2, §5, §6, §8, §12, §13
- explicit-extractors.md code example (make it compile)
- performance.md (re-run benchmarks post-matchit)

---

## PRD-Specific Issues

### Sections that are wrong or outdated

| Section | Problem |
|---|---|
| §4.1 `App` struct | Shows `o11y: O11yConfig` as a field — doesn't exist. O11y is wired via extension trait. |
| §4.2 Handler signature | Shows `Result<Response, AppError>` — code requires `Response`. |
| §5 Path matching | Says "linear scan in v0.1" — replaced with matchit. |
| §6 State injection | Shows `req.state::<T>()?` — currently panics, not `Result`. |
| §8 Graceful shutdown | Claims connection drain — not implemented. |
| §10 Performance targets | "< 1 us added latency" — no benchmark measures this specific metric. |
| §12 Milestones | v0.1 lists graceful shutdown (not done), v0.2 lists route groups (already done). |
| §13 Open questions | Q2 (state panic vs Option) and Q3 (linear matching) are answered by the codebase. |

### explicit-extractors.md issues

1. **Code example doesn't compile** — `req.state()` returns `&T` not `Result`, `req.extract()` doesn't exist, `req.json()` doesn't exist (`body_json` does).
2. **`req.extract::<T>()`** — Undocumented, reintroduces extractor-trait pattern. Should be removed.
3. **"Zero cloning" claim is overstated** — True for params/headers, not for body (body_json consumes self).
