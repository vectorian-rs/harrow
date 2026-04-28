# Auth Middleware Design Document

## Overview

This document surveys authentication middleware patterns across Rust HTTP frameworks
(axum, actix-web, tower-http) and proposes concrete designs for harrow. Harrow uses a
custom `Middleware` trait (not Tower), so all patterns are adapted to its
`fn call(&self, req: Request, next: Next) -> BoxFuture` signature.

The goal is to provide composable, feature-gated auth building blocks in
`harrow-middleware` that follow harrow's existing conventions: opt-in via Cargo
features, zero overhead when unused, and no mandatory dependencies.

---

## 1. Auth Middleware Patterns Across Rust Frameworks

### 1.1 Axum: Extractor-Based Auth

Axum's primary pattern uses `FromRequestParts` to extract auth data directly in
handlers rather than running a separate middleware layer:

```rust
// axum pattern: implement FromRequestParts for your claims type
#[async_trait]
impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let bearer = parts.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AuthError::MissingToken)?;

        let token_data = decode::<Claims>(bearer, &DECODING_KEY, &Validation::default())
            .map_err(|_| AuthError::InvalidToken)?;

        Ok(token_data.claims)
    }
}

// handler just declares what it needs
async fn protected(claims: Claims) -> impl IntoResponse {
    format!("hello {}", claims.sub)
}
```

Axum also supports `middleware::from_fn` for cases where auth must run before routing
or needs to short-circuit globally:

```rust
async fn auth_middleware(req: Request, next: Next) -> Result<Response, StatusCode> {
    let token = req.headers().get("authorization")
        .ok_or(StatusCode::UNAUTHORIZED)?;
    // validate...
    Ok(next.run(req).await)
}
```

**Key insight**: Axum uses `req.extensions_mut().insert(user)` to pass authenticated
identity from middleware to handlers.

### 1.2 Actix-Web: Guards and Middleware

Actix-web provides three mechanisms:

1. **Guards**: synchronous predicates that gate route access (no async, no state mutation)
2. **Middleware**: `Transform`/`Service` traits for request/response processing
3. **Extractors**: `FromRequest` for pulling auth data into handlers

The `actix-web-httpauth` crate wraps these into a validator function pattern:

```rust
// actix-web pattern
async fn validator(
    req: ServiceRequest,
    credentials: BearerAuth,
) -> Result<ServiceRequest, (Error, ServiceRequest)> {
    if validate_token(credentials.token()).is_ok() {
        Ok(req)
    } else {
        Err((ErrorUnauthorized("invalid token"), req))
    }
}

App::new()
    .wrap(HttpAuthentication::bearer(validator))
    .service(protected_resource)
```

**Key insight**: Actix separates the credential extraction (Bearer, Basic) from
validation logic, making it pluggable.

### 1.3 Tower-HTTP: Layer-Based Auth

Tower provides auth through composable layers:

```rust
// tower-http pattern
use tower_http::auth::RequireAuthorizationLayer;

let app = Router::new()
    .layer(RequireAuthorizationLayer::bearer("my-secret-token"));
```

For JWT specifically, `tower-oauth2-resource-server` provides OIDC-aware middleware
that validates JWTs against JWKS endpoints.

**Key insight**: Tower's strength is composability -- auth layers can be stacked
and applied to specific route groups via `ServiceBuilder`.

### 1.4 Mapping to Harrow

Harrow's `Middleware` trait maps most closely to axum's `from_fn` pattern. The
`Request::try_state<T>()` and `Request::require_state<T>()` methods serve the same
role as axum's `Extension` extractor -- middleware can read configuration from app
state, and handlers can read auth results stored by middleware.

Unlike axum, harrow does not have an extractor system. Auth in harrow is:
- **Middleware**: for global or group-scoped enforcement (short-circuits with 401/403)
- **Handler-level**: for per-route decisions using `req.try_state::<AuthUser>()`

---

## 2. JWT Validation Middleware

### 2.1 Typical Flow

