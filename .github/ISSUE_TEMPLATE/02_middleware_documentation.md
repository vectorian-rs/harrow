# Issue: Improve Middleware Documentation and Examples

## Summary
The middleware system is powerful but could benefit from more comprehensive examples. Documentation on writing custom middleware and understanding the `Next` type would be helpful.

## Current State
- `Middleware` trait and `Next` type exist in `harrow-core/src/middleware.rs`
- Basic unit tests show usage patterns
- `docs/middleware.md` compares available middleware
- The `Next::run(req)` pattern is not immediately obvious to newcomers

## Concerns
1. **Unclear `Next` abstraction**: The purpose and usage of `Next` isn't well documented
2. **Limited examples**: Only basic unit tests exist
3. **No step-by-step guide**: Users must infer patterns from source code

## Current Code (from `harrow-core/src/middleware.rs`)
```rust
pub trait Middleware: Send + Sync {
    fn call(&self, req: Request, next: Next) -> BoxFuture;
}

pub struct Next {
    inner: Box<dyn FnOnce(Request) -> BoxFuture + Send>,
}

impl Next {
    pub async fn run(self, req: Request) -> Response {
        (self.inner)(req).await
    }
}
```

## Proposed Solutions
1. **Document the middleware execution model**
   - Explain the chain of responsibility pattern
   - Clarify when to call `next.run(req)` vs short-circuit

2. **Add comprehensive examples**
   - Authentication middleware example
   - Request/response logging middleware
   - Header manipulation examples
   - Conditional middleware execution

3. **Add cookbook-style documentation**
   - "How to write your first middleware"
   - "Composing multiple middleware"
   - "Middleware best practices"

4. **API documentation improvements**
   - Better rustdoc for `Next::run`
   - Explain the blanket impl for async functions
   - Document the `BoxFuture` type alias

## Acceptance Criteria
- [ ] Middleware tutorial in `docs/` or README
- [ ] Example middleware implementations in `examples/`
- [ ] Improved API documentation with usage patterns
- [ ] Decision tree for "should this be middleware or handler logic?"
- [ ] Troubleshooting guide for common middleware issues

## Priority
High - improves onboarding and reduces support burden

## Related Files
- `harrow-core/src/middleware.rs`
- `docs/middleware.md`
- `harrow/examples/` (location for new examples)
