# HTTP/1 Dispatcher Design

**Status:** Draft
**Date:** 2026-04-15
**Scope:** `feat/custom-http-backend`

---

## 1. Purpose

This document describes the target HTTP/1 dispatcher architecture for Harrow.

The main goal is to move Harrow's server backends closer to the split that makes
`ntex` fast and maintainable:

- keep byte codec code separate from connection state management
- keep connection state management separate from app/router dispatch
- make Tokio and Monoio share the same protocol structure
- avoid forcing Meguri into abstractions that do not fit its io_uring model yet

This is a protocol-boundary design, not a proposal to copy `ntex`'s full
`Service`/`Control`/`Filter` stack.

It is also only half of the architecture decision. The runtime side of that
decision now lives in
[`docs/old/strategy-local-workers.md`](./old/strategy-local-workers.md): Harrow is
moving toward a local-worker model in the nginx/ntex sense, and this dispatcher
split is the protocol shape that makes that model practical.

---

## 2. Problem

Today Harrow's HTTP/1 transport logic is split awkwardly:

- `harrow-codec-h1` owns header parsing, payload decoding, and response-head
  serialization.
- `harrow-core` owns route matching, middleware execution, handler invocation,
  and request/response wrapper ergonomics.
- `harrow-server-tokio` still mixes too many protocol concerns into one file:
  listener lifecycle, per-connection state, request-body policy, app dispatch,
  and response writing.

The largest remaining correctness/performance gap is request-body handling:

- Harrow Tokio currently reads the full request body before it constructs the
  `Request` and calls `harrow_core::dispatch(...)`.
- `ntex` constructs the request as soon as the head is parsed, gives the
  handler a live payload stream, and keeps pumping body chunks while the
  service future runs.

That difference matters for:

- large uploads
- streaming request bodies
- memory footprint under concurrency
- request/response overlap behavior
- parity with `ntex` and the backend structure we want for Monoio

---

## 3. Goals

| Priority | Goal |
|----------|------|
| P0 | Introduce a real HTTP/1 connection dispatcher layer between codec and app dispatch. |
| P0 | Allow request bodies to stream into handlers without full pre-buffering. |
| P0 | Make request-body backpressure explicit and bounded at the dispatcher layer. |
| P0 | Keep `harrow-core` as the application dispatcher, not the transport dispatcher. |
| P0 | Make Tokio and Monoio converge on the same HTTP/1 structure and invariants. |
| P1 | Keep the dispatcher friendly to local-worker runtimes and worker-local state. |
| P1 | Extract shared protocol logic only after Tokio and Monoio have proven-compatible shapes. |
| P1 | Preserve the simple `async fn handler(req: Request) -> Response` model. |
| P2 | Leave room for future HTTP/2 reintroduction without polluting the HTTP/1 path. |

### Non-Goals

- Recreating `ntex`'s entire service/control abstraction model.
- Immediate unification of Meguri with the Tokio/Monoio dispatcher shape.
- Public extractor redesign in the same change.
- Premature generic traits that hide backend-specific runtime costs.

---

## 4. Current Layering

Current practical split:

```text
harrow-codec-h1
  parse request head
  decode request payload
  write response head
  encode chunked body frames

harrow-server-tokio
  accept loop
  connection lifetime
  request parse loop
  request body collection
  dispatch(shared, request)
  response serialization and writes

harrow-core
  route matching
  middleware chain
  fallback handling
  handler invocation
  Request/Response wrappers
```

This means the transport-level "dispatcher" exists, but only implicitly inside
`harrow-server-tokio/src/lib.rs`.

---

## 5. Target Layering

Target split:

```text
harrow-codec-h1
  pure bytes-in/bytes-out helpers

harrow-server-h1   (eventual shared layer)
  HTTP/1 connection dispatcher
  request head state
  request payload pump
  response write state
  protocol decisions

harrow-server-tokio / harrow-server-monoio
  listener setup
  worker model
  task spawning
  runtime timers
  concrete socket IO

harrow-core
  app dispatch only
```