```
Request
  |
  v
[Extract token from Authorization header or cookie]
  |
  v
[Decode JWT header (alg, kid)]
  |
  v
[Select verification key (static key or JWKS lookup by kid)]
  |
  v
[Verify signature + validate claims (exp, nbf, iss, aud)]
  |
  v
[Attach claims to request state / short-circuit on failure]
  |
  v
Handler
```

### 2.2 Crate Choices

| Crate | Downloads | Key Characteristics |
|-------|-----------|-------------------|
| `jsonwebtoken` | 12M+ | Simple API, low-level control, HS/RS/ES/EdDSA, `Validation` struct for claim checks |
| `jwt-simple` | 1M+ | Higher-level API, key types own sign/verify methods, auto-validates exp/nbf, WASM support |

**Recommendation for harrow**: `jsonwebtoken` is the better fit because:
- Lower-level control aligns with harrow's philosophy of explicit, minimal abstractions
- Smaller dependency tree
- `Validation` struct gives fine-grained control over which claims to check
- `DecodingKey` and `EncodingKey` are designed for reuse (store in app state)

### 2.3 Proposed Harrow JWT Middleware

```rust
use std::sync::Arc;
use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::de::DeserializeOwned;

/// Configuration for JWT validation middleware.
pub struct JwtConfig<C: DeserializeOwned + Send + Sync + 'static> {
    /// The decoding key (shared secret or public key).
    pub decoding_key: DecodingKey,
    /// Validation rules (algorithms, issuer, audience, etc.).
    pub validation: Validation,
    /// Phantom data for claims type.
    _claims: std::marker::PhantomData<C>,
}

impl<C: DeserializeOwned + Send + Sync + 'static> JwtConfig<C> {
    pub fn new(decoding_key: DecodingKey, validation: Validation) -> Self {
        Self {
            decoding_key,
            validation,
            _claims: std::marker::PhantomData,
        }
    }
}

/// Validated JWT claims, inserted into request state by the JWT middleware.
/// Handlers access via `req.require_state::<JwtClaims<MyClaims>>()`.
pub struct JwtClaims<C>(pub C);

/// JWT authentication middleware.
///
/// Reads `Arc<JwtConfig<C>>` from application state.
/// On success, inserts `JwtClaims<C>` into request state (via a per-request
/// TypeMap or a dedicated field -- see Section 6 for options).
///
/// On failure, returns 401 Unauthorized.
pub async fn jwt_middleware<C>(req: Request, next: Next) -> Response
where
    C: DeserializeOwned + Send + Sync + 'static,
{
    let config = match req.try_state::<Arc<JwtConfig<C>>>() {
        Some(c) => c.clone(),
        None => return Response::new(http::StatusCode::INTERNAL_SERVER_ERROR, "jwt not configured"),
    };

    let token = match extract_bearer_token(&req) {
        Some(t) => t,
        None => return Response::new(http::StatusCode::UNAUTHORIZED, "missing authorization"),
    };

    match decode::<C>(&token, &config.decoding_key, &config.validation) {
        Ok(token_data) => {
            // TODO: attach token_data.claims to request (see Section 6)
            next.run(req).await
        }
        Err(_) => Response::new(http::StatusCode::UNAUTHORIZED, "invalid token"),
    }
}

fn extract_bearer_token(req: &Request) -> Option<String> {
    req.header("authorization")?
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}
```

### 2.4 Usage

```rust
use harrow::{App, Response, Request};
use std::sync::Arc;
use jsonwebtoken::{DecodingKey, Validation, Algorithm};

#[derive(Debug, serde::Deserialize)]
struct MyClaims {
    sub: String,
    exp: u64,
    role: String,
}

let jwt_config = JwtConfig::<MyClaims>::new(
    DecodingKey::from_secret(b"my-secret-key"),
    Validation::new(Algorithm::HS256),
);

let app = App::new()
    .state(Arc::new(jwt_config))
    .group("/api", |g| {
        g.middleware(jwt_middleware::<MyClaims>)
         .get("/profile", profile_handler)
    })
    .get("/public", public_handler);
```

