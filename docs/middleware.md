# Middleware Comparison

**Status:** current as of 2026-03-21

This document compares the middleware surface that is common in Axum/Tower
projects with what Harrow currently ships.

It is intentionally practical rather than aspirational. A row is marked
"Yes" only if Harrow ships it today. "Partial" means Harrow covers part of the
use case but not with the same generality as the usual Axum/Tower choice.

## Current Harrow Middleware Surface

Harrow's shipped middleware modules are currently:

- `timeout`
- `request-id`
- `cors`
- `o11y`
- `catch-panic`
- `body-limit`
- `compression`
- `rate-limit`
- `session`

These are defined in `harrow-middleware/src/lib.rs` and re-exported from
`harrow/src/lib.rs` behind crate features.

## Support Matrix

| Category | Axum / Tower usual choice | Harrow status | Harrow equivalent / notes |
|---|---|---|---|
| Tracing / request spans | `tower-http::trace::TraceLayer` | Yes | `o11y_middleware` creates request spans, derives trace IDs, records latency and status |
| Request ID | `tower-http::request_id` | Yes | `request_id_middleware`, `request_id_middleware_with_header` |
| Metrics | `tower-http::metrics` or tracing/OTel stack | Partial | Covered through `o11y` + `rolly`, not as a standalone generic metrics middleware |
| CORS | `tower-http::cors::CorsLayer` | Yes | `CorsConfig`, `cors_middleware` |
| Response compression | `tower-http::compression::CompressionLayer` | Yes | `compression_middleware`; supports `gzip`, `deflate`, optional `br` |
| Request / response decompression | `tower-http::decompression` | No | Not shipped |
| Request timeout | `tower-http::timeout::TimeoutLayer` or `tower::timeout` | Yes | `timeout_middleware` |
| Panic recovery | `tower-http::catch_panic` | Yes | `catch_panic_middleware` |
| Request body limit | `tower-http::limit::RequestBodyLimitLayer` | Yes | `body_limit_middleware` |
| Rate limiting | `tower_governor`, `tower::limit::RateLimitLayer` | Yes | `rate_limit_middleware`, `InMemoryBackend`, `HeaderKeyExtractor`, configurable header style |
| Sessions | `tower-sessions` | Yes | `session_middleware`, `Session`, `SessionConfig`, `InMemorySessionStore` |
| Login / auth helpers | `axum-login`, custom auth middleware, auth crates | No official middleware | Can be built on top of request extensions and session support; design work exists in `docs/auth-middleware.md` |
| CSRF | various add-on crates | No official middleware | Not shipped |
| Validate request headers | `tower-http::validate_request` | No | No generic request-validation middleware shipped |
| Normalize path / trailing slash | `tower-http::normalize_path` | No | No shipped normalize-path middleware |
| Header propagation | `tower-http::propagate_header` | No | Handlers and middleware can mutate headers directly, but no generic reusable layer |
| Set / override headers | `tower-http::set_header` | No | Same note as above |
| Cookie jar utilities | `tower-cookies`, `axum-extra::CookieJar` | No general cookie crate | Session middleware manages cookies internally, but Harrow does not ship a generic cookie-jar API |
| Static file serving | `tower-http::services::ServeDir` | No | Not shipped |
| Generic request body transforms | `tower-http::map_request_body` | No | Not shipped |
| Generic response body transforms | `tower-http::map_response_body` | No | Not shipped |
| Retry | `tower::retry` | No | Harrow does not ship generic resilience layers |
| Buffer | `tower::buffer` | No | Not shipped |
| Load shed | `tower::load_shed` | No | Not shipped |
| Request concurrency limit | `tower::limit::ConcurrencyLimitLayer` | Partial | Harrow has server-level `max_connections`, but not a request middleware equivalent |
| Connection-level timeouts | hyper builder config, framework defaults | Yes | `ServerConfig`: header read timeout, connection lifetime, max connections, drain timeout. See `docs/connection-safety.md` |

## Short Read

If you compare Harrow to the common Axum/Tower stack, the current picture is:

- Harrow already covers the core HTTP middleware many applications expect:
  request ID, observability, CORS, compression, timeout, panic recovery,
  body limits, rate limiting, and sessions.
- Harrow does not yet cover the broader Tower ecosystem surface:
  generic request validators, normalize-path, static files, generic header/body
  transform layers, or generic resilience layers like retry/buffer/load-shed.
- Auth and CSRF are the biggest app-level gaps if the comparison point is the
  wider Axum ecosystem rather than only `tower-http`.

## Scope Notes

### Observability

Harrow's `o11y` support is more opinionated than `tower-http::trace`.
It is not only request logging. It wires request ID generation, trace ID
derivation, span fields, and OTLP-oriented configuration through
`harrow_o11y::O11yConfig`.

### Compression

Harrow ships response compression, but the current implementation is
whole-body buffering middleware rather than a streaming compressor. That makes
it functionally comparable to the common middleware category, but not
necessarily identical in performance characteristics.

### Sessions vs Framework Scope

Session management is shipped as middleware, which is the right boundary for
Harrow. It should not move into core framework primitives.

### Missing Tower Layers

The missing generic Tower layers are not accidental. Harrow defines its own
middleware model and does not currently expose Tower `Layer` / `Service`
compatibility as part of the framework surface.

That keeps the API smaller, but it also means Harrow does not automatically
inherit the broader Tower middleware ecosystem.

## Pointers

- Authentication design notes: `docs/auth-middleware.md`
- Rate limiting design and implementation notes: `docs/rate-limiting-middleware.md`
- Connection safety and timeout architecture: `docs/connection-safety.md`
- Broader performance tradeoffs around middleware shape: `docs/article.md`
- Core framework PRD and non-goals: `docs/prds/harrow-http-framework.md`