The important boundary is:

- `harrow-core::dispatch(...)` remains the application dispatcher
- the new HTTP/1 dispatcher becomes the transport dispatcher

---

## 6. Tokio-First Module Split

Before extracting any shared crate, `harrow-server-tokio` should be reshaped
internally like this:

```text
harrow-server-tokio/src/
  lib.rs
  server.rs
  connection.rs
  h1/
    mod.rs
    dispatcher.rs
    request_head.rs
    request_body.rs
    response.rs
    error.rs
```

### `lib.rs`

Public surface only:

- `serve`
- `serve_with_shutdown`
- `serve_with_config`
- `serve_multi_worker`

### `server.rs`

Owns:

- listener creation
- worker loops
- shutdown signal propagation
- drain timeout handling
- per-connection task tracking

### `connection.rs`

Owns:

- socket bootstrap (`set_nodelay`, peer metadata)
- selection of the HTTP/1 path
- future protocol selection if HTTP/2 returns later

### `h1/request_head.rs`

Owns:

- request-head parsing
- keep-alive determination
- `Expect: 100-continue`
- content-length prechecks
- initial body framing metadata

### `h1/request_body.rs`

Owns:

- request-body stream construction
- channel or adapter used to expose a live `Body`
- backpressure policy for request-body feeding
- byte-bounded payload buffering for local-worker backends or an equivalent
  backend-specific bounded queue
- request-body timeout and limit handling during streaming

### `h1/response.rs`

Owns:

- body-permitted rules (`HEAD`, `1xx`, `204`, `304`)
- fixed-length vs chunked write path
- direct frame-by-frame response streaming
- connection-close decisions tied to response state

### `h1/error.rs`

Owns:

- malformed-request wire responses
- timeout responses
- payload-too-large responses

### `h1/dispatcher.rs`

Owns the actual HTTP/1 connection state machine.

---

## 7. Dispatcher Shape

The dispatcher should model protocol state explicitly rather than burying it in
a large loop.

Suggested states:

```text
ReadHead
Dispatch
PumpRequestBody
WriteResponse
NextRequestOrClose
Stop
```

In practice the request-body and response phases overlap, so the implementation
will likely need an in-flight struct rather than a pure enum-only machine:

```rust
struct InFlightRequest {
    parsed: ParsedHead,
    request_body: RequestBodyPump,
    response_fut: Pin<Box<dyn Future<Output = http::Response<ResponseBody>> + Send>>,
}
```

The important behavior is:

1. parse the request head
2. build `http::Request<Body>` immediately
3. call `harrow_core::dispatch(...)` immediately
4. continue feeding request body chunks while the response future runs
5. stream the response back out
6. decide whether the connection can stay alive

That is the key `ntex` idea we want to adopt.

---

## 8. Request Streaming Design

### Current Behavior

Current Tokio behavior:

1. parse request head
2. read and collect full body into `Bytes`
3. wrap body as `Full<Bytes>`
4. call `dispatch(shared, request)`

This loses the main advantage of a dispatcher-driven protocol loop.

### Target Behavior

Target Tokio/Monoio behavior:

1. parse request head
2. create a bounded stream-backed `Body`
3. construct `http::Request<Body>`
4. start `dispatch(shared, request)`
5. pump decoded `PayloadItem::Chunk(Bytes)` into the body stream
6. close the stream on `Eof`
7. propagate decoder/time-limit/size-limit failures into the request body

### First-Phase Policy

The first implementation should keep the policy simple:

- if the handler responds before request-body EOF, set `Connection: close`
- do not attempt to pipeline another request on that socket
- do not support request-body duplex semantics beyond normal HTTP/1 request
  consumption

This is the safest first step and is enough to remove full pre-buffering.

### Backpressure Target

The longer-term target should match `ntex` semantically:

- request-body buffering is bounded by bytes, not just message count
- the dispatcher stops reading more body bytes when that buffer is full
- reading resumes when the handler drains enough buffered data