### 2.5 JWKS Support (Future)

For OIDC/multi-tenant scenarios, a `JwksConfig` variant would:
1. Cache the JWKS keyset in app state (behind `Arc<RwLock<JwkSet>>`)
2. Periodically refresh from the provider's `/.well-known/openid-configuration`
3. Select the decoding key by matching the JWT's `kid` header claim

This is best implemented as a separate feature (`jwt-jwks`) with a dependency on
`reqwest` and `tokio` for background refresh.

---

## 3. Session-Based Auth Middleware

### 3.1 Architecture

Session-based auth follows this pattern:

```
Request
  |
  v
[Read session cookie from request]
  |
  v
[Load session data from store (Redis, DB, in-memory)]
  |
  v
[Attach Session handle to request]
  |
  v
Handler (reads/writes session)
  |
  v
[If session modified: save to store + set/update cookie]
  |
  v
Response
```

### 3.2 Key Components

**Session ID**: A cryptographically random identifier (128-bit, generated via `rand`
or `getrandom`). Stored in a signed cookie (HMAC-SHA256 with server secret) to
prevent tampering.

**SessionStore trait**:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Load session data by ID. Returns None if expired or not found.
    async fn load(&self, id: &str) -> Result<Option<SessionData>, SessionError>;

    /// Save session data. Creates or updates.
    async fn save(&self, id: &str, data: &SessionData) -> Result<(), SessionError>;

    /// Delete a session.
    async fn delete(&self, id: &str) -> Result<(), SessionError>;
}
```

**Cookie handling**: The middleware manages `Set-Cookie` headers on the response.
Key attributes:
- `HttpOnly`: prevents JavaScript access
- `Secure`: HTTPS only
- `SameSite=Lax` or `Strict`: CSRF protection
- `Max-Age` / `Expires`: session lifetime
- `Path=/`: scope

### 3.3 Proposed Session Middleware for Harrow

```rust
pub struct SessionConfig {
    /// Cookie name (default: "sid")
    pub cookie_name: String,
    /// HMAC key for cookie signing
    pub signing_key: [u8; 32],
    /// Session TTL
    pub ttl: Duration,
    /// Cookie attributes
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

pub struct SessionMiddleware<S: SessionStore> {
    store: Arc<S>,
    config: Arc<SessionConfig>,
}
```

The session middleware is heavier than JWT (requires a store + async I/O), so it
would live behind a `session` feature gate with dependencies on `cookie` (cookie
parsing/signing), `rand` (ID generation), and optionally `redis` or similar for
the store backend.

### 3.4 Session vs JWT Tradeoffs

| Aspect | Session | JWT |
|--------|---------|-----|
| State | Server-side (store required) | Stateless (token is self-contained) |
| Revocation | Immediate (delete from store) | Requires blocklist or short TTL |
| Scalability | Store must be shared across instances | No shared state needed |
| Payload size | Cookie is just an ID (~50 bytes) | Token can be large (1-4KB) |
| CSRF | Vulnerable (cookie auto-sent) | Not vulnerable if using Authorization header |

### 3.5 Recommendation

Session support is a larger undertaking than JWT middleware and depends on
choices about the store backend. Recommend shipping JWT middleware first as it
is stateless and self-contained, then adding session support as a separate feature.

---

## 4. API Key Middleware

### 4.1 Pattern

API key auth is the simplest auth pattern: extract a key from a header (or query
parameter), compare against known keys, attach identity to request.

```
Request
  |
  v
[Extract API key from header (e.g., X-API-Key, Authorization: ApiKey ...)]
  |
  v
[Look up key in key store (HashMap, database)]
  |
  v
[Attach key metadata/identity to request]
  |
  v
Handler
```

### 4.2 Proposed API Key Middleware

```rust
use std::sync::Arc;
use subtle::ConstantTimeEq;
use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

/// Where to look for the API key.
pub enum ApiKeyLocation {
    /// Header name (e.g., "x-api-key")
    Header(String),
    /// Query parameter name (e.g., "api_key")
    QueryParam(String),
}

/// Result of a successful API key lookup, inserted into request state.
pub struct ApiKeyIdentity {
    pub key_id: String,
    pub metadata: Option<String>,
}

/// Trait for looking up API keys. Implement for your storage backend.
pub trait ApiKeyStore: Send + Sync + 'static {
    /// Look up an API key. Returns identity metadata if valid.
    /// Implementations MUST use constant-time comparison for the key value.
    fn lookup(&self, key: &str) -> Option<ApiKeyIdentity>;
}

