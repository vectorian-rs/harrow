# Linux Perf and OS Telemetry Plan for `harrow-remote-perf-test`

## Goal

Identify the real bottleneck when throughput falls as concurrency rises.

The question is not only "where is CPU time spent?" but:

- Is the bottleneck on the server (`harrow` or `axum`)?
- Is the bottleneck on the client (`spinr`)?
- Is the bottleneck the kernel or network rather than either process?
- Are the measurements correct and statistically defensible?

This document defines a benchmark collection method that can answer those questions with enough rigor to drive code changes.

## Core principle

Server-only profiling is insufficient.

If throughput collapses at higher concurrency, you must observe both nodes during the same run:

- **Server node**: framework process, scheduler pressure, network stack
- **Client node**: load generator process tree, scheduler pressure, network stack

Without both sides, you cannot tell whether `harrow` is saturated or `spinr` is failing to drive the server.

## Scope

The primary target is **Phase A** from the SSH-driven benchmark orchestrator in [harrow_remote_perf_test.rs](/Users/l1x/code/home/projectz/harrow/harrow-bench/src/bin/harrow_remote_perf_test.rs#L615), because it is the head-to-head comparison:

- Harrow bare
- Axum bare
- Same client
- Same endpoint matrix
- Same concurrency matrix

Phases B and C are still useful later, but they are not the clean place to answer "is the concurrency cliff in `spinr` or in the server?"

## What must be collected

For each benchmark key such as `a_harrow_json_1kb_c128`, collect artifacts on **both** nodes.

### 1. Per-run benchmark result

This already exists:

- `a_harrow_json_1kb_c128.json`

These files contain the throughput and latency outcome from `spinr`.

### 2. Host-level OS telemetry on both nodes

Collect these during the same timed window:

- `vmstat 1`
- `sar -u 1`
- `sar -q 1`
- `sar -n DEV,TCP,ETCP 1`
- `iostat -xz 1`

Purpose:

- `vmstat`: run queue, context switching, interrupts, CPU split
- `sar -u`: user/system/iowait/idle trends
- `sar -q`: scheduler pressure, blocked tasks, load
- `sar -n`: retransmits, network saturation, TCP symptoms
- `iostat`: mostly a falsification tool here; rules disk in or out

### 3. Per-process or per-cgroup telemetry on both nodes

Host-level telemetry tells you which resource class is stressed. It does not localize blame by itself.

Collect one of these for the relevant workload on each node:

- `pidstat -durwt 1`
- container/cgroup CPU and memory counters

Purpose:

- attribute CPU, context switches, faults, I/O, and scheduling waits to the actual workload
- separate benchmark noise from unrelated system activity

### 4. `perf stat` on selected runs

`perf stat` is the best structured counter set for explaining *why* one side is spending more CPU.

Recommended counters:

- `cycles`
- `instructions`
- `branches`
- `branch-misses`
- `cache-references`
- `cache-misses`
- `context-switches`
- `cpu-migrations`
- `page-faults`

Use `perf stat` on:

- the server workload
- the client workload

But not necessarily on every run in the full matrix.

### 5. `perf record` on anomaly runs only

`perf record` is for root-cause follow-up, not broad matrix collection.

Use it for:

- one "healthy" concurrency point
- one "bad" concurrency point
- one framework pair where the gap is large

That is enough to produce flamegraphs and confirm the hot path without perturbing every run.

## Why the current server-only perf plan is not enough

The earlier approach of wrapping only the server process with `perf stat` and `perf record` is directionally useful, but incomplete for this problem.

If concurrency rises and throughput drops, the following are all plausible:

- `harrow` is saturating CPU or locking internally
- `spinr` is spending too much time in kernel/system overhead
- the client is hitting connection reuse or scheduler issues
- the network stack is retransmitting or backpressuring

The server-only plan cannot distinguish those.

## Recommended client strategy

### Preferred: run `spinr` directly on the client host

For client attribution, host-native `spinr` is better than Docker.

Reasons:

- easier and more correct `perf stat` collection
- easier `pidstat` collection over the full process tree
- no container wrapper or Docker accounting noise
- simpler process lifecycle handling

Recommended orchestration shape:

```bash
ssh client "
  perf stat -o /tmp/$KEY-client-stat.txt -- \
    /usr/local/bin/spinr load-test --max-throughput \
      -c $C -d $DURATION -w $WARMUP -j $URL \
      > /tmp/$KEY-client.json 2> /tmp/$KEY-client.stderr
"
```

This is the cleanest way to collect client-side counters for the whole workload, including child processes spawned by `spinr`.

For process attribution during the run:

```bash
ssh client "
  nohup pidstat -durwt 1 -G '^spinr$' > /tmp/$KEY-client-pidstat.txt 2>&1 &
  echo \$! > /tmp/$KEY-client-pidstat.pid
"
```

Operational rule:

- ensure the client node is dedicated to the benchmark
- ensure there are no stray `spinr` processes before each run

### Secondary: run `spinr` in Docker

If the client must use Docker, do not use `docker run --rm` for instrumented runs.

Instead:

- run a named container
- wait for it explicitly
- collect logs explicitly
- monitor either the container cgroup or the process tree inside that container

Example shape:

```bash
ssh client "
  docker run -d --name spinr-run --network host --ulimit nofile=65535:65535 \
    spinr load-test --max-throughput -c $C -d $DURATION -w $WARMUP -j $URL
"
ssh client "docker wait spinr-run"
ssh client "docker logs spinr-run > /tmp/$KEY-client.json"
ssh client "docker rm spinr-run"
```

This mode is acceptable for parity with current infra, but it is less clean for attribution than host-native `spinr`.

## Server strategy

Server collection can remain container-oriented because the server already runs in Docker in the benchmark harness.

For the server side:

- attach `pidstat` and `perf stat` to the server container workload
- keep server images unstripped for flamegraphs on targeted runs

Container requirements for `perf`:

- `--privileged`, or
- `--cap-add SYS_ADMIN --cap-add SYS_PTRACE`

## Orchestration model

The correct place to integrate this is the SSH-driven orchestrator in [harrow_remote_perf_test.rs](/Users/l1x/code/home/projectz/harrow/harrow-bench/src/bin/harrow_remote_perf_test.rs#L345), not the long-running Ansible monitor playbook in [monitor.yml](/Users/l1x/code/home/projectz/harrow/infra/ansible/playbooks/monitor.yml#L1).

Reason:

- the benchmark harness already creates stable per-run keys
- it already owns run timing and result directory layout
- monitor artifacts must line up with those same keys

### Per-run flow

For each benchmark key:

1. Generate the run key.
2. Start server-side host monitors.
3. Start client-side host monitors.
4. Start server-side per-process monitors.
5. Start client-side per-process monitors.
6. Optionally start `perf stat`.
7. Run the benchmark.
8. Stop monitors.
9. Pull raw artifacts into the results directory.
10. Parse a compact summary from the raw artifacts.

### Pseudocode

```text
for framework in [harrow, axum]:
    start server container
    wait for health check

    for (endpoint, label) in PHASE_A_ENDPOINTS:
        for concurrency in PHASE_A_CONCURRENCIES:
            key = "a_{framework}_{label}_c{concurrency}"

            start server vmstat/sar/iostat
            start client vmstat/sar/iostat

            start server pidstat
            start client pidstat

            if perf_stat_enabled(key):
                start server perf stat
                start client perf stat

            run spinr

            stop server perf stat
            stop client perf stat
            stop server pidstat
            stop client pidstat
            stop server vmstat/sar/iostat
            stop client vmstat/sar/iostat

            collect all artifacts into docs/perf/<instance-type>/<timestamp>/

    stop server container
```

## Output structure

Use the existing benchmark directory layout and extend it with keyed monitor files:

```text
docs/perf/<instance-type>/<timestamp>/
├── a_harrow_json_1kb_c128.json
├── a_harrow_json_1kb_c128.server.vmstat.txt
├── a_harrow_json_1kb_c128.server.sar-u.txt
├── a_harrow_json_1kb_c128.server.sar-q.txt
├── a_harrow_json_1kb_c128.server.sar-net.txt
├── a_harrow_json_1kb_c128.server.iostat.txt
├── a_harrow_json_1kb_c128.server.pidstat.txt
├── a_harrow_json_1kb_c128.server.perf-stat.txt
├── a_harrow_json_1kb_c128.client.vmstat.txt
├── a_harrow_json_1kb_c128.client.sar-u.txt
├── a_harrow_json_1kb_c128.client.sar-q.txt
├── a_harrow_json_1kb_c128.client.sar-net.txt
├── a_harrow_json_1kb_c128.client.iostat.txt
├── a_harrow_json_1kb_c128.client.pidstat.txt
├── a_harrow_json_1kb_c128.client.perf-stat.txt
├── a_harrow_json_1kb_c128.meta.json
├── a_harrow_json_1kb_c128.server.svg
├── a_harrow_json_1kb_c128.client.svg
├── ...
└── summary.md
```

The `.meta.json` file should include:

- key
- framework
- endpoint
- concurrency
- warmup seconds
- measurement seconds
- server host
- client host
- UTC start time
- UTC end time
- whether `spinr` ran in Docker or directly on the host

## Correctness requirements

### Warmup and measurement windows must be explicit

The telemetry window should cover:

- benchmark warmup
- measured run
- a small pre/post margin, such as 2 seconds

But summary statistics should be computed from the measurement window only, or clearly split into:

- warmup metrics
- measured metrics

If warmup and measurement are mixed together, the telemetry is much less useful.

### Benchmark instrumentation overhead must be measured

Monitoring changes the benchmark. Treat this as a first-class verification step.

Run a calibration subset with:

1. no monitors
2. OS monitors only
3. OS monitors + `perf stat`
4. OS monitors + `perf stat` + `perf record`

If the instrumentation materially changes throughput or latency, do not use it blindly across the full matrix.

### Independent load-generator control

To verify whether `spinr` is the problem, add a small control set with a different client such as:

- `wrk2`
- `h2load`

Use the same server binary, endpoint, and concurrency on a representative subset.

Interpretation:

- if both clients show the same cliff, the server is more likely the bottleneck
- if only `spinr` shows the cliff, the problem is likely in `spinr`

## Statistical significance and experimental design

Single runs are not enough.

### Minimum recommendation

For each key:

- run at least 5 repetitions
- discard the first cold run
- report median
- report variability with either:
  - bootstrap 95% confidence interval, or
  - median and MAD

### Ordering

Do not run all Harrow runs and then all Axum runs in a fixed block.

Use either:

- randomized run order, or
- strict alternation/interleaving

This reduces time drift bias from:

- noisy neighbors
- thermal changes
- background AWS variance
- client or server state drift

### Decision rule

A suspected bottleneck should not be called "real" unless:

- the throughput gap reproduces across repeated runs
- the telemetry pattern reproduces across repeated runs
- the bottleneck story is consistent across at least one independent client

## How to actually pinpoint the bottleneck

### Strong evidence for a client-side `spinr` bottleneck

You see most of the following:

- client `%system` rises before server CPU does
- client run queue rises sharply while server run queue remains modest
- client context switches and migrations spike
- client `pidstat` shows `spinr` dominating CPU and scheduler activity
- server remains materially underutilized
- `wrk2` or `h2load` can push the same server harder than `spinr`

### Strong evidence for a server-side `harrow` or `axum` bottleneck

You see most of the following:

- server CPU or run queue saturates first
- server `pidstat` shows the framework process dominating resource usage
- client remains capable of driving more work
- alternate clients reproduce the same scaling curve
- server-side `perf stat` and flamegraphs explain the extra work

### Strong evidence for kernel or network contention

You see most of the following:

- `%system` dominates on one or both nodes
- retransmits or TCP anomalies rise in `sar -n`
- softirq-like behavior appears indirectly via interrupts/context switching
- user-space processes are not clearly saturating their own CPU budgets

### Strong evidence for disk or logging contention

You see most of the following:

- `iostat` device utilization rises materially
- `await` or service time increases
- `iowait` appears in `sar -u`

This is unlikely for the happy path here, but if it appears, it is important.

## `perf stat` and `perf record` usage

### `perf stat`

Default recommendation:

- use on selected representative runs
- use on both nodes
- treat it as a structured counter tool, not a flamegraph tool

Good targets:

- one low concurrency point
- one mid concurrency point
- one bad high concurrency point

### `perf record`

Default recommendation:

- do not run on every point
- use only after telemetry shows where the anomaly is

Good targets:

- the worst offending concurrency point
- a nearby control point where scaling still looks normal

Then compare flamegraphs between:

- Harrow vs Axum on the server
- spinr healthy vs spinr unhealthy on the client

## Prerequisites

### Server node

The benchmark role already installs the necessary packages in [roles/bench/tasks/main.yml](/Users/l1x/code/home/projectz/harrow/infra/ansible/roles/bench/tasks/main.yml#L35):

- `perf`
- `sysstat`
- `procps-ng`
- `ethtool`

### Client node

Same as server.

### Laptop

If generating flamegraphs locally:

```bash
cargo install inferno
```

If processing raw `perf.data`, use a Linux machine with matching architecture, or collapse stacks on the remote node and pull the collapsed text instead.

## Implementation plan

1. Extend the SSH-driven benchmark orchestrator in `harrow_remote_perf_test.rs` with a flag such as `--os-monitors`.
2. Add per-run monitor helpers for:
   `vmstat`, `sar -u`, `sar -q`, `sar -n DEV,TCP,ETCP`, `iostat -xz`, and `pidstat -durwt`.

3. Add per-run metadata emission for exact timing and mode.
4. Add `--spinr-mode docker|host`, with `host` as the preferred instrumented mode.
5. In host mode, run `/usr/local/bin/spinr` directly on the client and wrap it with `perf stat` when enabled.
6. In Docker mode, replace `docker run --rm` with named-container lifecycle for instrumented runs.
7. Add sparse `--perf-stat` and `--perf-record` controls rather than forcing them on every run.
8. Add parsing of key telemetry fields into `summary.md`.
9. Add run repetition and interleaving/randomization support.
10. Add a small control path using `wrk2` or `h2load` on representative cases.

## Recommended rollout order

1. Per-run OS telemetry on both nodes
2. Per-process `pidstat` on both nodes
3. Host-native `spinr` mode
4. Repetition and randomized ordering
5. `perf stat` on selected runs
6. `perf record` on anomaly runs
7. Independent control client

This order gives useful answers early, while keeping instrumentation overhead manageable.