The exact queue implementation can differ by backend:

- Tokio should move toward a local-worker/runtime-friendly queue
- Monoio should use a worker-local byte-bounded queue
- Meguri should adopt the same contract only after it has streaming request
  bodies

The important part is the protocol contract, not forcing one identical queue
type across all backends.

### Harrow Core API Impact

No immediate public API break is required.

`harrow_core::request::Body` is already boxed and stream-capable. Handlers that
call:

- `req.body_bytes().await`
- `req.body_json().await`
- `req.body_msgpack().await`

can keep working unchanged. They will collect from a live stream instead of a
pre-built `Full<Bytes>`.

Useful follow-up APIs:

- `into_body(self) -> Body`
- `body_mut(&mut self) -> &mut Body`
- `body_stream(self)` or `body_frames(self)`

Those should be additive.

---

## 9. Runtime Boundary

We should adopt `ntex`'s split in spirit, not in exact generic shape.

The likely mistake would be extracting a large shared IO trait too early.

We should also avoid treating this as only a protocol refactor. The dispatcher
shape is intended to support Harrow's local-worker runtime direction:

- connection state stays on one worker
- payload queues stay local to that worker
- hot-path protocol coordination avoids cross-worker scheduling where possible

Recommended sequence:

1. refactor Tokio into the module layout above
2. refactor Monoio into the same conceptual layout
3. compare the two
4. only then extract shared pieces into `harrow-server-h1` or `harrow-server`

What can be shared early:

- request/response protocol rules
- state transitions
- body-permitted decisions
- head serialization helpers
- error mapping rules

What should stay backend-specific until proven otherwise:

- concrete read/write loops
- timer primitives
- task spawning
- local buffering strategies
- cancellation mechanics

Meguri should continue to share codec and protocol rules, but it does not need
to conform to the same dispatcher extraction schedule as Tokio and Monoio.

---

## 10. Relationship to `ntex`

The `ntex` architecture provides the right benchmark for separation of concerns:

- request head parse is separate from app dispatch
- request payload is attached to the request before the handler runs
- response bodies stream frame-by-frame
- the connection dispatcher owns protocol state explicitly

What Harrow should copy:

- protocol-boundary split
- explicit connection dispatcher
- streaming request/response body handling
- bounded request-body backpressure
- thin runtime integration layer

What Harrow should not copy blindly:

- the full control-service abstraction stack
- backend-agnostic traits before the code shapes converge
- abstractions driven by `ntex` internals rather than Harrow's public API

---

## 11. Migration Plan

### Phase 1: Tokio Internal Refactor

- split `harrow-server-tokio/src/lib.rs`
- introduce `h1/dispatcher.rs`
- keep behavior equivalent where possible

### Phase 2: Streaming Request Bodies in Tokio

- replace pre-buffered request body construction
- add raw-wire tests for large/streamed uploads
- define early-response-close policy

### Phase 3: Monoio Alignment

- align Monoio H1 shape with Tokio's dispatcher split
- keep Monoio-specific IO optimizations local

### Phase 4: Shared Extraction

- extract protocol logic into a shared H1 dispatcher layer
- keep backend crates thin

### Phase 5: Meguri Evaluation

- reuse only the parts that fit Meguri's completion model
- do not force full convergence if it harms the io_uring design

---

## 12. Open Questions

1. What bounded queue size should the request-body stream use?
2. Should early handler completion always imply `Connection: close`, or can we
   safely drain unread body bytes in some cases?
3. Do we want body-stream accessors in `harrow-core` before or after the Tokio
   request-streaming change lands?
4. Once Tokio and Monoio converge, does the shared code belong in a new
   `harrow-server-h1` crate or inside `harrow-server`?

---

## 13. Summary

The right architectural move is:

- keep `harrow-core` as the application dispatcher
- introduce a real HTTP/1 transport dispatcher
- refactor Tokio first
- align Monoio second
- extract shared code third

This matches the useful part of the `ntex` split while preserving Harrow's
simpler request/handler/middleware model.