/// Simple in-memory API key store.
pub struct StaticApiKeyStore {
    /// Maps key hash -> identity. Keys are stored as SHA-256 hashes
    /// to avoid holding plaintext keys in memory.
    keys: std::collections::HashMap<[u8; 32], ApiKeyIdentity>,
}

impl StaticApiKeyStore {
    pub fn new() -> Self {
        Self { keys: std::collections::HashMap::new() }
    }

    pub fn add_key(&mut self, raw_key: &str, identity: ApiKeyIdentity) {
        let hash = sha256(raw_key.as_bytes());
        self.keys.insert(hash, identity);
    }
}

impl ApiKeyStore for StaticApiKeyStore {
    fn lookup(&self, key: &str) -> Option<ApiKeyIdentity> {
        let hash = sha256(key.as_bytes());
        // HashMap lookup by hash is constant-time w.r.t. the key value
        // because we compare hashes, not raw keys.
        self.keys.get(&hash).map(|id| ApiKeyIdentity {
            key_id: id.key_id.clone(),
            metadata: id.metadata.clone(),
        })
    }
}

pub struct ApiKeyMiddleware {
    location: ApiKeyLocation,
    store: Arc<dyn ApiKeyStore>,
}

impl ApiKeyMiddleware {
    pub fn new(location: ApiKeyLocation, store: Arc<dyn ApiKeyStore>) -> Self {
        Self { location, store }
    }
}

