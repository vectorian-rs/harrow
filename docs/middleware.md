# Middleware Architecture, Axum Comparison, and Runtime Strategy

**Status:** revised 2026-03-30

This document answers a practical question:

> What does "middleware" mean to users coming from Axum, where does Harrow
> differ today, and how should Harrow evolve now that it supports both Tokio
> and Monoio?

The short version is:

- Harrow should keep a **Harrow-native middleware model** as its primary API.
- Harrow should **not** make Tower `Layer` / `Service` the core abstraction for
  the framework.
- `harrow-middleware` should become **backend-neutral**. Direct `tokio::*`
  usage inside portable middleware is a design bug.
- Connection and runtime policy should live in the **server backends**, not in
  generic request middleware.
- If Tokio/Tower ecosystem interop is desirable, it should be an **explicit
  Tokio-only adapter layer**, not the foundation of Harrow.

This is the design that preserves Harrow's explicit request API, keeps the
Monoio story credible, and still gives Axum users a clear migration path.

## Why This Document Exists

Harrow's current middleware story is good for small, explicit request wrappers:

- `async fn(Request, Next) -> Response`
- global middleware via `App::middleware(...)`
- matched-route middleware via `Group::middleware(...)`
- app state via `Request::require_state(...)`
- per-request data via `Request::set_ext(...)`

That is enough for logging, auth, request IDs, CORS, panic recovery, and many
other first-party concerns.

The pain starts when users want the broader things that "middleware" means in
real Axum projects:

- stack and reorder lots of middleware with a reusable composer
- scope middleware precisely to routes or handlers
- reuse `tower-http` and third-party `Layer` crates
- write middleware once and publish it for multiple Tower-based frameworks
- use runtime-heavy layers such as timeout, buffer, retry, load-shed, and
  concurrency control

Those needs are real, but they are not all the same problem. Harrow should
separate them instead of trying to solve all of them with one abstraction.

## What Axum Users Mean By "Middleware"

Official Axum middleware docs describe several distinct usage modes:

1. **Apply middleware at different scopes**
   - `Router::layer(...)` wraps previously added routes.
   - `route_layer(...)` applies only to matched routes.
   - `Handler::layer(...)` / `MethodRouter::layer(...)` let users attach policy
     close to one handler.

2. **Compose middleware with `tower::ServiceBuilder`**
   - Axum explicitly recommends `ServiceBuilder` when stacking multiple layers.
   - It gives users a predictable ordering model and access to Tower
     combinators.

3. **Write ad hoc middleware with `axum::middleware::from_fn`**
   - This covers the common "quick auth/logging/header" case.
   - Axum positions it as the easy, Axum-specific path.

4. **Write reusable middleware as `Layer` + `Service`**
   - This is the "publishable crate" path.
   - It is more work because it introduces `Layer`, `Service`, `poll_ready`,
     associated error types, and readiness/backpressure concerns.

5. **Use request extensions and state**
   - Middleware often inserts data into request extensions.
   - Handlers retrieve it later via extractors.

6. **Use Tower combinators for small transforms**
   - `map_request`
   - `map_response`
   - `then`
   - `and_then`

That combination is why Axum middleware feels powerful in practice: not because
one API is magical, but because users can move up and down the abstraction
stack depending on what they need.

## What Harrow Has Today

Harrow's request middleware model is intentionally smaller:

- `Middleware` is a trait that takes `Request` and `Next` and returns a boxed
  response future.
- `Next` is a one-shot continuation to the rest of the chain.
- Global middleware is stored on `App`.
- Matched-route middleware is attached through groups.
- Middleware can pass data to handlers using request extensions.

This has real advantages:

- small mental model
- easy to author
- no extractor-specific middleware API
- no `poll_ready`
- no Tower readiness/backpressure surface in application code
- one obvious request flow: middleware -> handler -> response

It also has clear limits:

- no Tower ecosystem reuse
- no `ServiceBuilder`-style composition surface
- no generic request/response combinators
- no per-handler middleware scope
- no route-local equivalent to Axum's `route_layer(...)` beyond grouping
- reusable middleware crates must target Harrow specifically

## Why Harrow Feels Fine For Simple Middleware

For straightforward middleware, Harrow is already ergonomic:

- request ID
- auth checks
- add/remove headers
- trace/span setup
- panic recovery
- route-aware metrics

These are mechanically close to Axum's `from_fn` model:

```rust
async fn auth(req: Request, next: Next) -> Response {
    if req.header("authorization").is_none() {
        return Response::new(http::StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}
```

That is not the pain point.

## Where The Pain Actually Appears

The pain appears when users want more sophisticated **abstractions**,
**adapters**, or **combinators**.

### 1. Reusable middleware crates

In the Axum/Tower world, if a team writes a reusable auth layer or a generic
request shaping layer, the natural public API is `Layer` + `Service`.

