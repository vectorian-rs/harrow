# Changelog

All notable changes to this project will be documented in this file.

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
