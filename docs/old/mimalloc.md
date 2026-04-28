# Plan: Integrate mimalloc as Global Allocator

## Context

Harrow is ~2x slower than Axum on c8gn.12xlarge (502K vs 1.02M RPS). The root cause is scheduling overhead — 6.3x more context switches per second (1.69M vs 270K). Both servers use glibc's ptmalloc2 which has global lock contention under high concurrency. mimalloc's thread-local heaps should reduce cross-thread contention and context switches.

## Approach

Add 2 lines (`#[global_allocator]`) directly to each of the 4 server binaries. No feature flag — it's a leaf benchmark crate, not a library. Both harrow and axum binaries get mimalloc to keep the comparison fair.

## Files

| File | Action |
|---|---|
| `Cargo.toml` | **Edit** — add `mimalloc = "0.1"` to `[workspace.dependencies]` |
| `harrow-bench/Cargo.toml` | **Edit** — add `mimalloc = { workspace = true }` to `[dependencies]` |
| `harrow-bench/src/bin/harrow_perf_server.rs` | **Edit** — add `#[global_allocator]` stanza after doc comments |
| `harrow-bench/src/bin/axum_perf_server.rs` | **Edit** — add `#[global_allocator]` stanza after doc comments |
| `harrow-bench/src/bin/harrow_server.rs` | **Edit** — add `#[global_allocator]` stanza after doc comments |
| `harrow-bench/src/bin/axum_server.rs` | **Edit** — add `#[global_allocator]` stanza after doc comments |

## NOT modified

- `measure_allocs.rs` — has its own `#[global_allocator]` (TrackingAllocator); adding mimalloc would be a compile error
- Utility binaries (compare_frameworks, stat_bench, profile_concurrent, etc.) — not server binaries
- Dockerfiles — no changes needed; `rust:1` has `cc`, mimalloc is statically linked, distroless runtime works as-is

## What gets added to each server binary

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Placed after doc comments, before `use` statements.

## Verification

```bash
cargo build --workspace          # compiles, no conflict with measure_allocs.rs
cargo test --workspace           # no regressions
cargo clippy --workspace         # no warnings
```

Then rebuild perf Docker images and re-run remote benchmark to measure impact on context switches and RPS.
