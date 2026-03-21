# Connection Safety and Timeout Architecture

**Status:** current as of 2026-03-21

This document describes how Harrow protects against slow, idle, and malicious
TCP connections at the server level, independently of application middleware.

## The Problem

HTTP frameworks must defend against clients that abuse the connection lifecycle:

| Attack | Description | Impact |
|---|---|---|
| Slow-loris | Client sends headers at 1 byte/sec, never completing the request | Holds a connection slot open indefinitely |
| Idle connection | Client opens a TCP connection and never sends a request | Consumes a file descriptor and a semaphore slot |
| Slow-read | Client sends a valid request but reads the response at 1 byte/sec | Blocks the write path, pins the connection task |
| Connection flood | Many clients open connections simultaneously | Exhausts file descriptors and memory |

These are distinct from application-level timeouts. A handler timeout
(`TimeoutMiddleware`) only activates after the full request has been received
and the handler begins executing. Connection-level attacks happen before the
handler is ever reached.

## Harrow's Defense Layers

Harrow addresses connection-level threats through `ServerConfig`, which is
passed to `serve_with_config()`. All knobs have safe production defaults.

### 1. Header Read Timeout

```rust
header_read_timeout: Option<Duration>  // default: Some(5s)
```

**What it does:** Limits how long hyper waits for the client to send complete
HTTP headers after the TCP connection is accepted.

**What it protects against:** Slow-loris attacks and any client that opens a
connection but sends headers too slowly.

**How it works:** Wired directly into hyper's HTTP/1 builder via
`http1().header_read_timeout()`. Requires `http1().timer(TokioTimer::new())`
to enable the timer backend. If the client does not send a complete header
block within the timeout, hyper closes the connection.

**Trade-offs:** Each connection gets a Tokio timer. At extreme throughput
(500K+ RPS on 48 cores), timer creation and cancellation become visible in
profiles. This is why the setting is `Option<Duration>` — setting it to
`None` eliminates the timer entirely, which is appropriate for benchmarks
or when running behind a reverse proxy that enforces its own header timeout.

### 2. Connection Timeout (Total Lifetime)

```rust
connection_timeout: Option<Duration>  // default: Some(300s)
```

**What it does:** Hard cap on the total lifetime of any single connection,
regardless of activity.

**What it protects against:** Idle connections, slow-read attacks (as a
coarse backstop), and any client that holds a connection open too long.

**How it works:** The connection future is wrapped in
`tokio::time::timeout(ct, conn)`. If the connection has been open longer
than the timeout, it is dropped. This catches every case, but the
granularity is coarse — a slow-read client can hold a connection for up to
the full timeout duration.

**Trade-offs:** Same timer overhead concern as `header_read_timeout`. Setting
to `None` removes the per-connection timer. The 5-minute default balances
protection against long-lived idle connections with tolerance for legitimate
HTTP/1.1 keep-alive clients that make many requests over a single connection.

### 3. Max Connections (Semaphore)

```rust
max_connections: usize  // default: 8192
```

**What it does:** Limits the number of concurrent TCP connections the server
will accept. New connections beyond this limit are immediately dropped at the
TCP level.

**What it protects against:** Connection floods. Even if each individual
connection is cheap, accepting unbounded connections will exhaust file
descriptors and memory.

**How it works:** A `tokio::sync::Semaphore` with `max_connections` permits.
Each accepted connection acquires a permit via `try_acquire_owned()`. If no
permit is available, the TCP stream is dropped immediately — no response is
sent, no resources are allocated beyond the accept() syscall.

**Trade-offs:** The default of 8192 is appropriate for most servers. Systems
behind load balancers with health checks should ensure the limit is high
enough to accommodate health check connections alongside real traffic.

### 4. Drain Timeout (Graceful Shutdown)

```rust
drain_timeout: Duration  // default: 30s
```

**What it does:** During shutdown, limits how long the server waits for
in-flight connections to complete before forcefully aborting them.

**What it protects against:** Shutdown stalls caused by connections that
refuse to close.

