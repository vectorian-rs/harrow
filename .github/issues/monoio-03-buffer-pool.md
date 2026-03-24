# [monoio] Implement Buffer Pool & Registered Buffers

## Problem
The current implementation allocates fresh `BytesMut` for each request:

```rust
// connection.rs
let mut buf = BytesMut::with_capacity(8192);  // Allocated per request
```

This causes:
- Memory allocator pressure under high load
- Cache thrashing
- Suboptimal io_uring utilization (kernel must pin/unpin pages per op)

## Goals
Implement buffer pooling to enable:
1. **Fixed buffers** (kernel 5.1+): Pre-registered buffers for `IORING_OP_READ_FIXED`
2. **Provided buffer rings** (kernel 5.19+): Kernel selects buffer from shared ring at recv time
3. Zero-allocation fast path for hot requests

## Proposed Design

### Phase 1: Userspace Buffer Pool
```rust
// harrow-server-monoio/src/buffer.rs
pub struct BufferPool {
    slabs: Vec<BytesMut>,  // Pre-allocated slabs
    // ...
}
```
- Simple object pool using `VecDeque<BytesMut>`
- Lock-free per-thread pools (thread-per-core model)

### Phase 2: io_uring Fixed Buffers
```rust
use monoio::buf::IoBuf;

// Register buffers with the ring
let pool = BufferPool::register_fixed(&mut ring, 1024, 4096)?;  // 1024 buffers of 4KB
```
- Requires `IORING_REGISTER_BUFFERS`
- Use `read_fixed()` / `write_fixed()` ops

### Phase 3: Provided Buffer Rings (Kernel 5.19+)
```rust
// Kernel selects buffer index automatically on recv
let buf_ring = BufferRing::new(&ring, 256, 4096)?;
```
- Eliminates buffer over-provisioning
- Best for variable-sized request bodies

## Kernel Feature Detection

```rust
pub struct BufferStrategy {
    use_provided_buffers: bool,  // kernel >= 5.19
    use_fixed_buffers: bool,     // kernel >= 5.1
    use_pool: bool,              // always available
}
```

## Acceptance Criteria

- [ ] Userspace pool shows measurable improvement in `harrow-bench`
- [ ] Fixed buffer registration works on kernel 5.1+
- [ ] Provided buffer rings work on kernel 5.19+
- [ ] Graceful fallback chain: provided → fixed → pooled → allocated
- [ ] Memory usage metrics (peak, current pool size)
- [ ] No memory leaks under sustained load

## Benchmarking

```bash
# Compare before/after
cargo bench -p harrow-bench --bench echo -- --monoio
cargo bench -p harrow-bench --bench echo -- --monoio --buffer-pool
```

## Priority
**High** — Core to io_uring performance claims.

## Labels
`enhancement`, `monoio`, `performance`, `memory`

## References
- [liburing: buffer selection](https://unixism.net/loti/tutorial/buf_ring.html)
- [Cloudflare: io_uring buffer rings](https://blog.cloudflare.com/io_uring-zero-copy-send/)