That buys them:

- reuse across Axum, Hyper service stacks, and sometimes Tonic
- consistent composition with `ServiceBuilder`
- compatibility with other Tower layers

In Harrow, a middleware crate written against `Middleware` and `Next` is
Harrow-specific by design. That is fine for first-party or app-local code, but
it is a meaningful ecosystem difference.

### 2. Fine-grained scope control

Axum users rely heavily on the distinction between:

- global `Router::layer(...)`
- matched-route-only `route_layer(...)`
- handler-local layers

That distinction matters most for auth and rejection behavior. A common Axum
pattern is:

- unauthenticated matched route -> `401`
- non-existent route -> `404`

Users often choose `route_layer(...)` specifically to avoid turning every `404`
into a `401`.

Harrow today has:

- global middleware that also runs on unmatched requests
- group middleware that only runs for matched routes inside that group

That means Harrow can express some of this behavior with groups, but not all of
it as precisely as Axum users expect. The missing piece is **finer matched-route
scope**, not the ability to run middleware at all.

### 3. Small ad hoc composition helpers

Axum users do not always author a full middleware type. They often use:

- `map_request`
- `map_response`
- `then`
- `and_then`

for tiny transforms.

Harrow does not have an equivalent composition toolbox. The user can still
write a middleware, but the ergonomics are worse for "small plumbing"
operations.

### 4. Runtime-heavy middleware

This is the most important issue for Harrow.

Several Harrow middleware features currently depend directly on Tokio:

- `rate-limit`
- `session`

That is visible in `harrow-middleware/Cargo.toml`, where those features pull in
`tokio`.

The reason is concrete, not theoretical:

- `InMemorySessionStore::start_sweeper(...)` uses `tokio::spawn(...)` and
  `tokio::time::sleep(...)`
- `InMemoryBackend::start_sweeper(...)` in rate limiting does the same

This is where the current model stops being just a DX issue and becomes an
architecture issue. A "portable middleware crate" cannot directly depend on
Tokio if Harrow also wants first-class Monoio support.

## Why Not Make Tower The Core Abstraction?

It is tempting to say "Axum solves this with Tower, so Harrow should too."

That would be the wrong move for Harrow.

### Tower solves a different problem

Tower is excellent when the goal is:

- reusable middleware/services across many Tower-based stacks
- explicit readiness/backpressure
- generic service composition
- a large shared middleware ecosystem

But those benefits come with real costs:

- `Layer` and `Service` are more complex than `async fn(Request, Next) -> Response`
- custom middleware often needs manual future types or boxed futures
- users must reason about `poll_ready`, readiness, and error types
- handler-local code becomes less obviously "just request in, response out"

Axum's own docs call this out: there are easy Axum-specific paths
(`from_fn`) and lower-level Tower paths for reusable crates.

### Tower does not solve the Tokio vs Monoio problem

Even if Harrow adopted Tower in core, that still would not make the Tokio
middleware ecosystem backend-neutral.

Common Tower/Tower-HTTP middleware categories rely on Tokio-oriented runtime
features:

- timers
- spawned background tasks
- buffering
- readiness coordination
- concurrency limit implementations

That is not a criticism of Tower. It is just a mismatch with Harrow's goal of
supporting both:

- Tokio/custom HTTP/1
- Monoio/io_uring

If Harrow made Tower the foundation, it would likely end up in one of two bad
states:

1. **Tokio-first in practice**
   - Tower middleware works well on Tokio.
   - Monoio becomes second-class or permanently partial.

2. **Lowest-common-denominator core**
   - Harrow restricts itself to a subset of Tower semantics that fit both
     backends.
   - Users still do not get the real Tower ecosystem payoff.

Neither outcome is attractive.

### Tower would blur Harrow's design identity

Harrow's value proposition is:

- macro-free request handling
- explicit request parsing
- explicit state access
- direct control over routing and middleware
- a high-performance backend path that is not chained to the Hyper/Tower stack

Making `Layer` / `Service` the center of the framework would pull Harrow toward
"another Tower host" rather than a deliberate alternative.

## The Right Separation For Harrow

Harrow should separate middleware concerns into four buckets.

### 1. Portable request middleware

These are middleware that operate only on:

- request headers
- request extensions
- request state
- response headers
- response status/body

They do **not** require:

- runtime timers
- runtime spawning
- background maintenance loops
- connection lifecycle access

Examples:

- request ID
- CORS
- observability span decoration
- panic recovery
- auth that inspects headers/cookies and inserts claims into request extensions
- response compression, if implemented without backend-specific assumptions

These belong in `harrow-middleware` and should remain backend-neutral.

### 2. Matched-route policy

These are still request middleware, but their scope matters.

Examples:

