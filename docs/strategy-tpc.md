# Strategy: Thread-Per-Core Architecture

Thread-per-core (TPC) pins each worker to a CPU core with its own event loop, eliminating cross-thread synchronization. No work stealing, no shared queues, no atomic contention.

This document covers what TPC means for Harrow, the evidence for and against it, and how to adopt it.

For the current Harrow architecture decision, read
[`docs/strategy-local-workers.md`](./strategy-local-workers.md) first. This
document remains the broader research note on TPC systems, tradeoffs, and prior
art.

---

## 1. Why Thread-Per-Core

| Dimension | Work-stealing (current Tokio) | Thread-per-core |
| :--- | :--- | :--- |
| **Task scheduling** | Global queue, any thread can steal | Per-core queue, no migration |
| **Synchronization** | `Arc`, atomics, cross-thread wakers | `Rc`, no atomics, core-local wakers |
| **Cache behavior** | Tasks migrate between cores (cache thrashing) | Tasks stay on one core (cache-friendly) |
| **Scaling** | Good to ~16 cores, contention grows beyond | Near-linear to high core counts |

### Published Benchmarks

**Monoio vs Tokio** ([ByteDance](https://github.com/bytedance/monoio/blob/master/docs/en/benchmark.md)): 1 core — marginal. 4 cores — **2x** Tokio. 16 cores — **close to 3x** Tokio. Key insight: Tokio's per-core performance *decreases* with core count due to work-stealing overhead. Monoio scales linearly. Exception: at single-core with few connections, Tokio wins due to lower per-operation overhead.

**Tail latency** ([Enberg et al., ANCS 2019](https://penberg.org/papers/tpc-ancs19.pdf)): TPC with application-level partitioning reduced tail latency by **up to 71%** compared to shared-everything Memcached. Foundational academic paper on TPC for network services.

**ScyllaDB vs Cassandra** ([ScyllaDB benchmarks](https://www.scylladb.com/product/benchmarks/)): 3x–8x throughput with P99 < 10ms. Cassandra P99 latencies 80%–2,200% higher. Discord's migration: P99 write latency dropped from **70ms to 5ms**. 4 ScyllaDB nodes matched 40 Cassandra nodes.

**Redpanda vs Kafka** ([Redpanda benchmarks](https://www.redpanda.com/blog/redpanda-vs-kafka-performance-benchmark)): Up to 38% faster at P99.99, 70x faster at top-end tail latencies under medium-to-high throughput. Caveat: [independent analysis by Jack Vanlightly](https://jack-vanlightly.com/blog/2023/5/15/kafka-vs-redpanda-performance-do-the-claims-add-up) found performance degraded with 50 producers (vs 4) and over runs longer than 12 hours, suggesting static partitioning struggles with workload diversity.

**Apache Iggy** ([migration blog, Feb 2026](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)): Full Tokio-to-TPC migration. "Scaling is where the thread-per-core architecture truly shines — the more partitions and producers you throw at it, the better it performs."

---

## 2. Production Systems

| Organization | System | Stack | Notes |
| :--- | :--- | :--- | :--- |
| **ScyllaDB** | NoSQL database | Seastar (C++) | Pioneer. [Shard-per-core architecture.](https://www.scylladb.com/product/technology/shard-per-core-architecture/) Discord, Comcast, Grab, Zillow. |
| **Redpanda** | Kafka-compatible streaming | Seastar (C++) | Custom memory allocator, no virtual memory. [InfoQ talk.](https://www.infoq.com/presentations/high-performance-asynchronous3/) |
| **ByteDance** | Monolake proxy | Monoio (Rust) | Production gateways. HTTP-to-Thrift, security proxies. [monolake](https://github.com/cloudwego/monolake) |
| **Apache Iggy** | Message streaming | compio (Rust) | Full rewrite from Tokio (v0.6.0, Dec 2025). 5,000 MB/s throughput. |
| **Ceph** | Distributed storage | Seastar (C++) | Crimson OSD implementation. Not yet default path. |
| **Cloudflare** | Pingora proxy | Tokio (Rust) | **Chose NOT to go full TPC** but rejected work-stealing. Multi-threaded, no stealing — a hybrid. |

### The Pingora Insight

Cloudflare's [Pingora](https://blog.cloudflare.com/pingora-open-source/) handles 40M+ req/s with 70% less CPU than NGINX. Their architecture is a deliberate middle ground: **multi-threaded without work-stealing**. Threads share nothing by convention but *can* share state when needed. This gets most TPC benefits (no scheduler overhead, good cache locality) while retaining flexibility.

This suggests production systems may benefit from a **spectrum** rather than a binary TPC-vs-work-stealing choice. The [pingora-runtime](https://docs.rs/pingora-runtime/latest/pingora_runtime/) crate implements this third flavor of Tokio runtime.

---

## 3. The Seastar Model: Lessons for Rust

Seastar ([shared-nothing design](https://seastar.io/shared-nothing/)) is the canonical TPC framework, powering ScyllaDB and Redpanda. Key patterns applicable to Rust:

### Cross-Shard Communication

All inter-core communication uses **lock-free SPSC (single-producer, single-consumer) ring buffer queues** ([message passing docs](https://seastar.io/message-passing/)):

- **Per-pair queues:** For N cores, N*(N-1) request queues + N*(N-1) response queues. A 16-core system has 480 queues.
- **Flow:** Sender enqueues request, allocates promise, returns immediately. Receiver dequeues, executes, posts result to response queue. Sender's scheduler polls response queue, fulfills promise.
- **No locks:** SPSC queues use head/tail indexes — one writer, one reader, no contention.

### Higher-Level Abstractions

- **`sharded<T>`:** One instance per shard. `invoke_on(shard_id, func)` and `invoke_on_all(func)` for cross-shard operations.
- **Map/Reduce:** Broadcast lambda to all cores, collect results, reduce.
- **Service Groups:** Rate-limit cross-shard messaging to prevent starvation.

### Memory Model

Seastar overrides malloc/free with a **per-core slab allocator**. Each core has NUMA-local memory. No overcommit, no COW. For Rust, per-core allocators like mimalloc's per-thread heaps should be considered.

### Data Rebalancing (Tablets)

ScyllaDB's [Tablets system](https://www.scylladb.com/2024/06/13/why-tablets/) (2025.1) addresses the core TPC weakness — hot shards. Tables are split into tablets that can autonomously migrate between shards/nodes. Tablets split as data grows, merge as it shrinks. "Temperature-based balancing" (auto-split hot partitions) is planned.

**Lesson for Harrow:** Any TPC adoption needs a story for load imbalance, even if just documenting "stick with Tokio for workloads with high cost variance."

---

## 4. Rust-Specific Challenges

### The Send + Sync Problem

The Tokio ecosystem was built around `Send + 'static` requirements for spawned futures. Dropping these for TPC breaks:

| Library | Issue |
| :--- | :--- |
| **hyper** | `Service` trait requires `Send` on response futures for multi-threaded runtime |
| **tower** | Middleware and `Service` implementations assume `Send + Sync` |
| **axum** | Built on hyper/tower, inherits Send bounds. `Router` uses Send-bound trait objects |
| **reqwest** | Spawns internal tasks. [Stalls with `spawn_local`](https://github.com/tokio-rs/tokio/issues/2057) |
| **Any crate calling `tokio::spawn`** | Panics inside `LocalSet` in many configurations |

This means **TPC in Rust currently requires building your own HTTP stack** (or using Monoio/compio-native equivalents). The standard hyper/tower/axum path is not viable without Send.

### Tokio's Direction

Tokio is developing `LocalRuntime` ([docs](https://docs.rs/tokio/latest/tokio/runtime/struct.LocalRuntime.html), currently unstable) — an inherently `!Send` runtime where `tokio::spawn` and `spawn_local` behave identically. May eventually deprecate `LocalSet` ([issue #6741](https://github.com/tokio-rs/tokio/issues/6741)). This would make TPC more viable within the Tokio ecosystem, but it's not stable yet.

A [Pre-RFC for local wakers](https://internals.rust-lang.org/t/pre-rfc-local-wakers/17962) proposes removing the `Send + Sync` requirement on `Waker`, which would eliminate forced thread-safety overhead in TPC runtimes.

### Rc-Based State in Practice

Dropping `Send + Sync + 'static` makes async Rust ["a pleasure to work with"](https://emschwartz.me/async-rust-can-be-a-pleasure-to-work-with-without-send-sync-static/) — `Rc<RefCell<T>>` instead of `Arc<Mutex<T>>`, structured concurrency, simpler lifetimes. Monoio, Glommio, and compio all support `!Send` futures natively. The practical challenge is the ecosystem, not the runtime.

---

## 5. Harrow Implementation Plan

TPC builds on the same [driver abstraction](strategy-io-uring.md) as io_uring — the `IoDriver` trait is shared.

### Feature Flag

```toml
# Cargo.toml
[features]
tpc = [] # Thread-per-core optimizations (Rc vs Arc)
```

### Zero-Cost Synchronization Swap

When TPC is enabled, shared state uses `Rc` instead of `Arc`:

```rust
#[cfg(feature = "tpc")]
pub type State<T> = std::rc::Rc<T>;

#[cfg(not(feature = "tpc"))]
pub type State<T> = std::sync::Arc<T>;
```

This also applies to middleware storage (`Vec<Rc<dyn Middleware>>` vs `Vec<Arc<dyn Middleware>>`).

### SO_REUSEPORT for Connection Sharding

Each core runs its own listener on the same port. The kernel distributes incoming connections across cores using a hash of the 4-tuple (src IP, src port, dst IP, dst port).

```rust
// In the TPC driver
let socket = socket2::Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
socket.set_reuse_port(true)?;
socket.bind(&addr.into())?;
socket.listen(1024)?;
```

**Fairness:** [Cloudflare found](https://blog.cloudflare.com/the-sad-state-of-linux-socket-balancing/) that SO_REUSEPORT significantly improves distribution — NGINX workers ranged 9.3%–13.2% CPU, vs highly skewed without it. Distribution degrades when traffic comes from a single source IP or when NIC hardware hashing excludes source port.

**Connection migration:** Linux supports migrating in-flight connections when a listener closes (`net.ipv4.tcp_migrate_req` sysctl, [LWN](https://lwn.net/Articles/837506/)). Without it, closing a listener RSTs all connections in its accept queue — critical for graceful shutdown in TPC.

**eBPF steering:** For cases where kernel hash distribution is insufficient, `BPF_PROG_TYPE_SK_REUSEPORT` programs can select target sockets from a `BPF_MAP_TYPE_REUSEPORT_ARRAY` map with custom logic (round-robin, least-connections).

---

## 6. When TPC Hurts

TPC's fundamental weakness is **static partitioning in the face of dynamic workloads**. As [without.boats notes](https://without.boats/blog/thread-per-core/): "Even if you try to balance work up front among different threads, they can each end up performing different amounts of work because of unpredictable differences between tasks."

### Specific Failure Cases

| Scenario | Problem | Mitigation |
| :--- | :--- | :--- |
| **Hot keys / hot partitions** | One core overloaded, others idle | Data-level rebalancing (ScyllaDB Tablets), over-partitioning |
| **Variable-cost requests** | Complex query blocks one core | Stick with work-stealing for mixed-cost workloads |
| **Low connection count** | Not enough work to distribute | Monoio is *slower* than Tokio at 1 core, few connections |
| **Bursty uneven traffic** | Some cores backlog while others drain | eBPF-based request steering, work-stealing fallback |
| **Long-running ops (12h+)** | Partitioning imbalance accumulates | [Redpanda degraded over long runs](https://jack-vanlightly.com/blog/2023/5/15/kafka-vs-redpanda-performance-do-the-claims-add-up) |

### The "Death of Thread Per Core" Argument

[Justin Jaffray (Oct 2025)](https://buttondown.com/jaffray/archive/the-death-of-thread-per-core/) argues that for data processing / query engines, **morsel-driven parallelism** is superseding TPC — as core counts increase, skewed data distribution becomes more painful, and dynamic reshuffling (morsels) handles it better. This critique targets OLAP workloads rather than network services, but the underlying point — **skew is the enemy of static partitioning** — applies broadly.

---

## 7. When to Use TPC

| Scenario | Recommendation |
| :--- | :--- |
| **High-core-count Linux (16+ cores)** | TPC provides measurable throughput gains |
| **Network proxies and load balancers** | Ideal — stateless, high connection count, uniform cost |
| **Stateless API services** | Good fit — each request is independent |
| **Services with variable request cost** | Stick with work-stealing or Pingora-style hybrid |
| **Services requiring hyper/tower/axum** | Stick with Tokio until `LocalRuntime` stabilizes |
| **macOS / development** | Stick with Tokio (io_uring unavailable, core count low) |
| **Small deployments (1–4 cores)** | No benefit — work-stealing overhead is negligible |

---

## 8. Key References

### Foundational

- [Pekka Enberg et al.: Impact of TPC on Tail Latency (ANCS 2019)](https://penberg.org/papers/tpc-ancs19.pdf) — 71% tail latency reduction, rigorous academic benchmark
- [Seastar: Shared-Nothing Design](https://seastar.io/shared-nothing/) — Canonical TPC architecture description
- [Seastar: Message Passing](https://seastar.io/message-passing/) — Cross-shard SPSC queue implementation
- [Avi Kivity at Core C++ 2019](https://www.scylladb.com/2020/03/26/avi-kivity-at-core-c-2019/) — Seastar creator on async TPC

### Rust-Specific

- [without.boats: Thread-per-core (Oct 2023)](https://without.boats/blog/thread-per-core/) — Balanced analysis of TPC vs work-stealing tension in Rust
- [Glauber Costa: C++ vs Rust — an async TPC story (Nov 2020)](https://glaubercosta-11125.medium.com/c-vs-rust-an-async-thread-per-core-story-28c4b43c410c) — Seastar vs Glommio from someone who worked 7+ years at ScyllaDB
- [Maciej Hirsz: Local Async Executors Should Be the Default (June 2022)](https://maciej.codes/2022-06-09-local-async.html) — Argument that multi-threaded async is Rust's "original sin"
- [Evan Schwartz: Async Rust Can Be a Pleasure (Sept 2024)](https://emschwartz.me/async-rust-can-be-a-pleasure-to-work-with-without-send-sync-static/) — Practical TPC + structured concurrency
- [Apache Iggy: Migration to TPC + io_uring (Feb 2026)](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/) — Most recent real-world Tokio-to-TPC migration
- [corrode.dev: The State of Async Rust — Runtimes](https://corrode.dev/blog/async/) — Ecosystem survey

### Critical / Counterpoint

- [Justin Jaffray: The Death of Thread Per Core (Oct 2025)](https://buttondown.com/jaffray/archive/the-death-of-thread-per-core/) — Morsel-driven parallelism as alternative
- [Jack Vanlightly: Kafka vs Redpanda — Do the Claims Add Up? (May 2023)](https://jack-vanlightly.com/blog/2023/5/15/kafka-vs-redpanda-performance-do-the-claims-add-up) — Independent critique of TPC benchmark methodology
- [Cloudflare: The Sad State of Linux Socket Balancing](https://blog.cloudflare.com/the-sad-state-of-linux-socket-balancing/) — SO_REUSEPORT fairness issues

### Talks

- [SE Radio 354: Avi Kivity on ScyllaDB](https://se-radio.net/2019/02/se-radio-episode-354-avi-kivity-on-scylladb/) — Shard-per-core design in depth
- [InfoQ: Adventures in Thread-per-Core Async with Redpanda](https://www.infoq.com/presentations/high-performance-asynchronous3/) — Production TPC lessons
- [ScyllaDB: Why Shard-per-Core Matters (Oct 2024)](https://www.scylladb.com/2024/10/21/why-scylladbs-shard-per-core-architecture-matters/) — Updated architecture overview

---

## 9. Next Steps

1. Prototype `Rc`-based middleware chain behind the `tpc` feature flag.
2. Implement `SO_REUSEPORT` sharding in the TPC driver.
3. Benchmark TPC vs work-stealing on a high-core-count Linux instance (c7g.4xlarge, 16 vCPU).
4. Evaluate **Pingora-style hybrid** (multi-threaded, no work-stealing) as a lower-risk alternative to full shared-nothing TPC.
5. Track Tokio `LocalRuntime` stabilization — when stable, TPC becomes viable without leaving the Tokio ecosystem.
