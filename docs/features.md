# Feature Status

This page tracks Harrow's current functionality against the broader "one service,
many transports" platform vision. It is intentionally conservative: a feature is
only marked implemented when it exists in the current codebase and is part of the
supported product story.

Legend:

- ✅ implemented / available
- 🟡 partial, backend-specific, experimental, or needs polish
- ❌ not currently implemented / not a Harrow promise

## Summary

Harrow is currently a focused HTTP framework, not a full multi-transport
platform. Its strongest implemented path today is:

> explicit request/response handlers on custom HTTP/1.1 backends, with public
> Tokio and Monoio support, opt-in middleware, observability hooks, and lifecycle
> hardening.

Harrow also includes a first Hyper-based Tokio backend prototype so the stable
production path can be chosen with benchmark data and maintenance risk in view.

## Transports and Protocols

| Feature | Status | Notes |
| --- | --- | --- |
| HTTP/1.1 | ✅ | Implemented through custom Harrow H1 path across Tokio, Monoio, and experimental Meguri. Stable-by-default support level is under review pending Hyper comparison and hardening evidence. |
| REST-style HTTP APIs | ✅ | Strong fit for JSON/text API backends. |
| HTTP/2 | 🟡 | Required before 1.0 across Harrow server backends. Monoio has partial H2 code/tests today; Tokio may use the Hyper backend rather than extending the custom H1 path. |
| HTTP/3 / QUIC | ❌ | Not implemented. |
| WebSocket | 🟡 | Tokio-side `ws` feature exists. Not backend-universal. |
| Streaming responses | ✅ | `Response::streaming` exists. Needs more examples/helpers. |
| SSE | 🟡 | Can be built from streaming responses; no first-class helper yet. |
| WebTransport | ❌ | Not implemented. |
| gRPC | ❌ | Not implemented as a first-party feature. |
| raw TCP | ❌ | Not exposed as a framework API. |
| raw UDP | ❌ | Not exposed as a framework API. |
| Unix sockets | ❌ | Not currently exposed as a supported transport. |
| PROXY protocol | ❌ | Not implemented. |

## Runtime / Backend Support

| Feature | Status | Notes |
| --- | --- | --- |
| Tokio custom H1 backend | ✅ | Public backend today; stable 1.0 status under review because Harrow owns protocol correctness. |
| Tokio Hyper backend | 🟡 | First HTTP/1 prototype implemented with single-runtime and thread-per-core modes; HTTP/2/TLS/bench parity still pending. |
| Monoio backend | ✅ | Public Linux/io_uring backend; final stable label depends on parity evidence. |
| Meguri backend | 🟡 | Experimental direct io_uring workspace backend, not re-exported from root `harrow`. |
| Compio backend | ❌ | Not supported. |
| Same app mental model across runtimes | ✅ | `App`, `Request`, `Response`, middleware, and request helpers are shared across Tokio and Monoio. |
| TLS | 🟡 | Tokio-oriented feature surface exists; final support wording should be audited before 1.0. |
| HTTP/2 on all server backends | 🟡 | 1.0 target. Monoio is partial; Tokio Hyper backend may become the preferred path; Meguri needs a stabilization decision. |

## Application Primitives

| Feature | Status | Notes |
| --- | --- | --- |
| Middleware system | ✅ | `Middleware`, `Next`, and app middleware stack. |
| Graceful shutdown | ✅ | Tokio and Monoio lifecycle support. |
| Header/body/connection timeouts | ✅ | Exposed through server config. |
| Request body limits | ✅ | Server/request body limit behavior exists. |
| Request IDs | ✅ | `request-id` middleware. |
| Observability hooks | ✅ | `o11y` feature and `harrow-o11y` integration. |
| Metrics backend | 🟡 | Hooks/integration points exist; Harrow does not claim a full built-in metrics backend. |
| In-process signal helpers | 🟡 | Applications can wire runtime signals; not a major first-class API yet. |
| Background job queue | ❌ | Not implemented. |
| Static files | ❌ | Not first-party currently. |
| Stream helpers | 🟡 | Streaming primitive exists; higher-level helpers are still future work. |

## Middleware and Security

| Feature | Status | Notes |
| --- | --- | --- |
| Request ID | ✅ | Implemented. |
| CORS | ✅ | Implemented. |
| Catch panic | ✅ | Implemented. |
| Body limit | ✅ | Implemented. |
| Compression | ✅ | Implemented. |
| Brotli compression | ✅ | Available through `compression-br`. |
| Rate limiting | ✅ | Implemented. |
| Sessions | ✅ | Implemented. |
| Observability middleware | ✅ | Implemented. |
| Security headers | ✅ | Implemented via `security-headers`. |
| JWT auth | ❌ | Not implemented. |
| Basic auth | ❌ | Not implemented. |
| Bearer auth | ❌ | Not implemented. |
| API key auth | ❌ | Not implemented. |
| CSRF | ❌ | Not implemented. |
| Upload progress | ❌ | Not implemented. |
| Idempotency keys | ❌ | Not implemented. |

