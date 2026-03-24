# [monoio] Implement Multishot io_uring Operations

## Problem
The current implementation submits one I/O operation at a time:

```rust
// Current: One accept per iteration
let (stream, _) = listener.accept().await?;

// Current: One read per body chunk
let (result, read_buf) = stream.read(read_buf).await;
```

This doesn't leverage io_uring's batching capabilities. We're effectively using io_uring as a slower epoll.

## Goals
Implement multishot operations to reduce syscalls and improve throughput:

| Operation | Kernel | Benefit |
|-----------|--------|---------|
| `IORING_OP_ACCEPT_MULTISHOT` | 5.19 | One SQE → many accepts |
| `IORING_OP_RECV_MULTISHOT` | 6.0 | One SQE → continuous receives |
| `IORING_OP_SEND_ZC` | 6.0 | Zero-copy sends |

## Technical Design

### Multishot Accept
```rust
// Instead of looping accept(), submit once, get many CQEs
let accept_op = ring.multishot_accept(listener.fd(), 0);

// Each CQE is a new connection
while let Some(cqe) = accept_op.next_cqe().await {
    let fd = cqe.result()?;
    spawn(handle_connection(fd));
}
```

### Multishot Receive
```rust
// Submit one RECV_MULTISHOT, get data as it arrives
let recv_op = stream.recv_multishot(&buf_ring);

while let Some(chunk) = recv_op.next().await {
    parser.feed(chunk)?;
    if parser.has_complete_request() {
        break;
    }
}
```

### Send Zero-Copy
```rust
// For response bodies: avoid kernel buffer copy
stream.send_zc(&response_body, 0).await?;
```

## Implementation Phases

### Phase 1: Multishot Accept (5.19+)
- [ ] Replace `accept()` loop with `multishot_accept`
- [ ] Handle CQE overflow (kernel ring full)
- [ ] Benchmark: connection/sec improvement

### Phase 2: Multishot Receive (6.0+)
- [ ] Use for header reading
- [ ] Use for body reading
- [ ] Integrate with provided buffer rings

### Phase 3: Send Zero-Copy (6.0+)
- [ ] For responses with known Content-Length
- [ ] For chunked responses
- [ ] Fallback to buffered send for small responses

## Kernel Capability Detection

```rust
pub struct UringFeatures {
    pub multishot_accept: bool,
    pub multishot_recv: bool,
    pub send_zc: bool,
}

impl UringFeatures {
    pub fn detect() -> Self { /* probe io_uring ops */ }
}
```

## Acceptance Criteria

- [ ] Multishot accept shows >20% RPS improvement in `harrow-bench`
- [ ] Graceful fallback to single-shot on older kernels
- [ ] No connection leaks on shutdown
- [ ] Works with buffer pooling (Issue #3)
- [ ] Document kernel version requirements

## Risks

1. **Cancellation complexity**: Multishot ops are harder to cancel cleanly
2. **CQE overflow**: High connection rates can overflow completion ring
3. **Monoio support**: Verify these ops are exposed in monoio 0.2

## Priority
**High** — This is the core "io_uring advantage" feature.

## Labels
`enhancement`, `monoio`, `performance`, `io-uring`

## Related
- Blocked by: Issue #3 (buffer pool)
- Related: `docs/strategy-io-uring.md` Section 3
