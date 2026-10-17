# Docker Performance Optimization for High-Throughput Applications

This guide outlines the optimal configuration for running high-performance servers (like `harrow`) on a Linux 6.6+ kernel within a Docker environment.

## 1. Environment Overview
For maximum performance, the following "Sweet Spot" stack is recommended:
*   **Host OS:** Alpine Linux (Minimal overhead, modern 6.6+ kernel).
*   **Container Base:** `distroless/debian13` or similar (Glibc-based for superior multi-threaded memory allocation).
*   **Kernel Features:** Full support for `io_uring` and modern TCP optimizations.

## 2. Critical Bottlenecks

### File Descriptors (`ulimit -n`)
The default limit of **1024** is the most common cause of benchmark failure. A high-throughput server will exhaust this in seconds.
*   **Required:** `65535` or higher.
*   **Docker Run:** `--ulimit nofile=65535:65535`
*   **Docker Compose:**
    ```yaml
    ulimits:
      nofile:
        soft: 65535
        hard: 65535
    ```

## 3. Kernel Tuning (`sysctl`)
The following parameters should be tuned on the **Host OS**. Containers inherit these network stack settings.

| Parameter | Recommended Value | Description |
| :--- | :--- | :--- |
| `net.core.somaxconn` | `65535` | Increases the listen backlog for high connection rates. |
| `net.ipv4.tcp_tw_reuse` | `1` | Allows reusing sockets in `TIME_WAIT` state (prevents port exhaustion). |
| `net.ipv4.ip_local_port_range` | `1024 65535` | Provides ~64k ephemeral ports for client/server churn. |
| `net.core.rmem_max` | `16777216` | 16MB TCP Receive buffer for high-bandwidth streams. |
| `net.core.wmem_max` | `16777216` | 16MB TCP Send buffer. |
| `net.ipv4.tcp_fastopen` | `3` | Enables TCP Fast Open for reduced handshake latency. |

## 4. Networking Strategy

### Host Networking (`--network host`)
*   **Impact:** Zero overhead. The container shares the host's network namespace.
*   **Use Case:** Production benchmarks where maximum throughput and minimum latency are required.
*   **Trade-off:** Loss of network isolation; port conflicts with host services.

### Bridge Networking Optimization
If isolation is required, optimize the Docker daemon (`/etc/docker/daemon.json`):
```json
{
  "userland-proxy": false
}
```
*   **Why:** Disabling the Go-based `userland-proxy` forces Docker to use `iptables` for port forwarding, which is significantly more efficient.

## 5. CPU & Memory Optimization

### CPU Throttling
Docker's CFS (Completely Fair Scheduler) can throttle a container even if the host has idle CPU.
*   **Fix:** Avoid strict `--cpus` limits during benchmarks, or use `--cpu-period=0`.
*   **Pinning:** Use `--cpuset-cpus="0,1"` to pin the server to specific cores, reducing cache misses and context switching.

### Memory Swapping
Swapping is fatal to consistent tail latency.
*   **Fix:** Set `--memory-swap` equal to `--memory` and set `--memory-swappiness=0`.

## 6. Storage & Ephemeral Data
*   **Avoid Overlay2 for logs/temp files:** The OverlayFS driver adds latency to every write.
*   **Use tmpfs:** Map `/tmp` or log directories to RAM:
    ```bash
    docker run --tmpfs /tmp:size=1G ...
    ```

## 7. Benchmarking Checklist
1. [ ] **Verify ulimit:** Run `cat /proc/self/limits` inside the container.
2. [ ] **Warmup:** Run a 30s warmup to allow TCP slow-start and JIT optimization to settle.
3. [ ] **Separate Load Generator:** Never run the load generator (e.g., `wrk`) on the same CPU cores as the server.
4. [ ] **Monitor Dropped Packets:** Watch `cat /proc/net/softnet_stat` for CPU-level packet drops.
