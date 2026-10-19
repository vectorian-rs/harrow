# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Security
- HTTP/1 codec: a chunk-size line without hex digits (`\r\n`, `;ext\r\n`, ` \r\n`) was read as size 0, ending the body early and letting the following bytes be parsed as a new request on a keep-alive connection. Every chunk-size must now start with a hex digit (RFC 9112 §7.1).
- HTTP/1 codec: `Content-Length` accepted a leading `+` (`+5`), and `Content-Length` / `Transfer-Encoding` values padded with Unicode whitespace (e.g. U+00A0) were trimmed and accepted. Both are now rejected: `Content-Length` must be `1*DIGIT`, and only spaces and tabs are trimmed. An upstream proxy could read such values differently (request smuggling).
- meguri server: a connection's slab slot and its buffers were released while a RECV or SEND was still in flight (typically when a handler answered without reading the request body), so the kernel could write into freed memory, and a late completion could be applied to a newer connection in the same slot. Slots are now kept until their last completion, completions carry a slot generation, and sockets are shut down before close.
- meguri: `Ring::drop` freed the buffers of abandoned operations before the kernel had finished with them; it now cancels and waits (bounded), and leaks rather than frees if the kernel is still busy.

### Added
- Security headers middleware (`security-headers` feature)
- `harrow-server-tokio-hyper`: prototype Tokio backend where Hyper owns HTTP/1 parsing and framing, with one `current_thread` runtime per worker (`tokio-hyper` feature). Used to compare Hyper against the custom codec before 1.0.
- `Response::set_ext` / `Response::ext`: typed per-response data for outer middleware, e.g. an error that only knows its status, rendered with the request id by one middleware. Never written to the wire.
- Tokio backend: `start` / `start_with_config` returning a `ServerHandle` (`local_addr`, `shutdown`, `wait`), so a multi-worker server can drain gracefully on SIGTERM. `serve_multi_worker` is now `start_with_config(..)?.wait()`; binding port 0 works.
- `harrow_server::start_workers`: shared worker startup for the threaded backends (Tokio, Monoio, Meguri).
- `h1_lifecycle::trace`: backends record per-connection lifecycle events and tests replay them against the lifecycle model (`lifecycle-trace` feature, off in normal builds). The Tokio backend is instrumented.
- meguri `ops` layer is now functional: `Driver` plus `Accept`, `Connect`, `Recv`, `Send`, `Read`, `Write`, `Timeout` and `Cancel` as owned-buffer futures; dropping one before completion cancels it and keeps its buffers until the kernel is done.
- Adversarial HTTP/1 test suite (parser, framing, response writing, lifecycle) and a first recorded codec fuzz campaign; see `docs/operations/verification.md`. CI now builds the fuzz targets.
- `examples/framework-compare`: the same API in Harrow, axum, ntex and Tako, with a shared black-box contract test.

### Fixed
- HTTP/1 codec enforces the header size limit
- Stricter `Transfer-Encoding` parsing
- Response framing headers are normalized, so a response never carries both `Content-Length` and `Transfer-Encoding`
- Unsupported `Expect` values are rejected with `417 Expectation Failed` before the handler runs; non-ASCII `Expect` values with `400`. `Expect: 100-continue` on HTTP/1.0 is ignored (previously Harrow sent an interim `100` to 1.0 clients).
- `decode_chunked_with_limit` skipped the CRLF after each chunk without checking it; it is now a wrapper over `PayloadDecoder` and exactly as strict.
- `encode_chunk_into(b"")` wrote `0\r\n\r\n`, the end-of-body marker; an empty chunk now writes nothing.
- Tokio backend: a response to a request still running when shutdown started advertised keep-alive and the connection was then closed; it now carries `Connection: close`.
- meguri: `IORING_OP_TIMEOUT` SQEs were rejected by the kernel (`len` must be 1, count goes in `off`), so the meguri server's dispatch tick and timeout sweep never waited and its event loop spun.
- meguri server: `setsockopt` / `fcntl` results are checked, accepted sockets are close-on-exec and non-blocking from the accept itself, and an interrupted `io_uring_enter` (EINTR) is retried instead of stopping the worker.
- meguri: x86_64 builds failed under `-D warnings` (inline `asm!` without an inner `unsafe` block).