impl harrow_core::middleware::Middleware for ApiKeyMiddleware {
    fn call(
        &self,
        req: Request,
        next: Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response>>> {
        let store = Arc::clone(&self.store);
        let location = self.location.clone();

        Box::pin(async move {
            let key = match &location {
                ApiKeyLocation::Header(name) => req.header(name).map(|s| s.to_string()),
                ApiKeyLocation::QueryParam(name) => req.query_param(name),
            };

            let key = match key {
                Some(k) if !k.is_empty() => k,
                _ => return Response::new(http::StatusCode::UNAUTHORIZED, "missing api key"),
            };

            match store.lookup(&key) {
                Some(_identity) => {
                    // TODO: attach identity to request (see Section 6)
                    next.run(req).await
                }
                None => Response::new(http::StatusCode::UNAUTHORIZED, "invalid api key"),
            }
        })
    }
}
```

### 4.3 Usage

```rust
let mut key_store = StaticApiKeyStore::new();
key_store.add_key("sk_live_abc123", ApiKeyIdentity {
    key_id: "client-1".into(),
    metadata: Some("production".into()),
});

let app = App::new()
    .group("/api", |g| {
        g.middleware(ApiKeyMiddleware::new(
            ApiKeyLocation::Header("x-api-key".into()),
            Arc::new(key_store),
        ))
        .get("/data", data_handler)
    });
```

---

## 5. OAuth2/OIDC Middleware Patterns

### 5.1 Two Distinct Use Cases

OAuth2/OIDC has two very different middleware patterns depending on the role:

**A. Resource Server (API)**: Validates incoming JWT access tokens against the
identity provider's public keys. This is essentially JWT middleware with JWKS
auto-discovery.

**B. Relying Party (Web App)**: Implements the full OAuth2 authorization code flow
with redirects, callback handling, and session management.

### 5.2 Resource Server Pattern

```
Request with Authorization: Bearer <JWT>
  |
  v
[JWT middleware with JWKS backend]
  |
  v
[Validate: signature, exp, iss, aud, scope]
  |
  v
Handler (has access to verified claims)
```

This is a natural extension of the JWT middleware from Section 2, with:
- JWKS endpoint auto-discovery via `/.well-known/openid-configuration`
- Key caching with background refresh (typically every 5-15 minutes)
- `kid` (Key ID) header matching for key rotation support

**Crate choices**:
- `openidconnect`: Full OIDC client library, strongly typed, handles discovery
- `jsonwebtoken` + manual JWKS fetch: lighter weight, more control
- `tower-oauth2-resource-server`: Tower-specific, not directly usable in harrow

### 5.3 Relying Party Pattern

```
GET /login
  |
  v
[Generate state + nonce, store in session]
  |
  v
[Redirect to provider's /authorize endpoint]
  |
  v
[User authenticates at provider]
  |
  v
GET /callback?code=...&state=...
  |
  v
[Validate state against session (CSRF protection)]
  |
  v
[Exchange code for tokens at provider's /token endpoint]
  |
  v
[Validate ID token, extract user info]
  |
  v
[Create session, set cookie, redirect to app]
```

This is not middleware but a set of **handlers** (`/login`, `/callback`, `/logout`)
that use the `openidconnect` crate and session middleware. The auth check itself is
then session-based middleware.

### 5.4 Recommendation for Harrow

1. **Phase 1**: Ship JWT middleware (Section 2) -- covers resource server pattern
2. **Phase 2**: Add JWKS support as `jwt-jwks` feature -- completes OIDC resource
   server use case
3. **Phase 3**: If there is demand, add session middleware + OAuth2 handler helpers
   for the relying party pattern

---

## 6. Request State / Extensions Interaction

### 6.1 The Problem

Auth middleware validates credentials and produces an identity (claims, user info,
API key metadata). Handlers need to access this identity. How does the middleware
pass data to the handler?

### 6.2 How Other Frameworks Do It

| Framework | Mechanism |
|-----------|-----------|
| axum | `req.extensions_mut().insert(claims)` / `Extension<Claims>` extractor |
| actix-web | `req.extensions_mut().insert(claims)` / `web::ReqData<Claims>` |
| tower | Generic `Request<B>` extensions via `http::Extensions` |

All three use the same underlying mechanism: `http::Extensions`, a type-map on the
request object.

### 6.3 Current Harrow State

Harrow currently has two levels of state:

1. **App state** (`TypeMap` via `App::state()`): Shared across all requests. Immutable
   after app construction. Accessed via `req.require_state::<T>()`.

2. **Per-request fields**: `request_id`, `route_pattern`, `max_body_size` -- hardcoded
   fields on `Request`.

There is no per-request type-map for middleware to insert arbitrary data.

### 6.4 Proposed Solutions

**Option A: Add per-request extensions to `Request`**

Add an `extensions: http::Extensions` (or a second `TypeMap`) field to `Request`:

```rust
pub struct Request {
    inner: http::Request<Body>,
    path_match: PathMatch,
    state: Arc<TypeMap>,        // shared app state (immutable)
    extensions: TypeMap,         // per-request data (mutable)
    route_pattern: Option<Arc<str>>,
    request_id: Option<String>,
    max_body_size: usize,
}

impl Request {
    /// Insert per-request data (used by middleware).
    pub fn set_ext<T: Send + Sync + 'static>(&mut self, val: T) {
        self.extensions.insert(val);
    }

    /// Get per-request data (used by handlers).
    pub fn ext<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.try_get::<T>()
    }

    /// Get per-request data, returning an error if missing.
    pub fn require_ext<T: Send + Sync + 'static>(&self) -> Result<&T, MissingExtError> {
        self.extensions.try_get::<T>().ok_or(MissingExtError {
            type_name: std::any::type_name::<T>(),
        })
    }
}
```

**Option B: Use `http::Extensions` from the inner request**

The underlying `http::Request<Body>` already has an `extensions()` method. Expose it:

```rust
impl Request {
    pub fn extensions(&self) -> &http::Extensions {
        self.inner.extensions()
    }

