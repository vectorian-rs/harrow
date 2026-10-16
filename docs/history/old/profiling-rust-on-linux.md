# Profiling Rust Services on Linux with `perf`

This is the practical setup for getting usable `perf record` data out of optimized Rust services, including Harrow's Dockerized ARM64 servers.

It is scoped to the problems we hit in this repo:
- Linux server, macOS laptop
- release binaries inside distroless containers
- ARM64 EC2 guests where hardware PMU availability is not guaranteed
- needing real call stacks, not just flat symbols

## Current Harrow State

Today the repo builds release binaries with `debug = 1` in [Cargo.toml](/Users/l1x/code/home/projectz/harrow/Cargo.toml#L52), and the remote runner records with:

```text
perf record -g -e cpu-clock -F 99
```

in [harrow_remote_perf_test.rs](/Users/l1x/code/home/projectz/harrow/harrow-bench/src/bin/harrow_remote_perf_test.rs#L725). The production image is distroless and only copies the final binary in [tokio.prod.arm64.Dockerfile](/Users/l1x/code/home/projectz/harrow/tokio.prod.arm64.Dockerfile#L37).

That setup is enough for a rough flat profile, but not ideal for stack-heavy investigation:
- `debug = 1` is limited debug info, but for Harrow profiling we need `debug = 2`
- `-F 99` is sparse for a short, high-throughput run
- distroless means symbol resolution is a report-time problem unless the binary is copied out

## Rust Build Settings

### Debug info

Rust's official debug levels are:

| Setting | Meaning |
|---|---|
| `0` / `none` | no debug info |
| `line-tables-only` | minimal line tables |
| `1` / `limited` | limited debug info, more than line tables |
| `2` / `full` | full debug info |

For this repo, treat `debug = 2` as the profiling requirement:
- `debug = 1` can be enough for partial symbolization
- `debug = 2` is what you want for useful Rust profiling output, especially for source lines and inlined callsite expansion
- do not use `debug = 1` for Harrow profiling images

Recommended profiling profile:

```toml
[profile.release-perf]
inherits = "release"
debug = 2
strip = "none"
split-debuginfo = "off"
```

Then build with:

```bash
cargo build --profile release-perf
```

If you want to keep using `--release`, the equivalent environment override is:

```bash
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off \
cargo build --release
```

On Linux ELF, `split-debuginfo = "off"` keeps the debug info in the executable, which makes reporting simpler.

### Frame pointers

If you want fast frame-pointer unwinding, build with frame pointers enabled:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile release-perf
```

Or for the existing release profile:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
CARGO_PROFILE_RELEASE_DEBUG=2 \
CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off \
cargo build --release
```

## Choosing the Unwinder

`perf-record(1)` says user-space call graph modes are `fp`, `dwarf`, or `lbr`.

For this repo:

| Mode | Use it when | Tradeoff |
|---|---|---|
| `--call-graph dwarf,16384` | you want the most complete Rust stacks, including inline frames | higher overhead, larger `perf.data` |
| `--call-graph fp,32` | you want lower overhead and already built with frame pointers | no DWARF inline expansion |
| `lbr` | do not use here | Intel-only, not for ARM64 Graviton |

Important:
- `--call-graph dwarf` does not use frame pointers for user-space unwinding
- `--call-graph fp` does use frame pointers
- if you want the cheaper unwinder, you need both frame pointers in the build and `--call-graph fp`

Recommendation:
- first root-cause run: `--call-graph dwarf,16384`
- repeat/steady runs: `--call-graph fp,32` after frame pointers are enabled

The build-side requirement does not change: for Harrow profiling, use `debug = 2`.

## Containers and Symbols

For host-side profiling of a containerized process, recording is not the hard part. Reporting is.

`perf report` and `perf script` need access to the profiled binary and its debug info. In this repo the binary lives inside the distroless container at:
- `/harrow-perf-server`
- `/axum-perf-server`

That means the clean workflow is:

1. record on the Linux host
2. copy the binary out of the container
3. report using `--symfs`

Example:

```bash
PID=$(docker inspect -f '{{.State.Pid}}' harrow-perf-server)

doas perf record \
  --call-graph dwarf,16384 \
  -e cpu-clock \
  -F 1000 \
  -o /tmp/harrow.perf.data \
  -p "$PID" -- sleep 20

mkdir -p /tmp/perf-root
docker cp harrow-perf-server:/harrow-perf-server /tmp/perf-root/harrow-perf-server

doas perf report --stdio -i /tmp/harrow.perf.data --symfs /tmp/perf-root
```

`perf-report(1)` documents `--symfs` as "Look for files with symbols relative to this directory."

If you want to move data to another Linux machine, `perf archive` is the official tool for bundling the required objects referenced by `perf.data`.

### What is not required

For host-side `perf` against a host-visible container PID, the container does not need `--privileged` just to be profiled from the host. Those capabilities matter if `perf` itself is running inside the container.

What you do need is:
- sufficient host privileges for `perf`
- permissive enough `kernel.perf_event_paranoid`
- the binary available at report time

## Event Selection on EC2 ARM64

Do not assume hardware counters are available on a given EC2 guest.

Check first:

```bash
doas perf stat -e cycles -- sleep 1
```

If that works, hardware sampling may be usable. If it fails, fall back to software events.

Practical defaults:

```bash
# safe default everywhere
doas perf record --call-graph dwarf,16384 -e cpu-clock -F 1000 -p "$PID" -- sleep 20

# try this only after verifying PMU access
doas perf record --call-graph dwarf,16384 -e cycles -F 1000 -p "$PID" -- sleep 20
```

For Harrow's current EC2 setup, `cpu-clock` is the conservative default.

## Sample Rate

`-F 99` is too low for short, high-throughput investigations.

Reasonable starting points:

| Frequency | Use |
|---|---|
| `99` | broad low-overhead telemetry only |
| `1000` | good first profiling run |
| `4000` to `5000` | denser samples if overhead stays acceptable |

Use `1000` first. Increase only after checking overhead and file size.

Also note that `perf-record(1)` says frequencies may be throttled by `kernel.perf_event_max_sample_rate`.

## Commands That Make Sense

### Best first run for Harrow

```bash
PID=$(docker inspect -f '{{.State.Pid}}' harrow-perf-server)

doas perf record \
  --call-graph dwarf,16384 \
  -e cpu-clock \
  -F 1000 \
  -o /tmp/harrow.perf.data \
  -p "$PID" -- sleep 20
```

### Lower-overhead follow-up run

After rebuilding with frame pointers:

```bash
PID=$(docker inspect -f '{{.State.Pid}}' harrow-perf-server)

doas perf record \
  --call-graph fp,32 \
  -e cpu-clock \
  -F 1000 \
  -o /tmp/harrow.perf.data \
  -p "$PID" -- sleep 20
```

### Human-readable report

```bash
doas perf report \
  --stdio \
  --call-graph graph,caller \
  -i /tmp/harrow.perf.data \
  --symfs /tmp/perf-root
```

### Flamegraph

```bash
doas perf script -i /tmp/harrow.perf.data --symfs /tmp/perf-root \
  | inferno-collapse-perf > /tmp/harrow.folded

inferno-flamegraph < /tmp/harrow.folded > /tmp/harrow.svg
```

`perf script` produces stack traces, not folded stacks by itself. You still need a collapse step such as `inferno-collapse-perf`.

## Harrow-Specific Build Guidance

Local macOS builds are not enough for the remote Docker benchmark. The profiled binary is the one built into the Linux ARM64 image in [tokio.prod.arm64.Dockerfile](/Users/l1x/code/home/projectz/harrow/tokio.prod.arm64.Dockerfile#L37).

So for remote profiling, the important change is in the Docker build, not just on the laptop:

```dockerfile
ENV RUSTFLAGS="-C force-frame-pointers=yes"
ENV CARGO_PROFILE_RELEASE_DEBUG=2
ENV CARGO_PROFILE_RELEASE_STRIP=none
ENV CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off

RUN cargo build --locked --release --target=aarch64-unknown-linux-gnu \
    -p harrow-bench \
    --bin harrow-server-tokio --bin axum-server \
    --bin harrow-perf-server --bin axum-perf-server
```

And the remote runner should move from `-F 99` to something closer to `-F 1000` for investigation runs.

## macOS Caveat

Your laptop is macOS. `perf.data` analysis needs Linux `perf`, so either:
- run `perf report` and `perf script` on the Linux host that recorded the data
- or copy `perf.data` and the symbol files to another Linux machine

The final SVG flamegraph can then be copied back to macOS.

## Recommended Next Change in This Repo

For the next profiling iteration, the cleanest improvement is:

1. add a dedicated profiling build path for the ARM64 Docker images with `debug = 2`
2. add `RUSTFLAGS="-C force-frame-pointers=yes"` to that build
3. switch `perf record` in the remote runner to `--call-graph dwarf,16384 -F 1000`
4. optionally add a lower-overhead `fp` mode later

## References

- Rust `Cargo` profiles: https://doc.rust-lang.org/cargo/reference/profiles.html
- Rust `rustc` codegen options: https://doc.rust-lang.org/rustc/codegen-options/index.html
- `perf-record(1)`: https://man7.org/linux/man-pages/man1/perf-record.1.html
- `perf-report(1)`: https://man7.org/linux/man-pages/man1/perf-report.1.html
- `perf-script(1)`: https://man7.org/linux/man-pages/man1/perf-script.1.html
- `perf-archive(1)`: https://man7.org/linux/man-pages/man1/perf-archive.1.html