## Request Helpers / Extraction Model

Harrow intentionally uses explicit request helpers rather than extractor-heavy
handler signatures. See [Request Helpers](./request-helpers.md) and
[Explicit Extractors](./explicit-extractors.md).

| Feature | Status | Notes |
| --- | --- | --- |
| Path params | ✅ | `req.param("name")`. |
| Query params | ✅ | `req.query_param`, `req.query_pairs`. |
| Headers | ✅ | `req.header`, `req.headers`. |
| Body bytes | ✅ | `req.body_bytes`. |
| JSON body | ✅ | `json` feature, `req.body_json`. |
| MessagePack body | ✅ | `msgpack` feature, `req.body_msgpack`. |
| Application state | ✅ | `req.require_state`, `req.try_state`. |
| Request extensions | ✅ | `set_ext`, `ext`, `require_ext`. |
| Form helper | ❌ | Not implemented. |
| Cookies helper | ❌ | Not implemented. |
| Multipart | ❌ | Not implemented. |
| JWT claims helper | ❌ | Not implemented. |
| API key helper | ❌ | Not implemented. |
| `Accept` parser | ❌ | Not implemented. |
| `Range` parser | ❌ | Not implemented. |
| Protobuf helper | ❌ | Not implemented. |
| Large extractor ecosystem | ❌ | Not the current design direction. |

## Performance Paths

| Feature | Status | Notes |
| --- | --- | --- |
| Custom HTTP/1 transport | ✅ | Implemented and retained as a reference/advanced-performance path; production-stable status depends on hardening, fuzzing, and benchmark evidence. |
| Hyper + thread-per-core backend | 🟡 | First prototype implemented; benchmark harness integration and optimization still pending. |
| JSON buffer reuse | ✅ | Harrow has thread-local JSON buffer/capacity work. |
| SIMD JSON | ❌ | Not implemented. Evaluate only with benchmarks. |
| Zero-copy extractors | ❌ | Not implemented. |
| Compression | ✅ | Compression middleware exists. |
| zstd | ❌ | Not implemented. |
| jemalloc/mimalloc benchmark paths | 🟡 | Present in benchmark/build tooling, not a broad application API. |
| HTTP/3 performance path | ❌ | Not implemented. |
| Runtime matrix benchmarks | 🟡 | Tooling exists; needs fresh post-refactor runs before new claims. |

## Realtime

| Feature | Status | Notes |
| --- | --- | --- |
| Streaming responses | ✅ | Low-level primitive exists. |
| WebSocket | 🟡 | Tokio feature exists. |
| SSE | 🟡 | Possible via streaming; helper/example still needed. |
| GraphQL subscriptions | ❌ | Not first-party. Could be an integration example later. |
| HTTP/3 | ❌ | Not implemented. |
| WebTransport | ❌ | Not implemented. |

## Docs and API Surface

| Feature | Status | Notes |
| --- | --- | --- |
| Docs index | ✅ | `docs/index.md`. |
| Product identity | ✅ | `docs/what-is-harrow.md`. |
| Roadmap/status | ✅ | `docs/roadmap.md`. |
| Backend support matrix | ✅ | `docs/backend-support.md`. |
| Lifecycle docs | ✅ | `docs/server-lifecycle.md`. |
| Deployment docs | ✅ | `docs/deployment.md`. |
| Request helper docs | ✅ | `docs/request-helpers.md`. |
| Observability docs | ✅ | `docs/observability.md`. |
| Security docs | ✅ | `docs/security.md`. |
| Performance docs | ✅ | `docs/performance.md`. |
| Verification docs | ✅ | `docs/verification.md`. |
| OpenAPI | 🟡 | Route metadata/OpenAPI feature exists, but not a complete utoipa/vespera story. |
| GraphiQL | ❌ | Not implemented. |
| Example suite | 🟡 | Examples exist; more production-pattern examples are needed. |

## Most Relevant Missing Features

Likely pre-1.0 polish candidates:

- feature-combination verification;
- Hyper + thread-per-core Tokio backend prototype and benchmark comparison;
- HTTP/2 support/parity across the chosen 1.0 server backends;
- backend wording audit for TLS, WebSocket, HTTP/2, and custom-H1 stability;
- examples for security headers, graceful shutdown, observability, request helpers, and HTTP/2;
- SSE helper or example;
- typed `param` / `query_param` parse helpers;
- fresh runtime matrix benchmark.

Likely post-1.0 candidates:

- Basic/Bearer/API-key/JWT auth middleware;
- CSRF middleware;
- multipart/form/cookie helpers;
- `Accept` and `Range` helpers;
- PROXY protocol;
- Unix sockets;
- HTTP/3/QUIC research;
- SIMD JSON research;
- static files.