    pub fn extensions_mut(&mut self) -> &mut http::Extensions {
        self.inner.extensions_mut()
    }
}
```

This matches what axum, actix-web, and tower do, but `http::Extensions` has a
different API than harrow's `TypeMap` (uses `get::<T>()` not `try_get::<T>()`).

**Option C: Typed extension trait on Request**

Wrap `http::Extensions` behind harrow-style methods:

```rust
impl Request {
    pub fn set_ext<T: Send + Sync + 'static>(&mut self, val: T) {
        self.inner.extensions_mut().insert(val);
    }

    pub fn ext<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.inner.extensions().get::<T>()
    }
}
```

### 6.5 Recommendation

**Option C** is the best path:
- Uses the existing `http::Extensions` that is already part of the inner request
  (zero additional memory)
- Provides harrow-style ergonomic methods
- Compatible with any Tower middleware that might be adapted in the future
- Does not break the existing `Request` API

The JWT middleware example from Section 2 would then become:

```rust
match decode::<C>(&token, &config.decoding_key, &config.validation) {
    Ok(token_data) => {
        let mut req = req;
        req.set_ext(JwtClaims(token_data.claims));
        next.run(req).await
    }
    Err(_) => Response::new(http::StatusCode::UNAUTHORIZED, "invalid token"),
}
```

And handlers would access claims:

```rust
async fn profile(req: Request) -> Response {
    let claims = req.ext::<JwtClaims<MyClaims>>()
        .expect("jwt middleware must run before this handler");
    Response::text(format!("hello {}", claims.0.sub))
}
```

---

## 7. Middleware vs Extractor-Based Auth: Best Practices

### 7.1 When to Use Middleware

- **Global auth**: all routes (or a whole group) require authentication
- **Short-circuit**: reject unauthenticated requests before any handler logic runs
- **Cross-cutting**: logging, metrics, and tracing for auth failures
- **Multiple auth schemes**: try Bearer, then cookie, then API key in a chain

```rust
// harrow: group-scoped auth middleware
let app = App::new()
    .get("/health", health)                    // no auth
    .group("/api", |g| {
        g.middleware(jwt_middleware::<Claims>)  // all /api routes require JWT
         .get("/users", list_users)
         .get("/users/:id", get_user)
    });
```

### 7.2 When to Use Handler-Level Checks

- **Mixed auth**: some routes in a group are public, others need auth
- **Fine-grained authorization**: handler needs to check specific claims/roles
- **Optional auth**: enhance response if user is authenticated, but don't require it

```rust
// harrow: handler-level auth check
async fn maybe_personalized(req: Request) -> Response {
    if let Some(claims) = req.ext::<JwtClaims<MyClaims>>() {
        Response::text(format!("hello {}", claims.0.name))
    } else {
        Response::text("hello anonymous")
    }
}
```

### 7.3 Composability Patterns

**Layered auth**: Global middleware does authentication (who are you?), route-level
middleware or handlers do authorization (are you allowed?).

```rust
let app = App::new()
    .group("/api", |g| {
        g.middleware(jwt_middleware::<Claims>)     // authn for all /api
         .get("/public-data", public_data)         // any authenticated user
         .group("/admin", |admin| {
             admin.middleware(require_role("admin")) // authz for /api/admin
                  .get("/users", admin_list_users)
                  .delete("/users/:id", admin_delete_user)
         })
    });
