# Framework comparison: Harrow, axum, ntex, Tako

The same small API, modelled on the celeratax backend, written four times.
Each version is idiomatic for its framework and passes the same black-box
contract test over real TCP. Business logic, validation, and error shapes live
once in `src/domain.rs`, so the four `*_app.rs` files contain only HTTP-layer
code and can be compared directly.

- Spec: [SPEC.md](./SPEC.md)
- Shared code: [src/domain.rs](./src/domain.rs)
- Implementations: [harrow](./src/harrow_app.rs), [axum](./src/axum_app.rs),
  [ntex](./src/ntex_app.rs), [tako](./src/tako_app.rs)
- Contract test: [tests/contract.rs](./tests/contract.rs)

## Run it

This is a standalone cargo workspace. Tako 2.4 needs Rust 1.95+, so the
directory pins Rust 1.99.1 in its own `mise.toml`; Harrow's workspace and lock
file are not affected.

```bash
cd examples/framework-compare
cargo test                                # 4 contract tests + 4 in-process tests
ADDR=127.0.0.1:3000 cargo run --bin compare-harrow   # or compare-axum / -ntex / -tako
curl -H 'cookie: ct_session=sess-alice' localhost:3000/api/v1/companies
```

Versions: Harrow 0.10 (path), axum 0.8.9 + tower-http 0.6, ntex 4.3.0,
tako-rs 2.4.1.

## What the API covers

Taken from the celeratax HTTP layer, in order of how often celeratax uses
each:

- Request id from CloudFront or generated, echoed as `x-request-id`, one
  access-log line per request.
- Error envelope `{"error":{"code","message","request_id"}}` with
  `cache-control: no-store` and `www-authenticate` on 401.
- Cookie session auth, reused on every `/api/v1` route; admin routes hidden
  behind 404.
- Same-origin check on every POST (exactly one `Origin`).
- Path parameters with domain validation, raw query-string lookup.
- Size-limited JSON body with content-type check and unknown-field rejection.
- Private JSON responses (`no-store`, `vary: cookie`), a PDF download, a 303
  redirect with two `Set-Cookie` headers.
- JSON 404 fallback for unknown paths and wrong methods.
- Graceful shutdown on SIGTERM and Ctrl-C.

## Results

| | Harrow | axum | Tako | ntex |
|---|---:|---:|---:|---:|
| Code lines (no comments, blanks, tests) | **176** | 191 | 236 | 243 |
| Trait impls you write | **0** | 4 | 6 | 9 |
| Contract test | pass | pass | pass | pass |

### How each one reads

**Harrow.** Handlers are `async fn(Request) -> Result<Response, ApiError>`.
Session and origin checks are group middleware that put an
`AuthenticatedSession` into request extensions; handlers read it back. This is
the same shape celeratax already hand-rolls on top of axum, so a port is
mostly mechanical. Weak spots: handlers fetch state and session with
`expect()`, so forgetting a middleware is a runtime panic, not a compile
error; and errors need a small `api_fn` wrapper to get the request id (see
gaps below).

**axum.** The most self-documenting signatures: a handler that takes
`session: AuthenticatedSession` cannot be called without one. Errors are
`IntoResponse`, tagged in response extensions and rendered once by middleware.
Weak spots: `axum::serve` has no header-read timeout or connection cap, which
is exactly why celeratax wrote its own hyper accept loop; `Json` and `Query`
rejections are plain text, so the version uses custom extractors and
`RawQuery` instead.

**Tako.** Looks like axum, but a type used as both a last and a non-last
handler argument needs two extractor impls, extractors declare `ENTRIES`
constants, and Tako's own rejections (bad path, 405, 413) are plain text, so
the middleware rewrites them by status code. A missing `State` compiles and
fails at runtime.

**ntex.** The strongest compile-time guarantees: filters change the request
type (`WebRequest<()>` to `WebRequest<AuthenticatedSession>` to
`WebRequest<Admin>`), so an admin handler cannot be mounted without both
checks. The cost is the most boilerplate: a hand-written `Service` for the
request-id layer, an uninhabited error-domain type, and a newtype because the
error must implement `std::error::Error`. Web middleware cannot see requests
rejected by a filter, and SIGINT is not graceful by default.

### Which is cleaner

For this workload, Harrow and axum are close and clearly ahead of Tako and
ntex. Harrow is the shortest and has the fewest concepts. axum gives better
compile-time wiring. ntex buys the most safety for the most code. Tako adds
axum-like code plus workarounds without adding guarantees.

## Maturity

Measured October 2026.

