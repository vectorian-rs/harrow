# Security

This page collects Harrow's production security posture and security-related
middleware. Harrow keeps these features opt-in so applications can choose the
right policy for their deployment.

## Current Security-Relevant Features

- request body limits;
- header/body/connection timeouts in server config;
- CORS middleware;
- request ID middleware;
- rate limiting middleware;
- session middleware;
- catch-panic middleware;
- compression middleware;
- security headers middleware.

## Security Headers

Enable the feature:

```toml
harrow = { version = "0.10", features = ["tokio", "security-headers"] }
```

Use the default conservative policy:

```rust,ignore
let app = App::new()
    .middleware(harrow::security_headers_middleware(
        harrow::SecurityHeadersConfig::default(),
    ));
```

The default policy sets:

- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- `Referrer-Policy: no-referrer`
- `Permissions-Policy: camera=(), microphone=(), geolocation=()`

It does not set `Content-Security-Policy` or `Strict-Transport-Security` by
default because those are application/deployment-specific.

Example with explicit CSP and HSTS:

```rust,ignore
let config = harrow::SecurityHeadersConfig::default()
    .content_security_policy("default-src 'self'")
    .strict_transport_security("max-age=31536000; includeSubDomains");

let app = App::new().middleware(harrow::security_headers_middleware(config));
```

Only enable HSTS when the service is always reachable through HTTPS for the
covered hostnames.

## CORS

Use CORS middleware when browsers call your service across origins. Avoid using
wildcard origins with credentials. Prefer an explicit origin allowlist for
cookie/session-backed APIs.

## Body Limits and Timeouts

Body limits and read timeouts protect services from unbounded memory use and
slow clients. See [Server Lifecycle](../server/lifecycle.md) for defaults.

## Rate Limiting

Rate limiting is useful for public APIs and expensive routes. Choose keying
carefully: IP-only limits can be misleading behind proxies unless forwarded
headers are trusted and normalized.

## Sessions and CSRF

Session middleware exists, but CSRF policy should be application-specific until
Harrow grows first-class CSRF middleware. Browser form/cookie applications
should add CSRF protection before accepting unsafe cross-site requests.

## Reverse Proxies

If Harrow runs behind a proxy/load balancer:

- clearly define the trusted proxy boundary;
- decide where TLS terminates;
- normalize forwarded headers at the boundary;
- avoid trusting client-supplied forwarding headers directly in handlers.