```

**Multi-scheme auth**: A top-level middleware tries multiple schemes:

```rust
async fn multi_auth(mut req: Request, next: Next) -> Response {
    // Try JWT first
    if let Some(token) = extract_bearer_token(&req) {
        if let Ok(claims) = validate_jwt(&token) {
            req.set_ext(AuthIdentity::Jwt(claims));
            return next.run(req).await;
        }
    }

    // Fall back to API key
    if let Some(key) = req.header("x-api-key") {
        if let Some(identity) = lookup_api_key(key) {
            req.set_ext(AuthIdentity::ApiKey(identity));
            return next.run(req).await;
        }
    }

    Response::new(http::StatusCode::UNAUTHORIZED, "authentication required")
}
```

### 7.4 Harrow-Specific Recommendation

Since harrow does not have an extractor system, the pattern is:

1. **Auth middleware** runs early, validates credentials, calls `req.set_ext(identity)`
2. **Authorization helpers** are plain functions that read from `req.ext::<T>()`
3. **Handlers** call helpers or read extensions directly

This keeps the middleware trait simple and avoids needing a parallel extractor
type system.

---

## 8. Security Considerations

### 8.1 Timing Attacks

**Problem**: Naive string comparison leaks information about how many characters
of a token/key match, allowing attackers to reconstruct valid credentials byte by
byte.

**Mitigation for API keys**:

```rust
use subtle::ConstantTimeEq;

fn verify_key(provided: &[u8], expected: &[u8]) -> bool {
    // Returns 1 iff all bytes match, in constant time.
    provided.ct_eq(expected).into()
}
```

Alternatively, hash both values and compare hashes (the approach used in the
`StaticApiKeyStore` in Section 4):

```rust
fn verify_key_via_hash(provided: &str, expected_hash: &[u8; 32]) -> bool {
    let provided_hash = sha256(provided.as_bytes());
    provided_hash.ct_eq(expected_hash).into()
}
```

**Mitigation for JWT**: Not directly vulnerable to timing attacks because JWT
validation uses cryptographic signature verification (HMAC or RSA/EC), which is
inherently constant-time in reputable libraries (`ring`, `openssl`). However,
early rejection of malformed tokens (before signature check) is acceptable
because it does not leak information about valid tokens.

**Mitigation for password verification**: Always perform the full hash computation
even when the username does not exist. Use `argon2` or `scrypt` via
`tokio::task::spawn_blocking` to avoid blocking the async runtime:

```rust
async fn verify_password(req: Request, next: Next) -> Response {
    let (username, password) = extract_basic_auth(&req);

    let stored_hash = match db.lookup_user(&username).await {
        Some(user) => user.password_hash,
        None => {
            // Use a dummy hash to prevent timing-based user enumeration.
            // The computation takes the same time as a real verification.
            "$argon2id$v=19$m=19456,t=2,p=1$AAAA$BBBB".to_string()
        }
    };

    let valid = tokio::task::spawn_blocking(move || {
        argon2::verify_encoded(&stored_hash, password.as_bytes()).unwrap_or(false)
    }).await.unwrap_or(false);

    if valid {
        next.run(req).await
    } else {
        Response::new(http::StatusCode::UNAUTHORIZED, "invalid credentials")
    }
}
```

### 8.2 Token Rotation and Revocation

**JWT**:
- Short-lived access tokens (5-15 minutes) with refresh tokens for renewal
- For immediate revocation, maintain a blocklist (in Redis or in-memory with TTL)
  of revoked token IDs (`jti` claim)
- JWKS key rotation: the middleware should cache keys by `kid` and refresh when
  an unknown `kid` is encountered

**Session**:
- Store expiry time in the session store; middleware checks on every request
- On logout, delete the session from the store (immediate revocation)
- Rotate session ID after privilege escalation (login, sudo) to prevent
  session fixation attacks

**API keys**:
- Hash keys at rest (never store plaintext)
- Support key rotation by allowing multiple active keys per identity
- Include a `created_at` / `expires_at` to enforce rotation policies

### 8.3 CSRF Protection

**When is CSRF a concern?**
- Cookie-based auth (sessions): YES -- cookies are sent automatically by browsers
- Authorization header (JWT, API key): NO -- headers are not auto-sent

**Mitigation for session-based auth**:
- `SameSite=Lax` or `Strict` cookie attribute (covers most cases)
- Double-submit cookie pattern: middleware generates a random CSRF token, sets it
  in both a cookie and a response header; the client sends it back in a custom
  header on state-changing requests
- Synchronizer token pattern: store CSRF token in the session, embed in forms

```rust
/// CSRF middleware for session-based auth.
/// Only validates on state-changing methods (POST, PUT, DELETE, PATCH).
async fn csrf_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    if method == http::Method::GET || method == http::Method::HEAD || method == http::Method::OPTIONS {
        return next.run(req).await;
    }

    let cookie_token = extract_csrf_cookie(&req);
    let header_token = req.header("x-csrf-token");

    match (cookie_token, header_token) {
        (Some(cookie), Some(header)) if constant_time_eq(cookie.as_bytes(), header.as_bytes()) => {
            next.run(req).await
        }
        _ => Response::new(http::StatusCode::FORBIDDEN, "csrf validation failed"),
    }
}
```

### 8.4 Error Response Security

Auth middleware MUST NOT leak information about why authentication failed:

```rust
// BAD: leaks whether the user exists
"user 'admin' not found"
"password incorrect for user 'admin'"
"token expired at 2026-03-19T10:00:00Z"

