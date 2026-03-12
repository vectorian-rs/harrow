# Strategy: Adopting io_uring and Thread-Per-Core (TPC)

This document outlines the strategy for evolving Harrow's architecture to support **io_uring** and **Thread-Per-Core (TPC)** on Linux, while maintaining compatibility with standard Tokio-based deployments.

---

## 1. Landscape: Modern Rust Runtimes

Several projects have pioneered high-performance I/O and TPC in Rust. Adopting these technologies for Harrow involves standing on their shoulders.

| Project | Model | Primary Driver | Target Workload |
| :--- | :--- | :--- | :--- |
| **Tokio** (Current) | Work-stealing | `epoll` / `kqueue` | General-purpose, highly compatible. |
| **Monoio** | Thread-per-core | `io_uring` / `epoll` | Network-heavy services (proxies, load balancers). |
| **Glommio** | Thread-per-core | `io_uring` | Disk-heavy services (databases, storage engines). |
| **tokio-uring** | Work-stealing | `io_uring` | Adding `io_uring` to standard Tokio (early stage). |

### Performance Results
*   **Throughput:** Monoio has demonstrated **2x to 3x** higher throughput than standard Tokio on high-core-count machines by eliminating cross-thread synchronization and task migration overhead.
*   **Latency:** `io_uring` runtimes maintain significantly lower p99 latencies under high concurrency due to syscall batching and reduced kernel-user transitions.
*   **Disk I/O:** `io_uring` is natively asynchronous for disk operations, outperforming Tokio's `spawn_blocking` approach by up to **40x** in raw IOPS.

---

## 2. Harrow Implementation Plan

Harrow will adopt a **Runtime-Agnostic Core** with **Static Dispatch Drivers**.

### Phase 1: Driver Abstraction
Refactor `harrow-server` to be generic over an `IoDriver`. Use Rust's monomorphization to ensure zero runtime cost.

```rust
// harrow-core/src/driver.rs
pub trait IoDriver {
    type Listener;
    type Stream;
    // ...
}
```

### Phase 2: Feature-Gated Runtimes
Incorporate `monoio` as an optional backend for Linux.

```toml
# Cargo.toml
[features]
default = ["tokio"]
tokio = ["dep:tokio", "dep:hyper-util"]
uring = ["dep:monoio", "dep:monoio-http"]
tpc = [] # Thread-per-core optimizations (Rc vs Arc)
```

### Phase 3: Zero-Cost Synchronization
Use type aliases to swap `Arc` for `Rc` when the `tpc` feature is enabled.

```rust
#[cfg(feature = "tpc")]
pub type State<T> = std::rc::Rc<T>;

#[cfg(not(feature = "tpc"))]
pub type State<T> = std::sync::Arc<T>;
```

---

## 3. Operationalizing io_uring

### 3.1 Docker & Containers
`io_uring` introduces a larger kernel attack surface. Most modern container runtimes restrict it by default.

*   **Docker 25.0+:** Default seccomp profile blocks `io_uring_setup`. You must use a custom seccomp profile or `--security-opt seccomp=unconfined` (not recommended for production).
*   **Kernel Version:** Requires Linux **5.10+** for basic stability, **6.1+** recommended for high-performance features like `IORING_SETUP_SINGLE_ISSUER`.

### 3.2 AWS ECS & Fargate
*   **ECS on EC2:** You have full control. Use a custom AMI with a modern kernel (Amazon Linux 2023 or Bottlerocket). You must update the Docker daemon configuration to allow `io_uring` syscalls.
*   **ECS Fargate:** Fargate uses a microVM per task. While this provides excellent isolation, you cannot currently modify the seccomp profile. `io_uring` support in Fargate depends on the internal AWS firecracker/kernel version. **Recommendation:** Stick to standard Tokio for Fargate until `io_uring` is explicitly supported in the Fargate runtime.

### 3.3 Kubernetes
*   **Security:** By default, K8s pods are `Unconfined`. This means `io_uring` works out of the box but is a security risk.
*   **Best Practice:** Use `RuntimeDefault` seccomp profile and selectively allow `io_uring` only for the Harrow service using a specific `SeccompProfile` in the `SecurityContext`.
*   **Sharding:** Use `SO_REUSEPORT` in the TPC driver to allow K8s to load-balance traffic across your per-core listeners.

---

## 4. Environment Fit Matrix

| Environment | Approach | Recommendation |
| :--- | :--- | :--- |
| **Standard VM (Bare Metal/EC2)** | `uring` + `tpc` | **Best Performance.** Direct access to hardware/kernel. |
| **Kubernetes (Self-managed)** | `uring` + `tpc` | **Excellent.** Use `SO_REUSEPORT` for scaling. |
| **AWS ECS (EC2)** | `uring` + `tpc` | **Excellent.** Requires custom AMI/Docker config. |
| **AWS ECS (Fargate)** | `tokio` (standard) | **Most Reliable.** Highest compatibility and safety. |

---

## 5. Next Steps for Harrow
1.  Create `harrow-server-uring` as a workspace experiment.
2.  Implement `SO_REUSEPORT` sharding in the `uring` driver.
3.  Add a `bench_uring` workload to `harrow-bench` to compare with the Tokio baseline on Linux.
