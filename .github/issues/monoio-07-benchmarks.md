# [monoio] Add Benchmark Parity & Performance Testing

## Problem
We cannot objectively compare monoio vs tokio performance because:
1. No monoio variants in `harrow-bench`
2. No HTTP/2 comparison (monoio H1 vs hyper H2)
3. No io_uring-specific benchmarks (buffer pooling, multishot)

## Goals
Establish comprehensive benchmarking for the monoio server.

## Benchmark Matrix

### 1. Protocol Comparison
| Benchmark | Tokio (hyper) | Monoio (current) | Monoio (optimized) |
|-----------|---------------|------------------|-------------------|
| HTTP/1.1 pipelining | ✅ | ✅ | ✅ |
| HTTP/2 streams | ✅ | ❌ | ✅ |
| WebSocket | ❌ | ❌ | TBD |

### 2. Workload Types
```rust
// benches/monoio_echo.rs
#[bench]
fn bench_small_json(b: &mut Bencher) {
    // 1KB JSON request/response
}

#[bench]
fn bench_large_body(b: &mut Bencher) {
    // 1MB body streaming
}

#[bench]
fn bench_high_concurrency(b: &mut Bencher) {
    // 10K concurrent connections
}

#[bench]
fn bench_keep_alive_reuse(b: &mut Bencher) {
    // Measure connection reuse efficiency
}
```

### 3. io_uring-Specific Benchmarks
```rust
// benches/uring_features.rs
#[bench]
fn bench_buffer_pool(b: &mut Bencher) {
    // Compare: allocated vs pooled vs fixed vs provided
}

#[bench]
fn bench_multishot_accept(b: &mut Bencher) {
    // Connections/sec: single-shot vs multishot
}

#[bench]
fn bench_multishot_recv(b: &mut Bencher) {
    // Throughput: per-read vs multishot
}
```

## Implementation Plan

### Phase 1: Basic Parity
- [ ] Add `benches/monoio_echo.rs` — mirrors `benches/echo.rs`
- [ ] Add `benches/monoio_full_stack.rs` — mirrors `benches/full_stack.rs`
- [ ] Add monoio variant to `benches/server_variants.rs`

### Phase 2: Binary Servers
- [ ] `harrow-bench/src/bin/monoio_server.rs` — for external load testing
- [ ] `harrow-bench/src/bin/monoio_perf_server.rs` — instrumented version

### Phase 3: Remote Benchmarking
- [ ] Update `harrow_remote_perf_test.rs` to support monoio
- [ ] Add monoio to EC2 benchmark matrix
- [ ] Document kernel version on benchmark instances

## Metrics to Capture

### Standard Metrics
- Requests per second (RPS)
- Latency distribution (p50, p95, p99, p999)
- CPU utilization
- Memory usage

### io_uring-Specific Metrics
- Syscalls per request (target: <1 with multishot)
- Context switches per request
- io_uring SQ/CQ utilization
- Buffer pool hit rate

### Infrastructure
```rust
// harrow-bench/src/monoio_metrics.rs
pub struct IoUringStats {
    pub sqes_submitted: u64,
    pub cqes_completed: u64,
    pub syscalls: u64,  // Ideally near zero with polling
}
```

## Acceptance Criteria

- [ ] `cargo bench -p harrow-bench` includes monoio variants
- [ ] CI job runs monoio benchmarks (on Linux only)
- [ ] Benchmark results stored in `docs/perf/monoio/`
- [ ] Comparison table: tokio vs monoio vs monoio+optimizations
- [ ] Flamegraph generation works with monoio

## Priority
**Medium** — Required to validate io_uring investment.

## Labels
`enhancement`, `monoio`, `benchmarks`, `performance`

## Related
- `harrow-bench/` directory
- `docs/strategy-io-uring.md` Section 9 (next steps)
- Blocked by: Issue #1 (observability — need metrics to benchmark)

## Notes
- Benchmarks must run on Linux 6.1+ for full io_uring features
- Docker Desktop cannot run these (no io_uring in VM)
- EC2 Graviton3 recommended for consistent results
