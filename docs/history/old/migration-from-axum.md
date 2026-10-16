# Migrating from Axum to Harrow

**Status:** 2026-04-02

This guide shows concrete Axum patterns and their Harrow equivalents.
It is honest about what migrates easily, what requires rethinking, and
what Harrow deliberately does not support.

## Core Difference

Axum uses **extractors** — typed parameters in handler signatures that
the framework resolves automatically:

```rust
// Axum
async fn get_user(Path(id): Path<u32>, State(db): State<DbPool>) -> Json<User> {
    let user = db.find(id).await.unwrap();
    Json(user)
}
```

Harrow uses **explicit request access** — one `Request` parameter,
you pull what you need:

```rust
// Harrow
async fn get_user(req: Request) -> Response {
    let id: u32 = req.param("id").parse().unwrap();
    let db = req.require_state::<Arc<DbPool>>().unwrap();
    let user = db.find(id).await.unwrap();
    Response::json(&user)
}
```

More verbose, but errors appear at the extraction site, not at route
registration. No trait bound puzzles, no `#[debug_handler]`.

## Handlers

### Basic handler

```rust
// Axum
async fn hello() -> &'static str {
    "hello"
}
app.route("/", get(hello));

// Harrow
async fn hello(_req: Request) -> Response {
    Response::text("hello")
}
app.get("/", hello);
```

### Path parameters

```rust
// Axum
async fn greet(Path(name): Path<String>) -> String {
    format!("hello, {name}")
}
app.route("/greet/:name", get(greet));

// Harrow
async fn greet(req: Request) -> Response {
    let name = req.param("name");
    Response::text(format!("hello, {name}"))
}
app.get("/greet/:name", greet);
```

### JSON request/response

```rust
// Axum
async fn create_user(Json(user): Json<CreateUser>) -> (StatusCode, Json<User>) {
    let created = save(user).await;
    (StatusCode::CREATED, Json(created))
}

// Harrow
async fn create_user(req: Request) -> Result<Response, BodyError> {
    let user: CreateUser = req.body_json().await?;
    let created = save(user).await;
    Ok(Response::json(&created).status(201))
}
```

Note: `BodyError` implements `IntoResponse`, so `?` works directly.
Parse errors return 400, body too large returns 413.

### Application state

```rust
// Axum
let app = Router::new()
    .route("/users", get(list_users))
    .with_state(AppState { db, config });

async fn list_users(State(state): State<AppState>) -> Json<Vec<User>> { ... }

// Harrow
let app = App::new()
    .state(Arc::new(db))
    .state(Arc::new(config))
    .get("/users", list_users);

async fn list_users(req: Request) -> Response {
    let db = req.require_state::<Arc<DbPool>>().unwrap();
    let config = req.require_state::<Arc<Config>>().unwrap();
    // ...
}
```

Harrow stores each state type separately via `TypeMap`. No single
`AppState` struct needed — each type is independent.

### Error handling

```rust
// Axum — custom error type with IntoResponse
enum AppError {
    NotFound,
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response { ... }
}

async fn handler() -> Result<Json<Data>, AppError> { ... }

// Harrow — same pattern, different types
enum AppError {
    NotFound,
    Internal(String),
}

impl harrow::IntoResponse for AppError {
    fn into_response(self) -> harrow::Response {
        match self {
            AppError::NotFound => ProblemDetail::new(StatusCode::NOT_FOUND).into_response(),
            AppError::Internal(msg) => ProblemDetail::new(StatusCode::INTERNAL_SERVER_ERROR)
                .detail(msg)
                .into_response(),
        }
    }
}

async fn handler(req: Request) -> Result<Response, AppError> { ... }
```

`Result<T, E>` works when both `T` and `E` implement `IntoResponse`.

## Routing

### Route groups (nested routers)

```rust
// Axum
let api = Router::new()
    .route("/users", get(list_users))
    .route("/orders", get(list_orders));
let app = Router::new().nest("/api", api);

// Harrow
let app = App::new()
    .group("/api", |g| {
        g.get("/users", list_users)
         .get("/orders", list_orders)
    });
```

### Multiple HTTP methods on one path

```rust
// Axum
app.route("/users", get(list).post(create));

// Harrow
app.get("/users", list).post("/users", create);
```

### 404 and 405 handling

```rust
// Axum — custom fallback
app.fallback(handler_404);

// Harrow — built-in ProblemDetail (RFC 9457)
app.default_problem_details();
// or custom:
app.not_found_handler(my_404)
   .method_not_allowed_handler(my_405);
```

## Middleware

### Simple middleware (from_fn)

```rust
// Axum
async fn auth(req: AxumRequest, next: Next) -> Response {
    if req.headers().get("authorization").is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}
app.layer(middleware::from_fn(auth));

// Harrow
async fn auth(req: Request, next: Next) -> Response {
    if req.header("authorization").is_none() {
        return Response::new(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}
app.middleware(auth);
```

Nearly identical.

### Middleware with state

```rust
// Axum
async fn auth(
    State(config): State<AuthConfig>,
    req: AxumRequest,
    next: Next,
) -> Response {
    // use config...
    next.run(req).await
}
app.layer(middleware::from_fn_with_state(auth_config, auth));

// Harrow — access state from request
async fn auth(req: Request, next: Next) -> Response {
    let config = req.require_state::<Arc<AuthConfig>>().unwrap();
    // use config...
    next.run(req).await
}
app.state(Arc::new(auth_config)).middleware(auth);
```

### Request/response transforms

