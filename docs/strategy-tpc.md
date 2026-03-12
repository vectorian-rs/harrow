# Strategy: Thread-Per-Core Architecture

Thread-per-core (TPC) pins each worker to a CPU core with its own event loop, eliminating cross-thread synchronization. No work stealing, no shared queues, no atomic contention.

This document covers what TPC means for Harrow and how to adopt it.

---

## 1. Why Thread-Per-Core

| Dimension | Work-stealing (current Tokio) | Thread-per-core |
| :--- | :--- | :--- |
| **Task scheduling** | Global queue, any thread can steal | Per-core queue, no migration |
| **Synchronization** | `Arc`, atomics, cross-thread wakers | `Rc`, no atomics, core-local wakers |
| **Cache behavior** | Tasks migrate between cores (cache thrashing) | Tasks stay on one core (cache-friendly) |
| **Scaling** | Good to ~16 cores, contention grows beyond | Near-linear to high core counts |

### Performance Results

Monoio has demonstrated **2–3x** throughput over standard Tokio on high-core-count machines by eliminating task migration and atomic contention. The gains are most visible on machines with 16+ cores running network-heavy workloads (proxies, load balancers).

On low-core-count machines (1–4 cores), TPC and work-stealing perform similarly.

---

## 2. Harrow Implementation Plan

TPC builds on the same [driver abstraction](strategy-io-uring.md) as io_uring — the `IoDriver` trait is shared.

### Feature Flag

```toml
# Cargo.toml
[features]
tpc = [] # Thread-per-core optimizations (Rc vs Arc)
```

### Zero-Cost Synchronization Swap

When TPC is enabled, shared state uses `Rc` instead of `Arc` — eliminating atomic reference counting overhead.

```rust
#[cfg(feature = "tpc")]
pub type State<T> = std::rc::Rc<T>;

#[cfg(not(feature = "tpc"))]
pub type State<T> = std::sync::Arc<T>;
```

This also applies to middleware storage (`Vec<Rc<dyn Middleware>>` vs `Vec<Arc<dyn Middleware>>`).

### SO_REUSEPORT for Connection Sharding

Each core runs its own listener on the same port. The kernel distributes incoming connections across cores.

```rust
// In the TPC driver
let socket = socket2::Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
socket.set_reuse_port(true)?;
socket.bind(&addr.into())?;
socket.listen(1024)?;
```

This is also useful in Kubernetes — the kernel load-balances traffic across per-core listeners without an extra proxy layer.

---

## 3. Trade-offs

| Consideration | Impact |
| :--- | :--- |
| **Cross-core communication** | Requires explicit channels (no shared `Arc` state across cores) |
| **Uneven load** | No work stealing means one core can be overloaded while others idle |
| **Ecosystem compatibility** | Many Tokio libraries assume `Send + Sync`; `Rc`-based state is `!Send` |
| **Debugging** | Core-local state is harder to inspect from other threads |

TPC is best suited for stateless request handlers where each request is self-contained. Workloads that require cross-request coordination (WebSocket fan-out, shared caches) need explicit message passing between cores.

---

## 4. When to Use TPC

| Scenario | Recommendation |
| :--- | :--- |
| **High-core-count Linux servers (16+ cores)** | TPC provides measurable throughput gains |
| **Network proxies and load balancers** | Ideal — stateless, high connection count |
| **Stateless API services** | Good fit — each request is independent |
| **Services with shared mutable state** | Stick with Tokio work-stealing |
| **macOS / development** | Stick with Tokio (io_uring unavailable, core count low) |

---

## 5. Next Steps

1. Prototype `Rc`-based middleware chain behind the `tpc` feature flag.
2. Implement `SO_REUSEPORT` sharding in the TPC driver.
3. Benchmark TPC vs work-stealing on a high-core-count Linux instance (c7g.4xlarge, 16 vCPU).
