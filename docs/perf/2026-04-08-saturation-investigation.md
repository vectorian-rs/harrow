# Performance Saturation Investigation

**Date:** 2026-04-09
**Instance:** c8gn.12xlarge (48 vCPU Graviton3, 50 Gbps networking)
**OS:** Alpine Linux 3.21, kernel 6.12
**Goal:** Understand why throughput saturates around 880k rps, verify io_uring works, compare all backends

## Background

- March 2026: axum hit 1.06M rps on text-c128
- April 2026: all three servers cap at ~880-905k rps
- harrow-monoio was running with **epoll fallback** (missing `--privileged`) — now fixed in harness
- Client node had **default OS tuning** due to Ansible provisioning failure — now fixed

## Pre-flight Checklist

Before running any tests, verify on each node:

```bash
# Server — verify OS tuning
ssh alpine@<SERVER> "sysctl net.core.somaxconn net.core.netdev_max_backlog \
  net.core.rmem_max net.core.wmem_max net.ipv4.ip_local_port_range \
  net.ipv4.tcp_tw_reuse"
# Expected: somaxconn=65535, netdev_max_backlog=65535, rmem/wmem=16777216,
#           port_range=1024-65535, tcp_tw_reuse=1

# Client — same checks
ssh alpine@<CLIENT> "sysctl net.core.somaxconn net.core.netdev_max_backlog \
  net.core.rmem_max net.core.wmem_max net.ipv4.ip_local_port_range \
  net.ipv4.tcp_tw_reuse"

# Verify monitoring tools
ssh alpine@<SERVER> "which iostat vmstat sar mpstat perf"
ssh alpine@<CLIENT> "which iostat vmstat sar"

# Verify monoio uses io_uring (CRITICAL — has been wrong 3 times)
ssh alpine@<SERVER> "docker run --rm --privileged --network host \
  harrow-server-monoio:arm64-0.9.4 /harrow-server-monoio --bind 0.0.0.0 --port 3091 &
  sleep 2 && docker logs \$(docker ps -q --filter ancestor=harrow-server-monoio:arm64-0.9.4) 2>&1 | head -3
  docker rm -f \$(docker ps -q --filter ancestor=harrow-server-monoio:arm64-0.9.4) 2>/dev/null"
# MUST show: io: io_uring
# If it shows: io: epoll (io_uring unavailable) — STOP and fix before proceeding

# Verify images
ssh alpine@<SERVER> "docker images --format '{{.Repository}}:{{.Tag}}' | grep arm64"
ssh alpine@<CLIENT> "docker images --format '{{.Repository}}:{{.Tag}}' | grep arm64"
```

## Servers Under Test

| ID | Server | Backend | Allocator | Docker Flags |
|---|---|---|---|---|
| harrow-tokio-prod | harrow-perf-server:arm64-0.9.4 | tokio (epoll) | mimalloc | --network host --ulimit nofile=65535:65535 |
| harrow-monoio-prod | harrow-server-monoio:arm64-0.9.4 | monoio (io_uring) | mimalloc | --network host --ulimit nofile=65535:65535 **--privileged** |
| axum-prod | axum-perf-server:arm64-0.9.4 | tokio (epoll) | mimalloc | --network host --ulimit nofile=65535:65535 |

## Load Generators

| Tool | Version | Mode | Purpose |
|---|---|---|---|
| spinr | 0.5.1 | max_throughput (open loop) | Find throughput ceiling |
| wrk3 | 0.2.0 | constant rate (-R) + -L histogram | Latency under load with CO correction |

## Phase 1: Throughput Ceiling (spinr)

Find the max rps for each server. Run via the harness for consistency.

**Suite:** `harrow-bench/suites/spinr-vs-wrk3.toml` — case `spinr-text-c128`

```bash
# For each IMPL in harrow-tokio-prod harrow-monoio-prod axum-prod:
cargo run -p harrow-bench --release --bin bench-single -- \
    --impl ${IMPL} \
    --suite harrow-bench/suites/spinr-vs-wrk3.toml \
    --registry harrow-bench/implementations.toml \
    --mode remote \
    --server-ssh <SERVER_IP> --client-ssh <CLIENT_IP> \
    --server-private-ip <PRIVATE_IP> \
    --no-build-missing --duration 30 --warmup 5 \
    --case spinr-text-c128
```

**After each run**, verify monoio log:
```bash
# Only for harrow-monoio-prod — check the run artifacts or server docker logs
# Confirm "io: io_uring" appeared in startup
```

### Expected Results Table

| Server | Backend | RPS | p99 |
|---|---|---|---|
| harrow-tokio-prod | tokio/epoll | ? | ? |
| harrow-monoio-prod | monoio/io_uring | ? | ? |
| axum-prod | tokio/epoll | ? | ? |

