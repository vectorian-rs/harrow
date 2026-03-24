# [monoio] Audit & Fix Async Cancellation Safety

## Problem
io_uring has a fundamental tension with Rust's async model: **dropping a future does not cancel the in-flight kernel I/O**.

Current code has potential issues:

```rust
// connection.rs
monoio::select! {
    r = handle_connection_inner(...) => r,
    _ = monoio::time::sleep(ct) => {  // Timeout fires
        tracing::warn!("connection timed out");
        Ok(())  // Future dropped, BUT kernel may still be writing to `buf`
    }
}
```

When the timeout fires:
1. `handle_connection_inner` future is dropped
2. BUT the kernel may still hold references to our buffers
3. Next `read()` uses same buffer → **use-after-free**

## Impact
- Memory corruption (potential security vulnerability)
- Connection leaks (kernel holds fd reference)
- Data corruption in subsequent requests

## Goals
Audit all async operations and ensure safe cancellation.

## Audit Checklist

### connection.rs
- [ ] `read_headers()` — timeout on header read
- [ ] `read_body()` — partial body read before timeout
- [ ] `read_chunked_body()` — chunked decode interrupted
- [ ] Connection timeout — entire connection dropped

### lib.rs
- [ ] Graceful shutdown — in-flight requests during drain
- [ ] Accept loop — listener dropped during shutdown

## Solutions

### Option 1: Explicit Cancellation (Recommended)
Use monoio's cancellable I/O:

```rust
use monoio::io::CancellableAsyncReadRent;

let op = stream.read_cancellable(buf);
monoio::select! {
    result = op.fuse() => { /* handle result */ }
    _ = timeout => {
        op.cancel().await?;  // Explicitly cancel kernel op
    }
}
```

### Option 2: Buffer Pinning
Ensure buffers live until kernel completes:

```rust
struct ReadOp {
    buf: Pin<Box<[u8]>>,
    state: OpState,
}

impl Drop for ReadOp {
    fn drop(&mut self) {
        if self.state == OpState::Pending {
            // Leak the buffer rather than UAF
            Box::into_raw(Box::from(&self.buf));
        }
    }
}
```

### Option 3: Completion Tracking
Track all in-flight ops and wait for kernel completion:

```rust
struct Connection {
    pending_ops: Vec<OpHandle>,
}

impl Drop for Connection {
    async fn drop(&mut self) {
        for op in &self.pending_ops {
            op.cancel_and_wait().await;
        }
    }
}
```

## Testing Strategy

```rust
#[tokio::test]
async fn test_cancel_safety() {
    // Start a request that will timeout
    let fut = slow_request();
    
    // Drop it immediately
    drop(fut);
    
    // Verify no UAF with miri or valgrind
    // Verify no fd leaks with /proc/self/fd
}
```

## Acceptance Criteria

- [ ] All `select!` and timeout points audited
- [ ] Miri test passes on cancel-heavy workload
- [ ] No fd leaks detected under load test
- [ ] Documentation on cancellation safety in `docs/monoio.md`

## Priority
**Critical** — Safety issue before any production use.

## Labels
`bug`, `monoio`, `safety`, `security`

## References
- [Tonbo: Async Rust is not safe with io_uring](https://tonbo.io/blog/async-rust-is-not-safe-with-io-uring)
- [Monoio: Cancellable I/O docs](https://docs.rs/monoio/latest/monoio/io/trait.CancellableAsyncReadRent.html)
