# The Harrow Philosophy: Explicit Extractors

Harrow prioritizes **IDE transparency**, **predictable error handling**, and **zero-cost abstractions** over the "magic" ergonomics of argument-injecting frameworks.

## The Problem with "Magic" Handlers (e.g., Axum/Tower)

In many modern Rust frameworks, handlers look like this:
```rust
async fn handler(user: User, db: Db, body: Json<Update>) -> Response { ... }
```

While visually clean, this introduces three significant "Hidden Costs":

1.  **IDE Blindness:** Rust Analyzer often cannot "see" through the complex blanket implementations (e.g., `Handler<T1, T2, T3>`). If one argument fails to implement a trait, the IDE marks the entire route registration as an error, rather than the specific failing argument.
2.  **Spooky Action at a Distance:** If you forget to register `Db` in your application state, the compiler gives an error at the `.route()` call-site, potentially hundreds of lines away from the actual bug in the handler signature.
3.  **Performance Tax (Cloning):** Because the framework must extract arguments independently and concurrently, it often has to clone the `Request` or `State` multiple times to satisfy ownership requirements for each extractor.

## The Harrow Solution: Explicit Extractors

Harrow handlers use a "Request-First" model. Instead of the framework injecting data, the handler **explicitly asks** for what it needs.

```rust
async fn update_profile(req: Request) -> Result<Response, AppError> {
    // 1. Explicitly retrieve state. Returns Result<&T, Error>.
    // If missing, the error is ON THIS LINE, and the IDE knows it.
    let db = req.require_state::<DbPool>()?;

    // 2. Explicitly access path params. Returns &str.
    let id = req.param("id");

    // 3. Explicitly parse the body. 
    // Consumes the request — typically the final step.
    let body = req.body_json::<UpdateProfile>().await?;

    // Business Logic...
    db.save(id, body).await?;
    
    Ok(Response::ok())
}
```

## Comparison

| Feature | Magic (Axum) | Explicit (Harrow) |
| :--- | :--- | :--- |
| **Error Location** | Route Registration (Remote) | Handler Body (Local) |
| **IDE Support** | Often broken/slow | 100% Transparent |
| **Cloning** | Frequent (per argument) | **Minimal** (via `&mut` access) |
| **Complexity** | High (Variadic Trait gymnastics) | Low (Standard method calls) |
| **Traceability** | Hidden in framework plumbing | Visible in your code |

### A Note on Performance and Cloning
In "Magic" systems, the framework must often clone the `Request` or `State` multiple times to satisfy independent asynchronous extractors. In Harrow, you access metadata (params, headers, state) explicitly through `Request` methods **without any cloning**. While body extraction (e.g., `req.body_json()`) still consumes the request, the overall allocation pressure is significantly lower because you control the sequence.

## Security & Observability

By using explicit extractors that return `Result`, Harrow provides superior security and observability:

1.  **Early Exit:** The `?` operator ensures that if authentication or validation fails, the handler logic is never executed.
2.  **Clear Audit Trail:** Every dependency the handler has (Database, User, Config) is clearly listed at the top of the function.
3.  **Rich Error Context:** Errors are handled via the `IntoResponse` trait. This allows error types to perform their own logging (e.g., `tracing::error!`) before converting into a `Response`. Middleware can then observe the resulting status codes and latency without needing to "unwind" complex internal framework states.

## Conclusion

Harrow believes that **writing 3 lines of explicit extraction code** is a small price to pay for a codebase that is faster to compile, easier to debug, and fully understood by your IDE.
