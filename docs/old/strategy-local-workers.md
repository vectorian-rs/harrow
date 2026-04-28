# Strategy: Local Worker Runtime Model

**Status:** Draft
**Date:** 2026-04-15
**Scope:** `feat/custom-http-backend`

> Historical implementation strategy note. For the current product scope and
> backend support policy, see [docs/prds/harrow-1.0.md](./prds/harrow-1.0.md).

---

## 1. Decision

Harrow is adopting a local-worker runtime model for its performance-sensitive
server backends.

In practice this means:

- each worker owns its accepted connections and keeps them local
- connection state, parser state, payload queues, and response state stay on the
  same worker
- hot-path coordination prefers local ownership and explicit backpressure over
  shared queues and cross-thread scheduling
- Tokio should move toward per-worker `current_thread` runtimes plus
  `LocalSet`/local tasks
- Monoio should continue down the local-worker/thread-per-core path
- Meguri remains separate until it has streaming request bodies and a clearer
  io_uring-specific shape

This is the closest Harrow analogue to the nginx/ntex model.

---

## 2. Why This Direction

Two things became clear from the current branch.

First, the large performance wins are not coming from fancy abstraction or from
`io_uring` by itself. They come from local ownership:

- no work-stealing on the hot path
- less cross-thread synchronization
- better cache locality for connection state
- easier explicit backpressure for request bodies

Second, `ntex` validates that this model also applies to Tokio. Its Tokio path
still uses a local-task runtime shape, the same HTTP/1 dispatcher, and the same
local payload backpressure model. Tokio is not limited to a generic
work-stealing server architecture.

The custom HTTP/1 backend removes the biggest blocker to this direction. Harrow
is no longer tied to Hyper's runtime model or its connection builder surface.

---

## 3. What This Means Per Backend

### Tokio

Target direction:

- one runtime per worker
- `current_thread` worker runtime
- `LocalSet` for connection tasks and internal protocol tasks
- local request-body queue with explicit readiness/backpressure
- explicit HTTP/1 dispatcher state machine

Tokio should still be operationally straightforward. The point is not to become
Tokio-hostile. The point is to stop paying work-stealing costs on a connection
model that does not benefit from them.

### Monoio

Monoio is already aligned with this direction:

- one worker owns the connection
- no work-stealing
- explicit protocol loop

The next step is to finish the request-body side with a local byte-bounded
payload queue so reads stop when the handler-side buffer is full.

### Meguri

Meguri should not be forced into Tokio/Monoio internals yet.

It still needs:

- streaming request bodies before dispatch
- a clear local backpressure story that fits its completion-driven `io_uring`
  model

Meguri should share protocol rules and invariants, but not be used to constrain
the Tokio/Monoio dispatcher design prematurely.

---

## 4. Core Invariants

The local-worker direction implies these invariants:

1. A connection stays on one worker for its active lifetime.
2. Request parsing, request-body pumping, and response writing are owned by the
   same worker-local dispatcher.
3. Request-body buffering is bounded.
4. When the request-body buffer is full, the server stops reading more body
   bytes from the socket until the handler drains it.
5. Application dispatch stays separate from transport dispatch.
6. Shared abstractions should live at the protocol boundary, not above it.

This is the key Harrow split:

- `harrow-core` owns application dispatch
- the HTTP/1 dispatcher owns transport state
- backend crates own runtime and socket integration

---

## 5. Relationship to `ntex` and nginx

Harrow is not trying to clone `ntex`'s entire programming model, and it is not
trying to become nginx-in-Rust.

The useful parts to copy are narrower:

- worker-local connection ownership
- explicit dispatcher state machines
- bounded request-body buffering
- stop-reading backpressure
- thin runtime adapters underneath the protocol loop

That is the part of the nginx/ntex design space that produced the obvious
performance gains.

---

## 6. Relationship to Other Docs

- [`docs/h1-dispatcher-design.md`](./h1-dispatcher-design.md) describes the
  protocol split and dispatcher shape.
- [`docs/strategy-tpc.md`](./strategy-tpc.md) remains the broader research note
  on thread-per-core systems.
- [`docs/strategy-io-uring.md`](./strategy-io-uring.md) remains the lower-level
  I/O strategy note. `io_uring` is important, but it is not the primary reason
  for the runtime direction.
- [`docs/article.md`](./article.md) records the performance evidence that pushed
  Harrow toward this decision.

---

## 7. Immediate Consequences

Near-term documentation and implementation work should assume:

- Tokio and Monoio converge on the same HTTP/1 dispatcher structure
- Tokio eventually moves from generic worker spawning toward local worker
  runtimes
- request-body backpressure should be byte-bounded, not only message-bounded
- shared HTTP/1 extraction happens only after Tokio and Monoio have converged

This is the architecture Harrow should optimize around unless new evidence says
otherwise.
