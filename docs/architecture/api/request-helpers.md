# Request Helpers, Not Extractor Magic

Harrow's handler model is intentionally request-first:

```rust
use harrow::{Request, Response};

async fn handler(req: Request) -> Response {
    Response::text(req.param("id").to_string())
}
```

The framework does not require handler macros or extractor-heavy function
signatures. Instead, request data is accessed explicitly from `Request`.

For design rationale, see [Explicit Extractors](./explicit-extractors.md).

## Path Parameters

```rust,ignore
async fn user(req: Request) -> Response {
    let id = req.param("id");
    Response::text(format!("user {id}"))
}

let app = App::new().get("/users/:id", user);
```

Missing params return an empty string. Use route structure and application-level
validation for required/typed values.

## Query Parameters

```rust,ignore
async fn search(req: Request) -> Response {
    let q = req.query_param("q").unwrap_or_default();
    Response::text(format!("search: {q}"))
}
```

Use `query_pairs()` when you want all query pairs as a `HashMap<String, String>`.
Query parsing is bounded to avoid pathological allocation.

## Headers

```rust,ignore
async fn handler(req: Request) -> Response {
    let agent = req.header("user-agent").unwrap_or("unknown");
    Response::text(agent)
}
```

For raw access, use `req.headers()` or `req.inner()`.

## Bodies

```rust,ignore
async fn echo(req: Request) -> Response {
    match req.body_bytes().await {
        Ok(bytes) => Response::new(http::StatusCode::OK, bytes),
        Err(err) => err.into(),
    }
}
```

Body helpers enforce Harrow's configured max body size and return
`BodyError::TooLarge` when exceeded.

## JSON

Enable the `json` feature:

```toml
harrow = { version = "0.10", features = ["tokio", "json"] }
```

Then use `body_json`:

```rust,ignore
#[derive(serde::Deserialize)]
struct CreateUser {
    name: String,
}

async fn create(req: Request) -> Response {
    let body: CreateUser = match req.body_json().await {
        Ok(body) => body,
        Err(err) => return err.into(),
    };

    Response::text(format!("created {}", body.name))
}
```

## MessagePack

Enable `msgpack` and use `body_msgpack` for MessagePack request bodies.

## State and Extensions

Application state is explicit:

```rust,ignore
let db = req.require_state::<Database>()?;
```

Middleware can attach per-request data through extensions:

```rust,ignore
req.set_ext(CurrentUser { id });
let user = req.require_ext::<CurrentUser>()?;
```

## What Exists Today

Current stable helpers include:

- path params;
- query params / query pair maps;
- headers;
- body bytes;
- JSON bodies behind `json`;
- MessagePack bodies behind `msgpack`;
- application state;
- per-request extensions;
- raw `http::Request` access.

Cookies, multipart forms, protobuf, `Accept`, and `Range` helpers can be added
later as explicit helpers without changing the handler model.

## Migrating from Axum-style Extractors

Instead of this style:

```rust,ignore
async fn handler(Path(id): Path<String>, Json(body): Json<CreateUser>) { ... }
```

Use this style:

```rust,ignore
async fn handler(req: Request) -> Response {
    let id = req.param("id").to_string();
    let body: CreateUser = match req.body_json().await {
        Ok(body) => body,
        Err(err) => return err.into(),
    };
    Response::text(format!("{id}: {}", body.name))
}
```

The tradeoff is a little more explicit code in exchange for simple handler
signatures and less type-level machinery at route definition time.
