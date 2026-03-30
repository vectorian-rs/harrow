# Issue: Standardize Error Handling Patterns Beyond ProblemDetail

## Summary
While ProblemDetail support exists for 404/405 responses, general error handling in handlers could be more standardized. Consider providing more built-in error types or helper functions for common error scenarios.

## Current State
- `ProblemDetail` exists in `harrow-core/src/problem.rs` for RFC 9457 error responses
- 404/405 responses use ProblemDetail automatically
- Handlers must manually construct error responses

## Concern
Handlers currently need to manually create error responses:
```rust
// Current approach - lots of boilerplate
if let Err(e) = some_operation() {
    return ProblemDetail::new(StatusCode::INTERNAL_SERVER_ERROR)
        .title("Database Error")
        .detail(e.to_string())
        .into_response();
}
```

## Proposed Solutions
1. Add common error types (DatabaseError, ValidationError, AuthError, etc.)
2. Provide `IntoResponse` implementations for `Result<T, E>`
3. Add helper macros for common error scenarios
4. Consider a unified error type that converts to ProblemDetail automatically

## Acceptance Criteria
- [ ] Common error types defined in `harrow-core`
- [ ] `IntoResponse` trait implementations for standard errors
- [ ] Helper functions/macros for common error patterns
- [ ] Documentation with examples
- [ ] Tests for error conversion paths

## Priority
Medium - improves developer experience but not blocking

## Related Files
- `harrow-core/src/problem.rs`
- `harrow-core/src/response.rs`
- `harrow-core/src/error.rs` (to be created)