**How it works:** After the shutdown signal fires, the accept loop stops.
The server then calls `tokio::time::timeout(drain_timeout, join_all)` on
the remaining connection tasks. If the timeout expires, all remaining
connections are aborted via `JoinSet::abort_all()`.

## What Harrow Does NOT Have

### HTTP/1 Keep-Alive Idle Timeout

hyper's HTTP/1 builder exposes `keep_alive(bool)` (on/off) but does **not**
expose a keep-alive idle timeout — the time between a completed
response and the next request on the same connection.

This means a client that sends one request, receives the response, and then
holds the connection open without sending another request will be protected
only by the coarse `connection_timeout` (5 minutes by default).

For most production deployments, this gap is covered by the reverse proxy
(nginx, HAProxy, ALB) which enforces its own idle timeout (typically 60s).
For servers exposed directly to the internet without a reverse proxy, the
`connection_timeout` provides a backstop.

### Write Timeout (Slow-Read Protection)

hyper does not expose a server-side write timeout or minimum response
throughput knob. A client that reads the response body at 1 byte/sec will
hold the connection for as long as the write takes (bounded only by
`connection_timeout`).

Proper slow-read protection would require wrapping the I/O stream in a
custom `AsyncWrite` implementation that enforces a minimum bytes-per-second
rate or a per-write timeout. This is a meaningful piece of work and is not
currently implemented.

Again, reverse proxies handle this in practice. nginx's `send_timeout` and
`proxy_read_timeout` terminate slow-read clients before the backend sees
the problem.

## Configuration in Practice

### Production (direct internet exposure)

Use the defaults. They are designed for this case:

```rust
ServerConfig::default()
// header_read_timeout: Some(5s)
// connection_timeout: Some(300s)
// max_connections: 8192
// drain_timeout: 30s
```

### Production (behind a reverse proxy)

The proxy handles slow clients. You can relax or disable connection-level
timeouts to reduce per-connection overhead:

```rust
ServerConfig {
    header_read_timeout: None,  // proxy enforces this
    connection_timeout: None,   // proxy enforces this
    ..Default::default()
}
```

### Benchmarks

Disable all per-connection timers to match the baseline of frameworks that
do not set them by default (e.g., Axum's default `axum::serve`):

```rust
ServerConfig {
    header_read_timeout: None,
    connection_timeout: None,
    ..Default::default()
}
```

This is what `harrow-perf-server` does. See `docs/article.md` for the
measured impact: enabling these timers at 500K+ RPS on 48 cores caused a
**2x throughput regression** due to Tokio timer-wheel contention.

## Comparison with Other Frameworks

| Feature | Harrow | Axum (default) | Actix-web |
|---|---|---|---|
| Header read timeout | Yes (5s default) | No (hyper default: none) | Yes (5s default) |
| Connection lifetime | Yes (5min default) | No | Yes (`keep_alive` default 5s) |
| Max connections | Yes (semaphore, 8192) | No built-in limit | Yes (25,000 default) |
| Graceful drain | Yes (30s default) | Yes (via `axum::serve().with_graceful_shutdown()`) | Yes |
| Keep-alive idle timeout | No (hyper HTTP/1 limitation) | No | Yes (5s default) |
| Write/slow-read timeout | No | No | No (has `client_disconnect_timeout`) |

Harrow ships **safer defaults** than Axum's default serving path. The
trade-off is measurable timer overhead at extreme throughput, which is why
every timeout is `Option<Duration>` — the operator chooses the right
balance for their deployment.

## Relationship to Middleware Timeouts

`TimeoutMiddleware` is a separate concern:

```
TCP accept
  → header_read_timeout (connection level)
    → request fully received
      → TimeoutMiddleware starts (application level)
        → handler runs
      → TimeoutMiddleware expires if handler is too slow
    → response sent
  → connection_timeout (total connection lifetime)
TCP close
```

Connection-level timeouts protect the server from misbehaving clients.
`TimeoutMiddleware` protects the server from its own slow handlers.
Both are needed.

## Source

The implementation lives in `harrow-server/src/lib.rs`. The `ServerConfig`
struct and all wiring is in `serve_with_config()`.