// GOOD: generic error messages
"invalid credentials"       // for username/password
"invalid or expired token"  // for JWT
"unauthorized"              // for API key
```

Log detailed error information server-side (at debug/trace level) but never
include it in the response body.

### 8.5 Token Storage in Clients

While not a middleware concern, the documentation should recommend:
- **Never** store tokens in `localStorage` (XSS vulnerable)
- Prefer `HttpOnly` + `Secure` cookies for web apps
- For SPAs using Authorization headers, store tokens in memory only (not
  `sessionStorage`)
- Mobile/native apps: use platform-specific secure storage (Keychain, KeyStore)

### 8.6 Rate Limiting on Auth Endpoints

Auth endpoints (`/login`, `/token`, `/callback`) should have aggressive rate
limiting to prevent brute-force attacks. This is separate from general rate
limiting and should use per-IP or per-username counters with exponential backoff.

---

## 9. Proposed Feature Gates and Dependencies

```toml
[features]
# In harrow-middleware/Cargo.toml
jwt = ["dep:jsonwebtoken", "dep:serde", "dep:serde_json"]
api-key = ["dep:subtle", "dep:sha2"]
session = ["dep:cookie", "dep:rand", "dep:subtle"]
csrf = ["dep:subtle", "dep:rand"]

# In harrow/Cargo.toml (umbrella)
jwt = ["harrow-middleware/jwt"]
api-key = ["harrow-middleware/api-key"]
session = ["harrow-middleware/session"]
csrf = ["harrow-middleware/csrf", "harrow-middleware/session"]
```

### Priority

1. **Per-request extensions on `Request`** (prerequisite for all auth middleware)
2. **JWT middleware** (`jwt` feature) -- most common, stateless, fewest deps
3. **API key middleware** (`api-key` feature) -- simple, useful for internal APIs
4. **Session middleware** (`session` feature) -- larger scope, deferred
5. **CSRF middleware** (`csrf` feature) -- only needed with session-based auth

---

## 10. References

- [axum middleware documentation](https://docs.rs/axum/latest/axum/middleware/index.html)
- [jsonwebtoken crate](https://crates.io/crates/jsonwebtoken)
- [jwt-simple crate](https://crates.io/crates/jwt-simple)
- [tower-sessions](https://docs.rs/tower-sessions/latest/tower_sessions/)
- [subtle crate (constant-time ops)](https://docs.rs/subtle)
- [openidconnect crate](https://crates.io/crates/openidconnect)
- [tower-oauth2-resource-server](https://crates.io/crates/tower-oauth2-resource-server)
- [actix-web-httpauth](https://crates.io/crates/actix-web-httpauth)
- [actix-web-grants](https://crates.io/crates/actix-web-grants)
- [Password auth in Rust: Attacks and best practices (Luca Palmieri)](https://lpalmieri.com/posts/password-authentication-in-rust/)
- [Fortifying Rust Web Applications Against Timing Attacks (Leapcell)](https://leapcell.io/blog/fortifying-rust-web-applications-against-timing-attacks-and-common-vulnerabilities)
- [Authorization mechanisms in Rust web applications (ddtkey)](https://ddtkey.com/blog/authz-mechanisms-in-Rust/)