### Changed
- Shared H1 connection lifecycle and threaded worker control moved into `harrow-server`
- **Breaking:** `CodecError` has a new `ExpectationFailed` variant; `ErrorResponse` a new `ExpectationFailed` variant.
- **Breaking (meguri, not published):** `SubmissionQueue` builders that take raw pointers or fds are now `unsafe fn`, with per-opcode safety contracts. `ops::Read`/`Write`/… are aliases of the generic `ops::Op<K>`; `Accept` yields an `OwnedFd`; `Read`/`Recv`/`Write`/`Send` return their buffer together with the result.
- The lifecycle model (and its TLA+ spec) allows an error response while reading the request body, not only the headers.
- Benchmarks: Tako 2.4 with a thread-per-core mode, and the Hyper prototype, added to the comparison runs.
- Rust toolchain pinned to 1.99.1.

### Removed
- **Breaking:** the `tls` feature (`harrow` and `harrow-server-tokio`). It pulled in `rustls` but no code used it, so enabling it silently served plain HTTP. No backend terminates TLS in-process; terminate at a reverse proxy or load balancer.

## [0.10.0] - 2026-04-20

### Added
- `harrow-codec-h1`: Harrow's own HTTP/1.1 codec (request parsing, response serialization, chunked encoding, incremental `PayloadDecoder`), with cargo-fuzz targets and proptest round-trip tests
- `harrow-io`: runtime-agnostic buffer pool and I/O traits
- `harrow-server`: shared listener, worker, shutdown and config primitives for all backends
- Request bodies are streamed through dispatch with backpressure on the Tokio, Monoio and meguri backends
- Per-worker app factories for local-worker runtimes
- TLA+ model of the connection state machine and deterministic simulation tests for the connection handler

### Changed
- `harrow-server-tokio` now runs on `harrow-codec-h1` instead of Hyper. Hyper is no longer a default dependency of `harrow-core`.
- `harrow-server-monoio` migrated to `harrow-codec-h1`
- Fully buffered responses set `Content-Length` instead of being sent chunked (about 2x throughput on small responses)
- Lower allocation overhead when writing response heads
- Bumped rolly to 0.16

### Fixed
- Request smuggling hardening: reject any `Content-Length` + `Transfer-Encoding` combination and unsupported transfer codings, enforce cumulative body limits, fix meguri body boundaries with pipelined requests

## [0.9.6] - 2026-04-11

### Added
- meguri HTTP/1.1 server: completion-driven io_uring event loop with slab-indexed connections, `SO_REUSEPORT`, timeout sweeping and 100-continue support
- `.o11y_middleware()` for apps that set up their own tracing subscriber; `.o11y()` remains the one-call setup
- Public `init_telemetry` / `try_init_telemetry` helpers

## [0.9.5] - 2026-04-10

### Added
- `serve_multi_worker()`: one `current_thread` Tokio runtime per CPU core, with `SO_REUSEPORT` for kernel-level connection distribution

### Changed
- Bumped rolly to 0.14 (split into `rolly-tokio` and `rolly-monoio`)

## [0.9.3] - 2026-04-05

### Fixed
- `From<Error> for Response` impls for `MissingStateError`, `MissingExtError`, `BodyError`, `WsError`, and `ProblemDetail` — handlers can now use `?` directly without `.map_err()` or `.unwrap()`
- `IntoResponse` impls for `MissingStateError` and `MissingExtError` — missing state returns 500 instead of requiring a panic

### Changed
- License simplified from `MIT OR Apache-2.0` to `MIT`
- Added README to all crate packages on crates.io
- Added repository metadata to all workspace crates
- Consistent import style across harrow-core modules

## [0.9.0] - 2026-04-05