## Phase 2: Rate Ladder (wrk3)

Step through increasing rates to find the latency inflection point.
Run all 3 servers through the same ladder.

**Suite:** `harrow-bench/suites/rate-ladder.toml`

```bash
# For each IMPL in harrow-tokio-prod harrow-monoio-prod axum-prod:
cargo run -p harrow-bench --release --bin bench-single -- \
    --impl ${IMPL} \
    --suite harrow-bench/suites/rate-ladder.toml \
    --registry harrow-bench/implementations.toml \
    --mode remote \
    --server-ssh <SERVER_IP> --client-ssh <CLIENT_IP> \
    --server-private-ip <PRIVATE_IP> \
    --no-build-missing --duration 30 --warmup 5
```

### Expected Results Table

| Rate | harrow-tokio p50/p99 | harrow-monoio p50/p99 | axum p50/p99 |
|---|---|---|---|
| 500k | ? / ? | ? / ? | ? / ? |
| 600k | ? / ? | ? / ? | ? / ? |
| 700k | ? / ? | ? / ? | ? / ? |
| 800k | ? / ? | ? / ? | ? / ? |
| 900k | ? / ? | ? / ? | ? / ? |

Look for: the rate where p99 jumps from single-digit ms to 100ms+ (the "knee").

## Phase 3: OS Telemetry

Run these **in parallel** with each Phase 2 wrk3 test. Use the 800k rate case
(near saturation) for the deepest analysis.

### Server-side (SSH into server node)

```bash
# Start all collectors before the wrk3 run, kill after
iostat -x 1 40 > /tmp/iostat-${IMPL}.txt &
vmstat 1 40 > /tmp/vmstat-${IMPL}.txt &
sar -n DEV 1 40 > /tmp/sar-net-${IMPL}.txt &
mpstat -P ALL 1 40 > /tmp/mpstat-${IMPL}.txt &
sar -w 1 40 > /tmp/sar-cs-${IMPL}.txt &
```

### Client-side (SSH into client node)

```bash
# Monitor client CPU/network to check if the load generator is the bottleneck
mpstat -P ALL 1 40 > /tmp/client-mpstat.txt &
sar -n DEV 1 40 > /tmp/client-sar-net.txt &
vmstat 1 40 > /tmp/client-vmstat.txt &
```

### After the run — collect artifacts

```bash
# From your laptop:
scp alpine@<SERVER>:/tmp/{iostat,vmstat,sar-net,mpstat,sar-cs}-${IMPL}.txt results/
scp alpine@<CLIENT>:/tmp/{client-mpstat,client-sar-net,client-vmstat}.txt results/
```

### What to look for in telemetry

| Metric | Tool | Bottleneck Signal |
|---|---|---|
| CPU per core | mpstat | One or two cores at 100% while others idle → uneven work distribution |
| Total CPU idle | mpstat | All cores busy, low idle → server is CPU-saturated |
| Context switches/s | vmstat `cs` column | >500k/s → too many context switches, syscall overhead |
| Interrupts/s | vmstat `in` column | Very high → NIC interrupt storm, consider IRQ affinity |
| Network TX/RX KB/s | sar -n DEV | Approaching 50 Gbps link limit? (unlikely for small responses) |
| TCP retransmits | sar -n DEV `retrans` | >0 → network congestion or buffer overflow |
| Run queue | vmstat `r` column | >> num_cpus → more runnable threads than cores |
| Disk I/O | iostat | Should be ~0 for an in-memory server (if not, something is wrong) |
| Client CPU | client mpstat | If client CPU is saturated → wrk3/spinr is the bottleneck, not the server |

## Phase 4: Analysis

After collecting all data, answer:

1. **Ceiling comparison (Phase 1)**
   - harrow-tokio vs axum: are they within noise (~3%) or is there a real gap?
   - monoio with io_uring vs tokio: does io_uring win now that it's actually enabled?

2. **Latency knee (Phase 2)**
   - At what rate does p99 exceed 10ms for each server?
   - Does monoio/io_uring have a higher knee than tokio/epoll?

3. **Bottleneck identification (Phase 3)**
   - Server CPU-bound? → profile with `perf record` + flamegraph
   - Client-bound? → need a beefier client or multiple clients
   - Network-bound? → check retransmits, consider jumbo frames
   - Syscall-bound? → check context switches, compare monoio (io_uring, fewer syscalls) vs tokio (epoll)

4. **March regression (1.06M → 880k)**
   - If the same test now hits 1M+ with proper tuning → it was the client OS tuning
   - If still 880k → check binary differences (workspace changes, compiler version, spot instance variance)
