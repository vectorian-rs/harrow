# [monoio] Add HTTP/2 Support

## Problem
`harrow-server-monoio` is HTTP/1.1 only. Modern clients (browsers, mobile apps, gRPC) heavily use HTTP/2 for:
- Request multiplexing over single connection
- Server push (less relevant)
- Header compression (HPACK)
- Better throughput on high-latency links

## Goals
Add HTTP/2 support with feature parity to HTTP/1.1.

## Options Considered

### Option A: Use `monoio-http` (Recommended)
ByteDance's official HTTP library for monoio.
- **Pros:** Purpose-built for monoio/io_uring, production-proven
- **Cons:** Additional dependency, less mature than hyper

### Option B: Port `h2` crate
The `h2` crate is the de facto HTTP/2 implementation in Rust.
- **Pros:** Battle-tested, same as tokio/hyper stack
- **Cons:** Built on tokio-async traits, may need adaptation layer

### Option C: Manual implementation
- **Pros:** Full control, can optimize for io_uring
- **Cons:** High complexity, security risk, maintenance burden

## Proposed Approach

Start with **Option A** (`monoio-http`) as an experimental feature:

```toml
[features]
default = ["http1"]
http1 = []
http2 = ["dep:monoio-http"]
```

## Technical Requirements

- [ ] ALPN negotiation (`h2` vs `http/1.1`)
- [ ] Stream multiplexing (handle concurrent requests on one connection)
- [ ] Flow control (WINDOW_UPDATE frames)
- [ ] Settings frame handling
- [ ] Prioritization (optional for MVP)

## Acceptance Criteria
- [ ] HTTP/2 server passes `h2spec` compliance tests
- [ ] Integration tests for:
  - [ ] Basic request/response
  - [ ] Concurrent streams
  - [ ] Large body streaming
  - [ ] Graceful connection close (GOAWAY)
- [ ] Feature flag enables HTTP/2 alongside HTTP/1.1
- [ ] Benchmark comparison: HTTP/1.1 vs HTTP/2 on same workload

## Priority
**Medium-High** — Required for production parity with tokio server.

## Labels
`enhancement`, `monoio`, `http2`, `breaking-change`

## Related
- `strategy-io-uring.md` Section 3 (kernel features)
- `harrow-server` (HTTP/2 via hyper)
