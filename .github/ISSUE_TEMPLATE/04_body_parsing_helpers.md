# Issue: Add Optional Body Parsing Helpers as Opt-in Features

## Summary
Currently relies on manual body parsing in handlers. While this aligns with the "no extractors" philosophy, providing optional body parsing helpers (JSON, form, etc.) as opt-in features could improve usability.

## Current State
From `harrow-core/src/request.rs`:
```rust
/// Consume the request and collect the body as bytes.
pub async fn body_bytes(self) -> Result<Bytes, BodyError>;

/// Parse the body as JSON.
pub async fn body_json<T: serde::de::DeserializeOwned>(self) -> Result<T, BodyError>;
```

## Concern
Manual parsing creates boilerplate:
```rust
// Current approach
async fn create_user(req: Request) -> Response {
    let body = match req.body_json::<User>().await {
        Ok(user) => user,
        Err(e) => return Response::text(format!("Invalid JSON: {}", e)).status(400),
    };
    // ... handler logic
}
```

## Design Constraints
- Must maintain the "no extractors" philosophy
- Should be opt-in, not default
- Should not add significant compile-time overhead
- Should integrate with existing `body_limit` middleware

## Proposed Solutions

### Option 1: Helper Functions (Recommended)
Add convenience methods to `Request`:
```rust
impl Request {
    /// Parse body as JSON, returning a Result that can be ?-propagated
    pub async fn json<T: DeserializeOwned>(self) -> Result<T, JsonError>;
    
    /// Parse body as form-urlencoded
    pub async fn form<T: DeserializeOwned>(self) -> Result<T, FormError>;
    
    /// Parse body with custom validation
    pub async fn json_validated<T: DeserializeOwned + Validate>(self) -> Result<T, ValidationError>;
}
```

### Option 2: Feature-Gated Middleware
```rust
// Enable automatic body parsing via middleware
app.middleware(json_body_middleware::<User>());
```

### Option 3: Response Helper Integration
```rust
// Helper that parses and handles errors
pub async fn parse_json<T: DeserializeOwned>(req: Request) -> Result<(T, Request), Response> {
    match req.body_json::<T>().await {
        Ok(val) => Ok((val, req)),
        Err(e) => Err(ProblemDetail::new(StatusCode::BAD_REQUEST)
            .detail(format!("JSON parse error: {}", e))
            .into_response()),
    }
}
```

## Acceptance Criteria
- [ ] Helper functions for common body parsing (JSON, form, raw bytes)
- [ ] Consistent error types that integrate with ProblemDetail
- [ ] Integration with `body_limit` middleware
- [ ] Feature flags if compile-time impact is significant
- [ ] Documentation and examples
- [ ] Tests for error handling paths

## Priority
Medium - improves ergonomics while maintaining philosophy

## Related Files
- `harrow-core/src/request.rs`
- `harrow-middleware/src/body_limit.rs`
- `harrow-core/src/problem.rs`

## Notes
This should NOT become a generic extractor system. Keep it simple: helper methods on Request that return Results.
