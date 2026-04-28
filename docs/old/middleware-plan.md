# Middleware Plan

**Status:** draft plan, 2026-03-30

This document is the implementation plan that follows from the analysis in
[`docs/middleware.md`](./middleware.md).

`middleware.md` is the research and comparison document.
This file is the decision and execution document.

## Executive Summary

Harrow should keep its own middleware model and improve it, rather than
re-architecting the framework around Axum/Tower.

The key decision is:

- keep `Middleware` + `Next` as Harrow's primary middleware abstraction
- make `harrow-middleware` backend-neutral
- move connection/runtime policy into the server backends
- move background maintenance loops out of portable middleware
- improve Harrow-native middleware ergonomics where users actually feel pain:
  scope control and small compositional helpers

This plan is driven by one constraint that matters more now than earlier:

- Harrow is no longer only a Tokio/Hyper framework
- Harrow now has a real Monoio/io_uring backend
- portable middleware cannot continue to assume Tokio without making Monoio a
  second-class story

## Decisions

### 1. Keep Harrow-native middleware as the core model

Harrow will keep:

- `Middleware`
- `Next`
- `async fn(Request, Next) -> Response`

as the primary middleware authoring model.

We are **not** changing the framework core to:

- `tower::Layer`
- `tower::Service`
- `poll_ready`
- a Tower-first composition model

Reason:

- the current model is simple and fits Harrow's explicit request API
- it is easier to author than Tower middleware
- it works conceptually for both Tokio and Monoio
- making Tower the core would move Harrow toward Tokio/Tower assumptions at
  exactly the moment when we need backend neutrality

### 2. Treat portable request middleware and runtime policy as different things

We will explicitly separate four categories:

1. portable request middleware
2. matched-route policy middleware
3. connection/runtime policy
4. background maintenance helpers

This is the most important architectural cleanup.

### 3. Do not hide Tokio inside portable middleware

Anything in `harrow-middleware` that is supposed to be portable across Harrow
backends should not directly rely on:

- `tokio::spawn`
- `tokio::time::sleep`
- `tokio::time::timeout`

If a feature requires those semantics, it must either:

- move into a backend-specific surface
- or expose runtime-agnostic core logic plus explicit backend-specific helpers

### 4. Improve Harrow ergonomics instead of importing Tower semantics wholesale

The missing pieces for users are mostly:

- finer middleware scope
- small compositional helpers
- clearer migration patterns from Axum

Those should be solved with Harrow-native APIs, not by making Tower the center
of the framework.

## Middleware Classification

### A. Portable request middleware

Definition:

- request/response transforms that do not require runtime timers, spawning,
  buffering infrastructure, or connection-level control

Examples:

- `request-id`
- `cors`
- `o11y`
- `catch-panic`
- `body-limit`
- auth that reads headers/cookies and sets request extensions

Target state:

- stays in `harrow-middleware`
- backend-neutral
- works the same for Tokio and Monoio

### B. Matched-route policy middleware

Definition:

- request middleware where the main concern is *where* it applies

Examples:

- auth on `/api/*`
- admin policy on `/api/admin/*`
- versioned behavior on one subtree
- route-scoped header policies

Target state:

- still Harrow-native middleware
- better scope controls than today's global-or-group-only model

### C. Connection/runtime policy

Definition:

- behavior tied to connection I/O, deadlines, cancellation, accept loops, or
  backpressure at the server boundary

Examples:

- header read timeout
- body read timeout
- connection lifetime timeout
- max connections
- graceful drain

Target state:

- owned by `harrow-server-tokio`
- owned by `harrow-server-monoio`
- not expressed as general request middleware

### D. Background maintenance helpers

Definition:

- utilities that require periodic cleanup or background work but are not part
  of the request pipeline itself

Examples:

- session sweeper
- rate-limit state cleanup

Target state:

- no implicit Tokio background loops inside portable middleware
- explicit cleanup methods or backend-specific launch helpers

## Current Problems To Fix

### 1. `harrow-middleware` is not backend-neutral today

Current examples:

- `InMemorySessionStore::start_sweeper` uses `tokio::spawn` and
  `tokio::time::sleep`
- `InMemoryBackend::start_sweeper` uses `tokio::spawn` and
  `tokio::time::sleep`

Impact:

- `harrow-middleware` is conceptually portable but not actually portable
- Monoio support is weakened because important middleware carries Tokio
  assumptions
- users cannot reason clearly about what is runtime-neutral and what is not

### 2. Middleware scope is too coarse

Today Harrow gives users:

- `App::middleware(...)`
- `Group::middleware(...)`

This is usable, but it is weaker than the Axum patterns many users expect:

- global wrapping
- matched-route-only wrapping
- route-local wrapping

Impact:

- users must overuse groups to get policy scope right
- auth-like use cases are more awkward than they should be
- migration from Axum `route_layer(...)` is less direct than necessary

### 3. Harrow lacks small compositional helpers

Today, small transforms usually require writing a full middleware.

Examples:

- add one response header
- inject one request extension
- apply one middleware only when a predicate matches

Impact:

- the common case is still acceptable
- the "small ergonomic convenience" case is weaker than Axum/Tower
- users who are used to `map_request`, `map_response`, and `ServiceBuilder`
  feel more friction than needed

## Planned API Direction

This section describes the planned direction, not final signatures.

### 1. Add finer scope controls

The first major ergonomic improvement should be scope.

Target capabilities:

- middleware that runs only on matched routes
- middleware attached to one route
- subtree middleware without forcing large route restructuring

Candidate shapes:

```rust
let app = App::new()
    .get("/health", health)
    .route("/api/me", get(me).middleware(auth))
    .route("/api/admin", get(admin).middleware(admin_only));
```

or:

```rust
let app = App::new()
    .matched_middleware(auth)
    .get("/api/me", me);
```

The exact API is open, but the capability is the important part.

Priority:

- high

Reason:

- this is the biggest usability gap for Axum users after backend-neutral cleanup

### 2. Add small Harrow-native combinators

The initial combinator set should stay small.

Recommended first set:

- `map_request(...)`
- `map_response(...)`
- `when(...)`
- `unless(...)`

#### `map_request(...)`

Intended use:

- inject request extensions
- stamp request metadata
- perform cheap request-side mutation

Example:

```rust
let app = App::new()
    .middleware(map_request(|req| {
        req.set_ext(RequestStart(std::time::Instant::now()));
    }));
```

Important constraint:

- this is not a substitute for pre-routing URI/path normalization
- Harrow middleware currently runs after route matching

This means `map_request` should target request enrichment, not route rewriting.

#### `map_response(...)`

Intended use:

- add headers
- normalize response metadata
- perform cheap response-side transforms

Example:

```rust
let app = App::new()
    .middleware(map_response(|resp| resp.header("x-served-by", "harrow")));
```

#### `when(...)` / `unless(...)`

Intended use:

- apply a middleware only when a predicate matches

Example:

```rust
let app = App::new()
    .middleware(when(
        |req| req.path().starts_with("/api"),
        auth_middleware,
    ));
```

Priority:

- medium-high

Reason:

- these solve common ergonomic pain without importing Tower complexity

### 3. Do not prioritize a Tower-style builder as the first step

We should **not** start by building:

- `ServiceBuilder`-equivalent chaining
- readiness models
- Tower-style generic service combinators

Reason:

- these would pull us toward the wrong abstraction too early
- Harrow can get most of the user-facing benefit from better scope and a few
  small combinators

## Middleware-Specific Plan

### `request-id`

Plan:

- keep in `harrow-middleware`
- no architectural change expected

### `cors`

Plan:

- keep in `harrow-middleware`
- no architectural change expected

### `o11y`

Plan:

- keep in `harrow-middleware`
- ensure it stays backend-neutral

### `catch-panic`

Plan:

- keep in `harrow-middleware`
- no architectural change expected

### `body-limit`

Plan:

- keep in `harrow-middleware`
- continue to treat it as request middleware

### `compression`

Plan:

- keep in `harrow-middleware`
- verify backend-neutral assumptions as the implementation evolves

### `timeout`

**Resolved:** removed from `harrow-middleware`. Connection-level timeouts
(header_read_timeout, body_read_timeout, connection_timeout) live in
`ServerConfig` on both backends. Per-route handler timeouts are application
code — users wrap their handler in `tokio::time::timeout` directly.

### `rate-limit`

Plan:

- keep the GCRA request path portable
- remove implicit Tokio sweeper semantics from the portable surface
- expose explicit cleanup or backend-specific maintenance helpers

### `session`

Plan:

- keep session request behavior portable
- move background expiry maintenance out of the portable middleware layer

## Axum Migration Guidance

This plan is not trying to eliminate all migration cost from Axum.

The goal is:

- make simple and common middleware migration easy
- avoid pretending the full Tower ecosystem is portable across Harrow backends

### Easy migrations

- `from_fn(...)` style middleware
- request extension patterns
- request-state lookup patterns
- first-party middleware categories with Harrow equivalents

### Medium-friction migrations

- `route_layer(...)` semantics
- `ServiceBuilder` ordering expectations
- small request/response transform helpers

The scope and combinator improvements in this plan are meant to reduce exactly
this category of friction.

### High-friction migrations

- reusable Tower `Layer` / `Service` crates
- operational Tower layers such as retry, buffer, load-shed, and timeout

These should remain explicitly outside Harrow's core middleware promise.

## Implementation Phases

### Phase 1: Classification and backend-neutral cleanup

Scope:

- classify all shipped middleware into the four categories
- audit `harrow-middleware` for direct Tokio coupling
- ~~decide `timeout` ownership~~ (done: removed from harrow-middleware)
- remove or isolate Tokio sweepers from `rate-limit` and `session`

Deliverables:

- updated docs
- cleaned-up feature boundaries
- portable middleware that is actually portable

Acceptance criteria:

- portable middleware modules do not directly depend on Tokio runtime APIs
- backend-specific behavior is explicit in public docs and API placement

### Phase 2: Scope ergonomics

Scope:

- add one new scope capability beyond today's global/group model

Target:

- route-local middleware or matched-route-only middleware

Acceptance criteria:

- a user can express matched-route auth without overusing groups
- documentation clearly explains `404` vs `401` behavior under the new model

### Phase 3: Combinator ergonomics

Scope:

- add the first small combinator set

Recommended set:

- `map_request`
- `map_response`
- `when`
- `unless`

Acceptance criteria:

- common small transforms no longer require bespoke middleware functions
- combinators remain backend-neutral

## Non-Goals

This plan does **not** propose:

- replacing Harrow middleware with Tower as the core abstraction
- adding Tower compatibility as a planned middleware direction
- adding generic service readiness/backpressure APIs to ordinary middleware
- pretending request middleware should own connection-level runtime policy

## Open Questions

1. Should route-local middleware be added at the `Route` type, builder methods,
   or a new matched-route scope API?
2. ~~Should `timeout` be removed from `harrow-middleware`?~~ (done: removed)
3. Do we need a small pre-routing phase for things like path normalization, or
   should that stay out of scope?
## Recommended Next Step

The next implementation step should be **Phase 1**, not API sugar.

Reason:

- backend-neutral cleanup is the architectural prerequisite
- if we improve ergonomics before fixing Tokio coupling, we risk making the
  wrong API more attractive

After that, implement **scope ergonomics** before combinators.

That order gives the highest user impact with the lowest architectural risk.
