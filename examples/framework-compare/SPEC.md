# Framework comparison spec

One small API, shaped after the celeratax backend, implemented four times:
Harrow, axum, ntex, Tako. Every implementation lives in its own file under
`src/` and uses only the shared business code in `src/domain.rs`. The point is
to compare the *HTTP layer* code, so all business logic, validation, and error
shapes live in `domain.rs` and must not be duplicated.

The black-box test in `tests/contract.rs` runs the same requests against all
four servers over real TCP. An implementation is done when it passes.

## Server

- `pub fn run(addr: SocketAddr, store: domain::Store)` in each
  `src/<framework>_app.rs`. Blocks the calling thread, builds its own runtime,
  binds `addr`, and serves until SIGTERM or Ctrl-C, then shuts down
  gracefully. The contract test polls the port until it accepts connections.
- Use the framework's production-recommended server setup (worker model,
  timeouts, connection limits) and say in a short module comment what you
  chose and why.
- HTTP/1.1, plain TCP. No TLS (celeratax sits behind CloudFront).
- Request bodies over `domain::MAX_JSON_BODY` (4 KiB) are rejected with 400
  `bad_request` (or the framework's own 413 if it rejects earlier; the contract
  accepts 400 or 413).

## Cross-cutting behaviour (every response, including errors and 404)

1. **Request id.** If the request has `cloudfront-viewer-http-request-id`,
   use that value; otherwise generate one with `domain::new_request_id()`.
   Echo it in the `x-request-id` response header.
2. **Access log.** One `tracing::info!` per request with `request_id`,
   `method`, `status`, `latency_ms`. Never log the query string or cookies.
3. **Error envelope.** Every error is produced by `domain::ApiError` and
   rendered with `ApiError::body(request_id)`, `ApiError::status()` and
   `ApiError::www_authenticate()`. Errors always carry
   `cache-control: no-store` and `content-type: application/json`.

## Auth

- Session cookie `ct_session`. Read the `Cookie` header and call
  `domain::session_token(cookie_header)`, then `store.authenticate(token)`.
- Failure: 401 `session_required` with `www-authenticate: DD-Session`.
- Write the session check once (extractor, middleware, or helper - whatever is
  idiomatic in the framework) and reuse it on every authenticated route.
- **Same-origin check** on every POST: exactly one `Origin` header equal to
  `domain::EXPECTED_ORIGIN`, else 403 `forbidden`. Use
  `domain::same_origin(headers)`.

## Routes

| Method | Path | Auth | Behaviour |
|---|---|---|---|
| GET | `/healthz` | no | 200 `text/plain` body `ok` |
| GET | `/api/v1/session/current` | session | 200 JSON `domain::SessionView` |
| GET | `/api/v1/companies` | session | 200 JSON `Vec<domain::Company>` for the user, private headers |
| POST | `/api/v1/companies` | session + origin | body JSON `domain::CreateCompany`; content type must be `application/json` (else 400); 201 JSON `domain::Company` |
| GET | `/api/v1/companies/{company_id}/months/{month}/state` | session | validate with `domain::validate_company_id` / `validate_month`; user must have access (`store.company_for_user`, else 404 `not_found`); 200 JSON `domain::MonthState` |
| GET | `/api/v1/companies/{company_id}/documents/{file_id}/content` | session | validate ids (`validate_file_id`); 200 `application/pdf` body from `store.document`, headers `content-disposition: inline; filename="<file_id>.pdf"`, `cache-control: private, no-store`, `x-content-type-options: nosniff` |
| GET | `/api/v1/usage?period=YYYY-MM` | session | `period` query param required and valid (`validate_month`), else 400; 200 JSON `domain::Usage` |
| POST | `/api/v1/session/logout` | session + origin | `store.revoke`; 303 `location: /`; **two** `set-cookie` headers: `domain::CLEAR_SESSION_COOKIE` and `domain::CLEAR_POW_COOKIE` |
| GET | `/api/v1/admin/users` | session + admin | `domain::is_admin(email)` else 404 `not_found` (hide the route); 200 JSON list of emails |
| any | anything else | no | 404 `not_found` envelope (a wrong method on a known path may be 404 or 405) |

"Private headers" means `cache-control: no-store` and `vary: cookie` on the
success response (`domain::PRIVATE_HEADERS`).

## Tests

Besides the shared contract test, each `*_app.rs` should have one in-process
test (no socket) of an authenticated route, using whatever the framework
offers for that. This shows what testing looks like in each framework.