- auth for `/api/*`
- admin-only gates
- version-specific behavior
- per-group rate policies

These should use Harrow's own routing model, not Tower's.

Harrow already has group middleware. The missing ergonomic improvement is more
precise scoping, not a Tower rewrite.

### 3. Connection and runtime policy

These are not really request middleware and should not be modeled as such.

Examples:

- header read timeout
- body read timeout
- connection lifetime timeout
- max connections
- graceful drain timeout
- per-connection backpressure behavior

These belong in:

- `harrow-server-tokio`
- `harrow-server-monoio`

and they already mostly do.

This is especially important for Monoio because cancellation safety and timeout
behavior are backend-sensitive. A generic request-timeout middleware is the
wrong place to hide runtime-specific cancellation semantics.

### 4. Background maintenance helpers

These are utilities that need timers or spawned tasks but are not part of the
request chain itself.

Examples:

- session expiry sweepers
- rate-limit state cleanup
- background refresh tasks for caches

These should **not** live inside backend-neutral middleware as implicit Tokio
tasks.

The right shape is either:

- explicit maintenance methods such as `prune_expired()` / `prune_stale()`, or
- backend-specific helper crates or modules that spawn these loops deliberately

This is the key cleanup Harrow should make now.

## Recommended Harrow Architecture

### Primary recommendation

Keep a Harrow-native middleware model and make it runtime-neutral.

Concretely:

1. Keep `Middleware` + `Next` as the primary public abstraction.
2. Keep Harrow-specific request/response flow as the default authoring model.
3. Do **not** adopt Tower `Layer` / `Service` as the framework core.
4. Remove direct `tokio::*` dependencies from portable middleware modules.
5. Move connection/time/cancellation policy into the server backends.
6. Move background sweepers out of backend-neutral middleware.

### Secondary recommendation

Add Harrow-native ergonomics where Axum users actually feel the gap.

High-value additions would be:

- per-route middleware scope finer than groups
- request/response combinators such as:
  - `map_request`
  - `map_response`
  - `around`
- clearer middleware ordering documentation
- explicit migration recipes from Axum patterns

These close the biggest user-facing gap without importing the entire Tower
semantic model.

### Optional future recommendation

If migration pressure is high, add a **Tokio-only compatibility surface** for
Tower layers.

That would be explicitly:

- optional
- backend-specific
- not part of Harrow's core abstraction

This can ease migration for Tokio users without forcing Monoio to pretend it
supports the whole Tower ecosystem.

## What This Means For Existing Harrow Middleware

### Middleware that should stay portable

These should be backend-neutral:

- `request-id`
- `cors`
- `o11y`
- `catch-panic`
- `body-limit`
- most auth middleware built on headers/cookies/extensions

### Middleware that needs redesign

These need cleanup because they currently use Tokio directly:

#### `timeout`

Current state:

- implemented as request middleware
- uses `tokio::time::timeout(...)`

Design recommendation:

- do not treat request timeout as portable middleware
- keep connection read/body/lifetime timeouts in server config
- if Harrow wants handler deadlines, make them backend-specific or expose an
  explicit runtime-aware helper instead of hiding it in portable middleware

#### `rate-limit`

Current state:

- core GCRA algorithm is runtime-neutral
- optional sweeper uses Tokio spawning and sleeping

Design recommendation:

- keep the request-path rate-limit logic portable
- move sweeper orchestration out of the portable crate
- prefer explicit cleanup helpers or backend-specific helpers

#### `session`

Current state:

- request/session semantics are mostly runtime-neutral
- optional in-memory sweeper uses Tokio spawning and sleeping

Design recommendation:

- keep session request behavior portable
- move expiration task orchestration out of the portable crate
- do not make session support implicitly "Tokio middleware"

## Axum Migration: What Users Will Actually Experience

The migration cost depends almost entirely on how much the application depends
on the Tower ecosystem rather than just on Axum's easy middleware path.

### Low-friction migrations

These port fairly directly:

| Axum pattern | Typical use | Harrow migration experience |
|---|---|---|
| `middleware::from_fn(...)` | auth, logging, header mutation | Easy. Rewrite as `async fn(Request, Next) -> Response`. |
| `middleware::from_fn_with_state(...)` | auth/config lookup | Easy to medium. Use `Request::require_state(...)`, captured `Arc`, or both. |
| request extensions | pass user/session/context to handlers | Easy. Use `Request::set_ext(...)`, `ext(...)`, `require_ext(...)`. |
| CORS / request ID / panic recovery / basic tracing | common first-party middleware | Easy if Harrow ships an equivalent. |

### Medium-friction migrations

These work, but the shape is different:

| Axum pattern | Why users like it | Harrow migration impact |
|---|---|---|
| `Router::layer(...)` | global wrapping | Similar via `App::middleware(...)`, but remember Harrow global middleware also runs on unmatched requests. |
| `route_layer(...)` | matched-route-only auth; preserve `404` vs `401` | Partial today. Use groups where possible; Harrow needs finer per-route scope for parity. |
| `ServiceBuilder` | explicit ordering for multiple layers | No direct equivalent. Users chain `.middleware(...)` calls or group middleware; Harrow should add lighter-weight combinators. |
| `map_request` / `map_response` / `then` / `and_then` | tiny ad hoc transforms | No direct equivalent. Users write explicit middleware today. |

### High-friction migrations

These are where users feel the biggest difference:

| Axum pattern | Why users rely on it | Harrow migration impact |
|---|---|---|
| reusable `Layer` / `Service` crates | shared middleware across Axum/Tonic/Hyper | Hard. Must be rewritten for Harrow or gated behind a Tokio-only adapter surface. |
| Tower operational layers (`buffer`, `retry`, `load_shed`, `concurrency_limit`, `timeout`) | production service composition | Mixed. Some belong in Harrow server config, some are intentionally out of scope, some need backend-specific implementations. |
| extractor-driven middleware composition | reuse extractors as policy | Harder. Harrow is explicit-request-first, so users rewrite extractor logic as request helpers or middleware helpers. |

### Practical migration rule

The farther an Axum application leans into the Tower ecosystem, the more
expensive migration to Harrow becomes.

The farther it leans into simple `from_fn` middleware and explicit request
logic, the easier migration becomes.

That is the real migration boundary.

## The Tradeoff: Harrow vs Axum

### Where Axum is stronger

- mature `Layer` / `Service` ecosystem
- fine-grained scope controls
- `ServiceBuilder` composition
- publishable generic middleware model
- easier reuse of existing ecosystem crates

### Where Harrow should stay different

- simpler request middleware authoring model
- explicit `Request` API instead of extractor-driven middleware
- no `poll_ready` / readiness model in common application middleware
- backend independence at the framework API layer
- freedom to support Monoio without pretending Tokio-based middleware is portable

### Core tradeoff

Axum gives users a larger shared middleware ecosystem because it accepts the
Tower service model and the complexity that comes with it.

Harrow should accept a smaller ecosystem surface in exchange for:

- a clearer request model
- less conceptual overhead for first-party middleware
- a backend-neutral core
- a serious Monoio path

That tradeoff is only worth it if Harrow is disciplined about not leaking Tokio
assumptions back into its "portable" middleware story.

## Recommended Near-Term Cleanup

1. **Reclassify middleware by concern**
   - portable request middleware
   - matched-route policy
   - connection/runtime policy
   - background maintenance helpers

2. **Audit `harrow-middleware` for Tokio usage**
   - remove direct `tokio::*` from portable features
   - move sweeper helpers out of the portable crate

3. **Document route scope more clearly**
   - global middleware runs for unmatched requests
   - group middleware runs only for matched routes in that group

4. **Add Harrow-native combinators**
   - enough to cover the common `map_request` / `map_response` style cases
   - without importing Tower as a dependency model

5. **Add finer matched-route scope**
   - this is the highest-value migration improvement for Axum users after
     portable middleware cleanup

6. **Treat Tower interop as optional and backend-specific**
   - only if real user demand justifies it
   - never as the core abstraction

## Current Middleware Surface

Harrow's shipped middleware modules are currently:

- `request-id`
- `cors`
- `o11y`
- `catch-panic`
- `body-limit`
- `compression`
- `rate-limit`
- `session`
- `security-headers`

This list is useful, but the more important question is whether each item is:

- backend-neutral request middleware
- backend-specific runtime policy
- or a portable feature that currently contains runtime-specific implementation

That classification should drive the next cleanup.

## References

### External

- [Axum middleware docs](https://docs.rs/axum/latest/axum/middleware/index.html)
- [Tower `ServiceBuilder`](https://docs.rs/tower/latest/tower/builder/struct.ServiceBuilder.html)
- [Tower `Service`](https://docs.rs/tower/latest/tower/trait.Service.html)
- [Tower HTTP timeout middleware](https://docs.rs/tower-http/latest/tower_http/timeout/struct.TimeoutLayer.html)

### Harrow source and design docs

- `harrow-core/src/middleware.rs`
- `harrow-core/src/dispatch.rs`
- `harrow-core/src/request.rs`
- `harrow-core/src/route.rs`
- `harrow-middleware/Cargo.toml`
- `harrow-middleware/src/timeout.rs`
- `harrow-middleware/src/rate_limit.rs`
- `harrow-middleware/src/session.rs`
- `harrow-middleware/src/security_headers.rs`
- `docs/security.md`
- `docs/connection-safety.md`
- `docs/old/strategy-tpc.md`
- `docs/old/auth-middleware.md`
