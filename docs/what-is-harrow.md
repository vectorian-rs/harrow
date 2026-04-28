# What Harrow Is

Harrow is a small, explicit Rust HTTP framework focused on predictable request
handling, custom HTTP/1 backends, and opt-in production features.

The core idea is simple:

```rust
async fn handler(req: harrow::Request) -> harrow::Response {
    harrow::Response::text("hello")
}
```

Handlers receive a `Request` and return something that becomes a `Response`.
There are no handler macros, no extractor-heavy signatures, no Tower service
stack, and no default server runtime hidden behind the public API.

## Design Priorities

1. **Explicit over magical**: request data is accessed through request helpers
   such as `param`, `query_param`, `header`, `body_bytes`, and `body_json`.
2. **Backend choice is explicit**: applications opt into a server backend with a
   Cargo feature, currently `tokio` or `monoio`.
3. **HTTP/1 first**: Harrow's production path is a custom HTTP/1.1 transport
   shared across backend implementations where practical.
4. **Small core, opt-in features**: middleware, observability, content formats,
   WebSocket support, and backend integrations are feature-gated.
5. **Measured performance**: performance claims should come from repeatable
   benchmarks, not broad framework marketing.

## Current Public Backends

- **Tokio**: cross-platform custom HTTP/1 backend. This is the general-purpose
  default recommendation.
- **Monoio**: Linux-focused io_uring backend for high-throughput deployments.

The workspace also contains **Meguri**, a direct io_uring backend used for
experimentation and benchmarks. It is not part of the stable root `harrow` API.

See [Backend Support](./backend-support.md) for the full support matrix.

## Non-goals for the Current 1.0 Line

Harrow is not trying to become an all-in-one web platform before 1.0. The
following are intentionally out of scope or research-only for now:

- HTTP/3 / QUIC
- WebTransport
- built-in gRPC
- built-in GraphQL server
- background job queue
- raw TCP/UDP framework APIs
- large extractor ecosystems as the primary handler model

These may be explored later, but the 1.0 line is about hardening the current
HTTP/1 framework, documenting the runtime choices, and making production use
clear.
