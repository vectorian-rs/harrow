# Strategy: Adopting io_uring

`io_uring` replaces `epoll`/`kqueue` as the syscall interface for async I/O on Linux. It batches syscalls via shared ring buffers between userspace and kernel, reducing context switches and improving throughput under high concurrency.

This document covers what io_uring means for Harrow, how to adopt it, and the operational constraints.

---

## 1. Why io_uring

| Dimension | epoll (current) | io_uring |
| :--- | :--- | :--- |
| **Syscall model** | One syscall per I/O event | Batched submission/completion via shared memory |
| **Disk I/O** | `spawn_blocking` (thread pool) | Natively async — up to **40x** raw IOPS improvement |
| **Tail latency** | Higher p99 under load (kernel transitions) | Lower p99 via syscall batching |
| **Kernel requirement** | Any modern Linux | 5.10+ (basic), **6.1+** recommended |

### Rust Runtime Landscape

| Project | I/O Backend | Notes |
| :--- | :--- | :--- |
| **Tokio** (current) | `epoll` / `kqueue` | General-purpose, highly compatible |
| **tokio-uring** | `io_uring` on Tokio | Adds io_uring to standard Tokio (early stage) |
| **Monoio** | `io_uring` / `epoll` | Also supports thread-per-core (see [strategy-tpc.md](strategy-tpc.md)) |
| **Glommio** | `io_uring` | Disk-heavy workloads (databases, storage engines) |

---

## 2. Harrow Implementation Plan

### Phase 1: Driver Abstraction

Refactor `harrow-server` to be generic over an `IoDriver` trait. Rust monomorphization ensures zero runtime cost.

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

Incorporate `monoio` (or `tokio-uring`) as an optional backend for Linux.

```toml
# Cargo.toml
[features]
default = ["tokio"]
tokio = ["dep:tokio", "dep:hyper-util"]
uring = ["dep:monoio", "dep:monoio-http"]
```

The `uring` feature swaps the I/O driver implementation. Application code (handlers, middleware, routing) is unchanged.

---

## 3. Operational Constraints

### 3.1 Docker & Containers

`io_uring` introduces a larger kernel attack surface. Most container runtimes restrict it by default.

- **Docker 25.0+:** Default seccomp profile blocks `io_uring_setup`. Requires a custom seccomp profile or `--security-opt seccomp=unconfined` (not recommended for production).
- **Kernel version:** Linux **5.10+** minimum, **6.1+** recommended for `IORING_SETUP_SINGLE_ISSUER`.

### 3.2 AWS ECS & Fargate

- **ECS on EC2:** Full control. Use Amazon Linux 2023 or Bottlerocket with a modern kernel. Update Docker daemon config to allow `io_uring` syscalls.
- **ECS Fargate:** Cannot modify seccomp profile. `io_uring` support depends on internal AWS firecracker/kernel version. **Recommendation:** Use standard Tokio for Fargate until `io_uring` is explicitly supported.

### 3.3 Kubernetes

- **Default:** K8s pods use `Unconfined` seccomp — `io_uring` works but is a security risk.
- **Best practice:** Use `RuntimeDefault` seccomp profile and selectively allow `io_uring` via a specific `SeccompProfile` in the pod `SecurityContext`.

---

## 4. Environment Fit

| Environment | io_uring | Recommendation |
| :--- | :--- | :--- |
| **Bare metal / EC2** | Yes | **Best performance.** Direct kernel access. |
| **Kubernetes (self-managed)** | Yes | **Excellent.** Custom seccomp profile needed. |
| **AWS ECS (EC2)** | Yes | **Excellent.** Requires custom AMI/Docker config. |
| **AWS ECS (Fargate)** | No | **Use Tokio.** Highest compatibility and safety. |

---

## 5. Next Steps

1. Create `harrow-server-uring` as a workspace experiment.
2. Add a `bench_uring` workload to `harrow-bench` to compare with the Tokio baseline on Linux.
3. Evaluate `tokio-uring` vs `monoio` for the io_uring backend (tokio-uring preserves ecosystem compatibility; monoio offers TPC integration).