First published release on crates.io.

### Added
- **WebSocket support** (`ws` feature)
  - RFC 6455 compliant handshake with correct GUID
  - `upgrade()` and `upgrade_with_config()` for HTTP → WebSocket upgrade
  - `WebSocket` struct with `recv()`, `send()`, `close()` methods
  - `Stream` and `Sink` trait implementations for composable async patterns and `.split()`
  - `WsConfig` builder: `max_message_size`, `max_frame_size`, `write_buffer_size`, `max_write_buffer_size`, `accept_unmasked_frames`
  - Subprotocol negotiation via `WsConfig::protocols()`
  - Auto close response — server replies to client close frames automatically
  - `Utf8Bytes` zero-copy text type (backed by `bytes::Bytes`)
  - `close_code` constants (RFC 6455 Section 7.4.1)
  - `Message` types use `bytes::Bytes` for Binary/Ping/Pong (zero-copy on receive)
- `WsError::NotUpgradable` — explicit error when `OnUpgrade` handle is missing
- `WsError::Transport` — proper error variant for runtime WebSocket errors
- Always use `serve_connection_with_upgrades` (zero overhead, matches axum)

### Changed
- `Message::Text` holds `Utf8Bytes` instead of `String`
- `Message::Binary`/`Ping`/`Pong` hold `bytes::Bytes` instead of `Vec<u8>`
- `WebSocket::send()` and `close()` return `WsError` instead of `Box<dyn Error>`

## [0.5.1] - 2026-03-25

### Added
- Monoio thread-per-core server bootstrap
- Unified Docker benchmark harness

### Changed
- Standardized vegeta load-test targets and examples

## [0.5.0] - 2026-03-24

### Added
- HTTP/2 support for harrow-server-monoio
- Monoio buffer pool for I/O operations
- Monoio cancellation safety and observability
- Unified performance test orchestrator with Vegeta support
- OpenAPI 3.0.3 JSON generation from `RouteTable`

### Changed
- Renamed `harrow-server` to `harrow-server-tokio`

## [0.4.0] - 2026-03-22

### Added
- Problem detail responses (RFC 9457)
- Probe APIs: `health()`, `liveness()`, `readiness_handler()`
- `default_problem_details()` for structured 404/405 responses
- Route 404/405 through global middleware
- HTTP server metrics
- `body_read_timeout` in `ServerConfig`
- Middleware combinators: `map_request`, `map_response`, `when`, `unless`

### Changed
- Bumped rolly to 0.10
- Removed `timeout_middleware` — use `ServerConfig` connection timeouts instead
- Removed `InMemorySessionStore` and `InMemoryBackend` — use your own store implementations

## [0.2.7] - 2026-03-21

### Added
- Experimental monoio-based HTTP/1.1 server (`harrow-server-monoio`)
- HTTP/2 h2c integration tests
- Per-request extensions on `Request`
- Proptest and cargo-fuzz verification infrastructure

### Changed
- Hardened monoio server: 100-continue, body limits, Slowloris deadline, TCP_NODELAY

## [0.2.6] - 2026-03-19

### Added
- Rate-limit middleware with pluggable backend

## [0.2.5] - 2026-03-18

### Added
- Body-limit middleware

## [0.2.3] - 2026-03-17

### Added
- Catch-panic middleware — returns 500 instead of connection reset
- Session middleware with pluggable store
- `harrow-middleware` crate with feature-gated middleware
- `harrow-serde` crate with JSON and MessagePack serialization
- `App::client()` for TCP-free testing

## [0.1.0] - 2026-03-10

### Added
- Initial release
- Macro-free HTTP framework over Hyper 1.x
- Route groups with shared prefix and scoped middleware
- matchit-based routing
- Opt-in observability via rolly
- Request-id, CORS, compression middleware
- HEAD request auto-handling (RFC 9110)
- Query string parsing with percent-decoding
- Body size limits with Content-Length pre-check
- Configurable o11y with OTLP trace export