```rust
// Axum
app.layer(SetResponseHeaderLayer::new(
    header::SERVER,
    HeaderValue::from_static("harrow"),
));

// Harrow
app.middleware(map_response(|resp| resp.header("server", "harrow")));
```

```rust
// Axum
app.layer(middleware::map_request(|mut req| {
    req.extensions_mut().insert(RequestStart(Instant::now()));
    req
}));

// Harrow
app.middleware(map_request(|mut req| {
    req.set_ext(RequestStart(Instant::now()));
    req
}));
```

### Conditional middleware

```rust
// Axum — no built-in, use custom from_fn
async fn maybe_auth(req: AxumRequest, next: Next) -> Response {
    if req.uri().path().starts_with("/api") {
        // check auth...
    }
    next.run(req).await
}

// Harrow — built-in combinator
app.middleware(when(
    |req| req.path().starts_with("/api"),
    auth,
));

// Or skip for health checks:
app.middleware(unless(
    |req| req.path() == "/health",
    logging,
));
```

### Scoped middleware

```rust
// Axum — route_layer applies only to matched routes
let api = Router::new()
    .route("/users", get(list_users))
    .route_layer(middleware::from_fn(auth));

// Harrow — use groups
app.group("/api", |g| {
    g.middleware(auth)
     .get("/users", list_users)
});

// Or use `when` for more precise control:
app.middleware(when(
    |req| req.path().starts_with("/api"),
    auth,
));
```

### Tower layers

```rust
// Axum — use any Tower layer
app.layer(TimeoutLayer::new(Duration::from_secs(30)))
   .layer(ConcurrencyLimitLayer::new(100));

// Harrow — no Tower compatibility
// Timeouts are in ServerConfig:
serve_with_config(app, addr, shutdown, ServerConfig {
    header_read_timeout: Some(Duration::from_secs(5)),
    body_read_timeout: Some(Duration::from_secs(30)),
    connection_timeout: Some(Duration::from_secs(300)),
    ..Default::default()
});
// Concurrency limit is also in ServerConfig:
// max_connections: 8192
```

Tower layers do not work with Harrow. This is deliberate — Harrow
supports both Tokio and Monoio backends, and Tower assumes Tokio.

## Request extensions

```rust
// Axum
req.extensions().get::<UserId>().unwrap();

// Harrow
req.ext::<UserId>().unwrap();
// or fallible:
req.require_ext::<UserId>()?;
```

## Sessions

```rust
// Axum — typically via tower-sessions crate
let session_layer = SessionManagerLayer::new(MemoryStore::default());
app.layer(session_layer);

// Harrow — bring your own SessionStore implementation
// The framework provides the trait + middleware, you implement the store
struct RedisSessionStore { pool: RedisPool }

impl SessionStore for RedisSessionStore {
    async fn load(&self, id: &str) -> Option<HashMap<String, String>> { ... }
    async fn save(&self, id: &str, data: &HashMap<String, String>, ttl: Duration) { ... }
    async fn remove(&self, id: &str) { ... }
}

app.middleware(session_middleware(RedisSessionStore::new(pool), config));
```

Harrow does not ship an in-memory session store. Use Redis, DynamoDB,
or another distributed store for production.

## Rate Limiting

```rust
// Axum — typically via tower-governor or custom
app.layer(GovernorLayer { config });

// Harrow — bring your own RateLimitBackend
struct RedisRateLimit { pool: RedisPool }

impl RateLimitBackend for RedisRateLimit {
    async fn check(&self, key: &str) -> RateLimitOutcome { ... }
}

app.middleware(rate_limit_middleware(
    RedisRateLimit::new(pool),
    HeaderKeyExtractor::new("x-api-key"),
));
```

Same pattern as sessions — Harrow provides the trait and middleware,
you provide the backend.

## Health checks

```rust
// Axum
app.route("/health", get(|| async { "ok" }));

// Harrow — built-in helpers
app.health("/health")          // returns "ok"
   .liveness("/live")          // returns "alive"
   .readiness_handler("/ready", custom_check);
```

## OpenAPI

```rust
// Axum — via utoipa or aide crates with macros
#[utoipa::path(get, path = "/users")]
async fn list_users() -> Json<Vec<User>> { ... }

// Harrow — built-in, no macros
app.get("/users", list_users)
   .with_metadata("/users", |m| {
       m.name = Some("listUsers".into());
       m.tags.push("users".into());
   })
   .openapi("/docs", OpenApiInfo::new("My API", "1.0.0"));
```

## What Harrow Does Not Have

These Axum/Tower features have no Harrow equivalent:

| Feature | Why not |
|---|---|
| Tower `Layer`/`Service` ecosystem | Deliberate — backend independence over ecosystem size |
| `ServiceBuilder` composition | Use `.middleware()` chaining instead |
| Extractor-based handlers | Deliberate — explicit request API |
| `poll_ready` / backpressure | Not needed for request middleware |
| Built-in WebSocket support | Not yet implemented |
| Built-in SSE (Server-Sent Events) | Not yet implemented |
| `tower-http` crate compatibility | Deliberate — no Tower dependency |

## Migration Decision Framework

**Migrate to Harrow if:**
- You want backend independence (Tokio + Monoio)
- You prefer explicit request handling over extractors
- You want simpler middleware authoring
- Your middleware is mostly `from_fn` style, not reusable Tower layers
- You value framework transparency over ecosystem breadth

**Stay with Axum if:**
- You depend heavily on Tower middleware crates
- You need WebSocket or SSE support today
- Your team is invested in the extractor pattern
- You need `ServiceBuilder` composition for complex middleware stacks
