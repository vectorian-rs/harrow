# [monoio] Integrate with Main `harrow` Crate

## Problem
`harrow-server-monoio` is isolated from the rest of the workspace. Users cannot easily use it:

```rust
// This doesn't work — monoio not re-exported
use harrow::{App, serve};

fn main() {
    // No way to select monoio backend
}
```

## Goals
Make monoio a first-class backend option alongside tokio.

## Proposed API

### Feature Flag Approach

```toml
# harrow/Cargo.toml
[features]
default = ["tokio"]
tokio = ["dep:harrow-server"]
monoio = ["dep:harrow-server-monoio"]
```

```rust
// harrow/src/lib.rs
#[cfg(feature = "tokio")]
pub use harrow_server::{serve, serve_with_shutdown, ServerConfig as TokioConfig};

#[cfg(feature = "monoio")]
pub use harrow_server_monoio::{serve, serve_with_shutdown, ServerConfig as MonoioConfig};
```

### Explicit Runtime Selection

```rust
// Preferred: explicit API
use harrow::{App, runtime};

#[runtime::monoio]
fn main() {
    let app = App::new().get("/", hello);
    harrow::serve_monoio(app, addr).await?;
}

#[runtime::tokio]
fn main() {
    let app = App::new().get("/", hello);
    harrow::serve_tokio(app, addr).await?;
}
```

Or without proc macros:

```rust
use harrow::{App, MonoioRuntime};

fn main() {
    let mut rt = MonoioRuntime::new()
        .enable_timer()
        .build()?;
    
    rt.block_on(async {
        let app = App::new().get("/", hello);
        harrow::serve(app, addr).await?;
    });
}
```

## Documentation Requirements

- [ ] Update `README.md` with monoio setup instructions
- [ ] Document kernel version requirements
- [ ] Document seccomp profile for containers
- [ ] Example: `examples/monoio_hello.rs`
- [ ] Migration guide from tokio to monoio

## Configuration Consistency

Ensure `ServerConfig` options have consistent semantics:

| Option | Tokio | Monoio | Notes |
|--------|-------|--------|-------|
| `max_connections` | Semaphore | Rc<Cell> | Document thread-safety difference |
| `header_read_timeout` | hyper timer | monoio::time | Verify same precision |
| `connection_timeout` | tokio::time | monoio::time | Verify same precision |
| `drain_timeout` | tokio::time | monoio::time | Verify same precision |

## Testing Strategy

- [ ] Feature flag combinations build correctly
- [ ] `cargo test --features monoio` passes
- [ ] Integration test parity with tokio server
- [ ] Doc tests for new APIs

## Acceptance Criteria

- [ ] `harrow` crate re-exports monoio server
- [ ] Feature flags work correctly (tokio | monoio | both)
- [ ] API documentation shows both backends
- [ ] Example crate compiles and runs
- [ ] No breaking changes to existing tokio API

## Priority
**Medium** — Required for adoption, not for benchmarking.

## Labels
`enhancement`, `monoio`, `api`, `documentation`

## Related
- `harrow-server-monoio/src/lib.rs` (current API)
- `harrow/src/lib.rs` (integration point)
- `docs/strategy-io-uring.md` Section 6 (operational constraints)
