# Issue: Improve Route Group Middleware Composition and Debugging

## Summary
While route groups work well, the middleware composition could be made more explicit. Consider providing better debugging/introspection for which middleware applies to which routes.

## Current State
- Route groups exist and support middleware composition
- Complex group hierarchies can make it unclear which middleware applies where
- No runtime introspection of middleware chain per-route

## Concerns
1. **Implicit middleware composition**: Understanding which middleware runs for a route requires mentally tracing the group hierarchy
2. **No debugging tools**: No way to print/inspect the full middleware chain for a route
3. **Order can be surprising**: Middleware execution order may not match declaration order

## Example of the Problem
```rust
let app = App::new()
    .middleware(global_mw)  // runs first
    .group("/api", |g| g
        .middleware(api_mw)  // runs second
        .group("/v1", |g| g
            .middleware(v1_mw)  // runs third
            .get("/users", list_users)
        )
    );

// What middleware runs for GET /api/v1/users?
// Currently requires mental model or reading source
```

## Proposed Solutions

### 1. Runtime Introspection
```rust
// Add method to print middleware chain
app.print_middleware_chain("/api/v1/users");
// Output: global_mw -> api_mw -> v1_mw -> handler
```

### 2. Debug Logging
```rust
// Optional debug mode that logs middleware execution
let app = App::new()
    .debug_middleware(true); // logs each middleware entry/exit
```

### 3. Middleware Chain Visualization
```rust
// Generate OpenAPI-like documentation showing middleware per-route
let docs = app.generate_middleware_docs();
```

### 4. Explicit Composition API
```rust
// Alternative API that makes composition more explicit
let api_group = RouteGroup::new("/api")
    .with_middleware(api_mw);

let v1_group = RouteGroup::new("/v1")
    .with_middleware(v1_mw)
    .inherit_from(&api_group); // explicit inheritance
```

## Acceptance Criteria
- [ ] Method to print/inspect middleware chain for a route
- [ ] Debug logging mode for middleware execution
- [ ] Documentation explaining middleware composition rules
- [ ] Visual representation of middleware hierarchy (optional)
- [ ] Tests verifying middleware order in complex hierarchies

## Priority
Medium - helps with debugging complex applications

## Related Files
- `harrow-core/src/route.rs`
- `harrow-core/src/dispatch.rs`
