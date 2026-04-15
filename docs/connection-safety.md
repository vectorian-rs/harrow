# Connection Safety and Timeout Architecture

**Status:** current as of 2026-04-15
**Scope:** custom HTTP/1 backends on `feat/custom-http-backend`

This document describes how Harrow protects itself from slow, idle, and
malicious TCP clients at the transport layer.

The important architectural point is that these controls now live in Harrow's
own HTTP/1 connection loops, not in Hyper builder configuration. The relevant
runtime shape is documented in
[`docs/strategy-local-workers.md`](./strategy-local-workers.md), and the
dispatcher split is documented in
[`docs/h1-dispatcher-design.md`](./h1-dispatcher-design.md).

## The Problem

HTTP servers need to defend against clients that abuse the connection lifecycle:

| Attack | Description | Impact |
|---|---|---|
| Slow-loris | Client sends headers or body bytes extremely slowly | Holds a connection slot open and ties up worker-local state |
| Idle connection | Client opens a TCP connection and does nothing useful | Burns file descriptors and worker capacity |
| Slow-read | Client reads the response extremely slowly | Pins response state and socket buffers |
| Connection flood | Many clients connect at once | Exhausts file descriptors and memory |
| Large concurrent uploads | Many clients stream large bodies simultaneously | Inflates request-body memory usage if buffering is uncontrolled |

These are different from application-level handler timeouts. A route or
middleware timeout only begins after the request has already reached the
application. Connection abuse happens before or during transport dispatch.

## Design Principles

Harrow's transport safety story is built around four principles:

1. Connection-level controls belong in the transport dispatcher, not in
   middleware.
2. Connection ownership stays local to the worker handling that socket.
3. Request-body buffering must be explicit and bounded.
4. Timeouts and hard limits should be operator-configurable because the right
   tradeoff differs between direct internet exposure, reverse-proxy deployments,
   and benchmarks.

## Current Safety Controls

These controls are configured via `ServerConfig` and enforced by the backend's
HTTP/1 connection loop.

### 1. Header Read Timeout

```rust
header_read_timeout: Option<Duration>  // default: Some(5s)
```

Limits how long Harrow will wait for a complete request head.

This protects against:

- slow-loris headers
- idle accepted sockets that never produce a request
- keep-alive clients that stop sending the next request

On the custom backends this is enforced directly in the request-head read path.
It is no longer a Hyper builder option.

### 2. Body Read Timeout

```rust
body_read_timeout: Option<Duration>  // default: None
```

Limits how long Harrow will wait between request-body progress events while the
body is being consumed.

This protects against:

- slow body uploads
- clients that send valid headers and then trickle request-body bytes

This timeout is enforced while the dispatcher is pumping body chunks into the
request body stream.

### 3. Connection Timeout

```rust
connection_timeout: Option<Duration>  // default: Some(300s)
```

Places a coarse upper bound on total connection lifetime.

This is a backstop for:

- long-lived idle sockets
- slow-read clients
- connections that otherwise never terminate cleanly

It is intentionally coarse. It is not a substitute for finer-grained header or
body timeouts.

### 4. Max Connections

```rust
max_connections: usize  // default: 8192
```

Limits concurrent accepted connections.

This protects against connection floods and ensures the server can fail fast
instead of accepting unbounded file-descriptor and memory growth.

Backends may divide this into per-worker budgets, but the operator-facing
contract is one global maximum.

### 5. Drain Timeout

```rust
drain_timeout: Duration  // default: 30s
```

On shutdown, Harrow stops accepting new sockets and gives in-flight connections
time to finish before aborting the remainder.

This prevents shutdown from hanging forever on stuck connections.

## Request-Body Backpressure

The next major safety/performance requirement is bounded request-body
backpressure.

The target model is:

- request-body buffering is bounded
- when that buffer is full, the dispatcher stops reading more body bytes from
  the socket
- reading resumes only when the handler drains buffered data

This matters for both correctness and survivability under concurrency. Without
it, many simultaneous uploads can turn into unbounded in-memory buffering.

Current backend state:

- Tokio has streamed request bodies and a bounded channel today, but not yet the
  final local byte-budgeted queue shape.
- Monoio has streamed request bodies and is moving toward a worker-local
  byte-bounded queue.
- Meguri still buffers request bodies before dispatch and needs a streaming path
  first.

So the design direction is clear, but backend parity is still in progress.

## What Harrow Still Does Not Have

### 1. Write / Slow-Read Timeout

Harrow does not yet enforce a server-side write timeout or minimum response
throughput policy.

A client that reads the response extremely slowly can still hold the connection
open until either:

- the response finishes, or
- `connection_timeout` expires

This is a real remaining gap.

### 2. Full Request-Body Backpressure Parity

The architecture target is a byte-bounded payload queue on every backend. That
is not fully true yet across Tokio, Monoio, and Meguri.

### 3. Proxy-Aware Policy Defaults

Harrow exposes the right knobs, but it does not yet auto-tune behavior based on
whether it sits behind a reverse proxy. Operators still choose that profile
themselves.

## Configuration Profiles

### Direct Internet Exposure

Use the defaults unless there is measured reason to relax them:

```rust
ServerConfig::default()
```

That gives:

- header read timeout
- connection lifetime cap
- max connection cap
- graceful drain timeout

Add `body_read_timeout` if request bodies are expected and slow upload clients
are part of the threat model.

### Behind a Reverse Proxy

If nginx, HAProxy, ALB, or another proxy already enforces header/body/idle
policies, it can be reasonable to relax some backend connection timers:

```rust
ServerConfig {
    header_read_timeout: None,
    connection_timeout: None,
    ..Default::default()
}
```

That trades safety responsibility to the proxy in exchange for lower timeout
overhead in the backend.

### Benchmarks

Benchmark servers should disable controls that the comparison baseline is not
paying for:

```rust
ServerConfig {
    header_read_timeout: None,
    body_read_timeout: None,
    connection_timeout: None,
    ..Default::default()
}
```

This keeps the benchmark focused on transport, dispatch, and runtime overhead
instead of timeout policy.

## Relationship to Application Timeouts

Application timeouts and connection timeouts solve different problems.

Connection-level controls protect the server from bad clients:

```text
accept socket
  -> wait for request head
  -> stream request body
  -> write response
  -> keep alive or close
```

Application-level timeouts protect the server from its own slow handlers after
the request has already reached the app.

Both are useful, but they should not be confused.

## Where to Look in the Code

- Tokio: `harrow-server-tokio/src/server.rs`, `src/connection.rs`, and `src/h1/*`
- Monoio: `harrow-server-monoio/src/h1/*`
- shared server config helpers: `harrow-server/src/lib.rs`

Those are now the authoritative implementation surfaces rather than one
Hyper-specific builder path.