| | Harrow | axum | Tako | ntex |
|---|---|---|---|---|
| Version | 0.10 | 0.8 | 2.4 (2.0 in May 2026) | 4.3 (4.0 on 2026-10-08) |
| Maintainers | us | tokio-rs team | 1 (506 of 508 commits) | 1 main (4,694 commits; next has 57) |
| Downloads | small | very high | ~5k total | ~1.1M total |
| Issues hit while building this | 405 status, no response extensions | none | no test client, shutdown waits forever if the server dies | 4.3.0 fails to build without `ws`; docs name a method that was renamed |

## Harrow gaps found

These blocked a clean port or forced a workaround. Fix them before migrating
production services.

1. **`Response` has no extensions.** axum, Tako, and ntex all solve "the error
   body needs the request id" by tagging the error response and rendering it
   in one middleware. Harrow cannot, so every handler goes through an
   `api_fn` adapter that boxes the future. Adding
   `Response::extensions()`/`extensions_mut()` removes the adapter.
2. **Custom 405 handler cannot choose its status.** `dispatch.rs` forces 405
   onto whatever `method_not_allowed_handler` returns. celeratax answers a
   wrong method with 404.
3. **`serve_multi_worker` has no shutdown hook.** Only the single-worker
   `serve_with_config` can drain on SIGTERM, so multi-core deployment and
   graceful shutdown are currently exclusive.
4. **Over the connection cap, Harrow drops the socket.** celeratax writes a
   raw 503 with a JSON body first so CloudFront and clients see a clear
   overload signal.
5. **Test client ergonomics.** `App::client()` works, but setting a header
   needs a full `http::Request` builder. A `.get(path).header(..)` style
   builder would match celeratax's `TestCtx`.

The `tls` feature on `harrow-server-tokio` is declared but not wired into the
custom HTTP/1 server. celeratax does not need it (CloudFront terminates TLS),
but the feature should either work or be removed.

## Migrating celeratax and datadeft-auth

**celeratax** uses axum only as a catch-all fallback plus one `from_fn`
middleware; routing is a hand-written `match (method, path)` over 49 routes,
and auth and checks are explicit function calls. That maps onto Harrow almost
one to one:

| celeratax today | Harrow |
|---|---|
| `match (&Method, &str)` per route group + `route_paths.rs` matchers | `App::group("/api/v1/...", ...)` with `:param` routes; keep the format checks as `domain::validate_*` |
| `request_id_middleware` (`from_fn`) | global middleware, as in `harrow_app.rs` |
| `authenticate(&request, state).await?` in handlers | `require_session` group middleware, or keep the explicit call |
| `read_limited_body` + timeout | `App::max_body_size` + `ServerConfig::body_read_timeout`, then `req.body_bytes()` |
| custom hyper accept loop, `Semaphore(512)`, 5 s header timeout | `ServerConfig { max_connections, header_read_timeout, .. }` (needs gap 4 for the 503) |
| `PublicError::into_response(request_id)` | same, until gap 1 lands |
| `router(state).oneshot(req)` tests | `app(state).client().request(req)` |

**datadeft-auth** is already mostly framework-neutral. `authenticate_session`
and `admit_proof` take `&http::HeaderMap`, which is the type
`Request::headers()` returns, so Harrow can call them unchanged. Five
functions take or return axum's `Request`/`Response`:
`handle_magic_link_request_json`, `magic_link_landing`,
`magic_link_confirmation`, `magic_link_landing_redirect`, and
`magic_link_flow_state`. Split each into a core that takes
`(&http::request::Parts, Bytes)` and returns `http::Response<Bytes>`; the axum
adapter keeps its current API and a `datadeft-magic-link-harrow` adapter
becomes a few lines per function.

### Suggested order

1. Fix Harrow gaps 1 to 4 (each is small; 3 is the largest).
2. Add the framework-neutral core to datadeft-auth and a Harrow adapter crate.
3. Port celeratax's `backends/api` route group by group behind the existing
   insta snapshot tests, which already pin the response contract.

## Recommended Harrow setup for these services

- Features: `tokio` and `json`. Add `o11y` only if you want OTLP traces;
  celeratax logs JSON to CloudWatch, which plain `tracing-subscriber` covers.
- Backend: the Tokio custom HTTP/1 backend. Not Monoio or meguri: containers
  often block io_uring, and the services are I/O-bound on AWS calls, not on
  the HTTP stack.
- Server: `serve_with_config` with explicit `max_connections`,
  `header_read_timeout`, `body_read_timeout`, and `max_body_size`, and
  SIGTERM as the shutdown future. Move to `serve_multi_worker` once gap 3 is
  fixed.
