# Strategy: Adopting io_uring

`io_uring` replaces `epoll`/`kqueue` as the syscall interface for async I/O on Linux. It batches syscalls via shared ring buffers between userspace and kernel, reducing context switches and improving throughput under high concurrency.

This document covers what io_uring means for Harrow, how to adopt it, and the operational constraints.

Read this alongside [`docs/strategy-local-workers.md`](./strategy-local-workers.md).
For Harrow, the primary architectural win is local worker ownership; `io_uring`
is a secondary transport mechanism that only pays off when the runtime and
dispatcher actually exploit it.

---

## 1. Why io_uring

| Dimension | epoll (current) | io_uring |
| :--- | :--- | :--- |
| **Syscall model** | One syscall per I/O event | Batched submission/completion via shared memory |
| **Disk I/O** | `spawn_blocking` (thread pool) | Natively async — up to **40x** raw IOPS improvement |
| **Tail latency** | Higher p99 under load (kernel transitions) | Lower p99 via syscall batching |
| **Kernel requirement** | Any modern Linux | 5.6+ (basic), **6.1+** recommended |

### Published Benchmarks

**Drop-in replacement yields modest gains; purpose-built yields 2x.**
An [Alibaba OpenAnolis analysis](https://www.alibabacloud.com/blog/io-uring-vs--epoll-which-is-better-in-network-programming_599544) found that using io_uring as a drop-in epoll replacement yields only **1.06x** improvement. When explicitly designed around io_uring capabilities (batching, registered buffers, multishot), improvements reach **2.05x**. io_uring is faster for request-response (HTTP) patterns but slower for streaming workloads.

**TCP echo server** ([Seipp, 2024](https://ryanseipp.com/post/iouring-vs-epoll/)): io_uring achieved **~25% more throughput** and **~1ms better p99 latency** vs epoll on a Rust TCP echo server.

**Monoio gateway** ([ByteDance benchmarks](https://github.com/bytedance/monoio/blob/master/docs/en/benchmark.md)): 1 core — marginal difference. 4 cores — **2x** Tokio. 16 cores — **close to 3x** Tokio. The scaling advantage comes from combining io_uring with [thread-per-core](strategy-tpc.md).

**Apache Iggy** ([migration blog, Feb 2026](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)): Migrated from Tokio to compio (io_uring). Achieved **5,000 MB/s** throughput (5M messages/sec at 1KB each). WebSocket P9999 latency: ~9.5ms with fsync-per-message persistence.

**Zero-syscall HTTPS** ([habets.se, 2025](https://blog.habets.se/2025/04/io-uring-ktls-and-rust-for-zero-syscall-https-server.html)): Combined io_uring with kTLS (kernel TLS offload) — after setup, the server handles requests with **zero syscalls per request** as the kernel busy-polls the submission queue.

### Rust Runtime Landscape

| Project | I/O Backend | Stars | Status | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **Tokio** (current) | `epoll` / `kqueue` | 28k+ | Stable | General-purpose, highly compatible |
| **Monoio** (ByteDance) | `io_uring` / `epoll` | ~4,900 | Stable (0.2.4) | Production at ByteDance. Also TPC — see [strategy-tpc.md](strategy-tpc.md) |
| **Compio** | `io_uring` / IOCP / `kqueue` | Rising | Active (0.17) | Cross-platform. Chosen by Apache Iggy over monoio for broader io_uring feature coverage |
| **tokio-uring** | `io_uring` on Tokio | ~1,400 | Young | Layered on Tokio (adds overhead). Slow development cadence |
| **Glommio** (Datadog) | `io_uring` | — | **Largely unmaintained** | Historically significant but development stalled after Glauber Costa left |

**Recommendation:** Evaluate **compio** (broadest io_uring coverage, cross-platform, actively maintained) and **monoio** (production-proven, TPC integration). tokio-uring preserves Tokio compatibility but adds overhead from the layered approach. Glommio is not a viable option going forward.

---

## 2. Harrow Implementation Plan

### Phase 1: Driver Abstraction

Refactor `harrow-server-tokio` to be generic over an `IoDriver` trait. Rust monomorphization ensures zero runtime cost.

```rust
// harrow-core/src/driver.rs
pub trait IoDriver {
    type Listener;
    type Stream;
    // ...
}
```

This trait is shared with the [TPC strategy](strategy-tpc.md) — both io_uring and TPC build on the same abstraction.

### Phase 2: Feature-Gated io_uring Backend

Incorporate `monoio` or `compio` as an optional backend for Linux.

```toml
# Cargo.toml
[features]
default = ["tokio"]
tokio = ["dep:tokio", "dep:hyper-util"]
uring = ["dep:monoio", "dep:monoio-http"]
```

The `uring` feature swaps the I/O driver implementation. Application code (handlers, middleware, routing) is unchanged.

---

## 3. Kernel Features That Matter for HTTP Servers

Not all io_uring features are equally relevant. These are the ones that directly benefit HTTP server workloads:

| Feature | Kernel | Impact |
| :--- | :--- | :--- |
| **Basic io_uring** | 5.1 | Async submission/completion rings |
| **Fixed file descriptors** | 5.1 | Skip fd lookup + refcount per I/O op |
| **Fixed buffers** | 5.1 | Skip mmap/munmap per I/O; use `READ_FIXED`/`WRITE_FIXED` |
| **Provided buffer rings** | 5.19 | Kernel picks buffer from shared ring at recv time — reduces over-provisioning |
| **Multishot accept** | 5.19 | One SQE repeatedly posts CQEs for new connections — eliminates re-submission |
| **Multishot receive** | 6.0 | One SQE repeatedly posts CQEs on data arrival — combined with provided buffers for efficient body reads |
| **Send zero-copy** | 6.0 | `IORING_OP_SEND_ZC` avoids copying response bodies to kernel |
| **Network zero-copy receive** | 6.15 | DMA directly into userspace memory (bleeding edge) |

**Practical minimum for HTTP:** Kernel **5.19** (multishot accept + provided buffer rings). Kernel **6.0** adds multishot receive + send zero-copy. Amazon Linux 2023 ships **6.1**, covering everything except ZC receive.

Sources: [liburing wiki: io_uring networking in 2023](https://github.com/axboe/liburing/wiki/io_uring-and-networking-in-2023), [Red Hat: Why you should use io_uring for network I/O](https://developers.redhat.com/articles/2023/04/12/why-you-should-use-iouring-network-io), [Jens Axboe: What's new with io_uring (PDF)](https://kernel.dk/axboe-kr2022.pdf)

---

## 4. Safety: Async Cancellation Problem

io_uring has a fundamental tension with Rust's async model. Dropping a future in standard async Rust is implicitly a cancellation — you just stop polling. With epoll this is safe because epoll is a notification mechanism. But io_uring **submits actual kernel operations** — dropping the future does not cancel the in-flight kernel I/O.

Consequences:
- **Buffer use-after-free:** The kernel still holds a reference to the buffer after the future is dropped.
- **Connection leaks:** TCP connections leak when using io_uring but not epoll ([demonstrated by Tonbo](https://tonbo.io/blog/async-rust-is-not-safe-with-io-uring)).
- **Mitigation:** Runtimes must issue explicit cancellation on the ring in `Drop`. Monoio provides "cancellable I/O" for this purpose.

This is not a showstopper but requires careful runtime integration. Application code is unaffected if the runtime handles cancellation correctly.

---

## 5. Security

### Attack Surface

io_uring has a significantly larger kernel attack surface than epoll. Google's kCTF Vulnerability Rewards Program found that **60% of all exploit submissions** targeted io_uring, paying out ~$1M in bounties. Google disabled io_uring on all production servers, ChromeOS, and Android.

Notable CVEs:

| CVE | Year | Description |
| :--- | :--- | :--- |
| CVE-2023-2598 | 2023 | Out-of-bounds physical memory access via fixed buffer registration |
| CVE-2023-21400 | 2023 | Double free in io_defer_entry |
| CVE-2024-0582 | 2024 | Use-after-free (full exploit writeup published) |
| CVE-2025-21655 | 2025 | eventfd use-after-free from rapid context creation/destruction |

In April 2025, ARMO security researchers built [Curing](https://github.com/armosec/curing), a proof-of-concept rootkit operating entirely via io_uring — performing network and file I/O without any syscalls, bypassing Falco, Microsoft Defender, and other runtime security tools. Recommended mitigation: KRSI (eBPF on LSM hooks).

Sources: [Google restricting io_uring](https://www.phoronix.com/news/Google-Restricting-IO_uring), [ARMO: io_uring rootkit](https://www.armosec.io/blog/io_uring-rootkit-bypasses-linux-security/)

### Container Defaults Block io_uring

All major container runtimes now block io_uring by default in their seccomp profiles:

- **Docker/Moby:** [PR #46762](https://github.com/moby/moby/pull/46762) blocks `io_uring_setup`, `io_uring_enter`, `io_uring_register`
- **containerd:** [PR #9320](https://github.com/containerd/containerd/issues/9048) removed io_uring from RuntimeDefault
- **Docker Desktop (macOS):** Blocks io_uring even with `--privileged` and `seccomp=unconfined` as of 4.42.0

Any deployment using io_uring in containers needs a **custom seccomp profile** explicitly allowing these three syscalls.

---

## 6. Operational Constraints

### 6.1 Docker & Containers

Required custom seccomp profile (minimal):

```json
{
  "defaultAction": "SCMP_ACT_ERRNO",
  "syscalls": [
    { "names": ["io_uring_setup", "io_uring_enter", "io_uring_register"], "action": "SCMP_ACT_ALLOW" }
  ]
}
```

In practice, extend the default profile rather than starting from scratch.

### 6.2 AWS

| Environment | io_uring | Notes |
| :--- | :--- | :--- |
| **EC2 (bare metal, Graviton3)** | Yes | Amazon Linux 2023 ships kernel 6.1. Full io_uring up to send zero-copy. |
| **ECS on EC2** | Yes | Custom AMI + custom seccomp profile required. |
| **ECS Fargate** | No | Firecracker microVM. Cannot modify seccomp profile. Only `SYS_PTRACE` can be added. |
| **Lambda** | No | Firecracker. Kernel not user-configurable. No evidence io_uring is permitted. |
| **Docker Desktop (dev)** | No | Blocked even with `--privileged` on macOS. Develop with epoll fallback. |

**Recommended production path:** ECS on EC2 with Amazon Linux 2023 AMI (kernel 6.1) + Graviton3 for best price-performance (~40% better than x86).

### 6.3 Kubernetes

- **Default:** K8s pods use `Unconfined` seccomp — io_uring works but is a security risk.
- **Best practice:** Use `RuntimeDefault` seccomp profile and selectively allow io_uring via a specific `SeccompProfile` in the pod `SecurityContext`.
- Combine with `SO_REUSEPORT` for per-core listener sharding (see [TPC strategy](strategy-tpc.md)).

---

## 7. Production Adoption

| Organization | System | Stack | What they learned |
| :--- | :--- | :--- | :--- |
| **ScyllaDB** | NoSQL database | Seastar (C++) | io_uring for all async I/O. [Pioneered the approach.](https://www.scylladb.com/2020/05/05/how-io_uring-and-ebpf-will-revolutionize-programming-in-linux/) |
| **Redpanda** | Kafka-compatible streaming | Seastar (C++) | Direct io_uring + DPDK + O_DIRECT. [What makes Redpanda fast.](https://www.redpanda.com/blog/what-makes-redpanda-fast) |
| **Meta** | RocksDB storage engine | C++ | io_uring for async scans and multi-gets on NVMe. [Doubled performance](https://rocksdb.org/blog/2022/10/07/asynchronous-io-in-rocksdb.html) in some scenarios. |
| **ByteDance** | Monolake proxy framework | Monoio (Rust) | Production gateways. HTTP-to-Thrift, security gateways. [monolake](https://github.com/cloudwego/monolake) |
| **Apache Iggy** | Message streaming | compio (Rust) | Full Tokio-to-io_uring migration. [5,000 MB/s throughput.](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/) |
| **Cloudflare** | Pingora proxy | Tokio (Rust) | **Does NOT use io_uring.** Standard multi-threaded Tokio. [70% less CPU than NGINX.](https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet/) |

Cloudflare's choice is notable — Pingora handles 40M+ req/s without io_uring. The gains from io_uring are most pronounced for disk-heavy or very-high-connection-count workloads, not pure HTTP proxying at moderate concurrency.

---

## 8. Key References

### Articles

- [Alibaba: io_uring vs epoll — Which is Better?](https://www.alibabacloud.com/blog/io-uring-vs--epoll-which-is-better-in-network-programming_599544) — Quantifies when io_uring helps and when it doesn't
- [Red Hat: Why you should use io_uring for network I/O](https://developers.redhat.com/articles/2023/04/12/why-you-should-use-iouring-network-io) — Comprehensive feature overview
- [Tonbo: Async Rust is not safe with io_uring](https://tonbo.io/blog/async-rust-is-not-safe-with-io-uring) — Critical safety analysis of cancellation semantics
- [Ryan Seipp: io_uring vs epoll benchmarks](https://ryanseipp.com/post/iouring-vs-epoll/) — Practical Rust TCP benchmark
- [habets.se: Zero-syscall HTTPS with io_uring + kTLS](https://blog.habets.se/2025/04/io-uring-ktls-and-rust-for-zero-syscall-https-server.html) — Cutting-edge approach
- [Apache Iggy: Migration to thread-per-core + io_uring](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/) — Most detailed Rust migration case study
- [Glauber Costa: Introducing Glommio](https://www.datadoghq.com/blog/engineering/introducing-glommio/) — Foundational io_uring + Rust architecture
- [corrode.dev: The State of Async Rust — Runtimes](https://corrode.dev/blog/async/) — Ecosystem survey

### Talks

- [EuroRust 2024: I/O in Rust — The Whole Story](https://eurorust.eu/2024/talks/io-in-rust-the-whole-story/)
- [InfoQ: Adventures in Thread-per-Core Async with Redpanda](https://www.infoq.com/presentations/high-performance-asynchronous3/)

---

## 9. Next Steps

1. Create `harrow-server-uring` as a workspace experiment.
2. Add a `bench_uring` workload to `harrow-bench` to compare with the Tokio baseline on Linux.
3. Evaluate **compio** vs **monoio** for the io_uring backend (compio: broader feature coverage, cross-platform; monoio: production-proven, TPC integration).
4. Implement epoll fallback for macOS development and container environments where io_uring is blocked.
5. Test on Amazon Linux 2023 (kernel 6.1) to validate multishot accept + provided buffer rings + send zero-copy.
