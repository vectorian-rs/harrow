# AGENTS.md

This file is the fast-start map for agents working in the Harrow workspace.
Use it to narrow the search space before opening files.

## Default Strategy

- Start with the smallest relevant surface, not a full-repo read.
- Read `README.md` and the target crate's `src/lib.rs` before opening deeper files.
- Prefer source files, tests, and high-signal design docs over large artifact directories.
- Use `rg --files` and `rg -n` for navigation. Do not browse `perf/**` (local benchmark output, git-ignored) unless the task is explicitly about recorded benchmark runs.

## Repo Shape

- `harrow/`
  - Public crate. Re-exports core APIs, feature-gated middleware, and server entrypoints.
  - Start here for public API changes and feature wiring.
- `harrow-core/`
  - Core framework behavior: routing, dispatch, request/response wrappers, middleware trait, state.
  - Most correctness-sensitive changes land here.
- `harrow-middleware/`
  - Feature-gated middleware implementations: request-id, cors, o11y, catch-panic, body-limit, compression, rate-limit, session, security-headers.
- `harrow-codec-h1/`
  - HTTP/1.1 codec shared by the custom backends: request parsing, response serialization, chunked encoding.
  - Protocol-correctness and request-smuggling fixes land here. Has fuzz targets and proptests.
- `harrow-io/`
  - Runtime-agnostic I/O primitives: buffer pool and I/O traits.
- `harrow-server/`
  - Shared server primitives used by every backend: listener, workers, shutdown, config, shared H1 lifecycle.
- `harrow-server-tokio/`
  - Tokio backend on the custom HTTP/1 codec: local workers, connection lifecycle, graceful shutdown, concurrency limits.
- `harrow-server-tokio-hyper/`
  - Prototype Tokio backend where Hyper owns HTTP parsing and framing. Used to compare against the custom codec.
- `harrow-server-monoio/`
  - Monoio/io_uring server for high-performance Linux deployments.
- `harrow-server-meguri/` and `meguri/`
  - Experimental io_uring backend, and the Tokio-free io_uring library it runs on.
- `harrow-o11y/`
  - `O11yConfig` and observability-facing configuration types.
- `harrow-serde/`
  - JSON and MessagePack helpers behind feature flags.
- `harrow-bench/`
  - Criterion benches, perf binaries, remote benchmark tooling.
- `docs/`
  - Product and design docs. Use selectively.
- `infra/`
  - Terraform, Ansible, Vector, and EC2 benchmarking setup. Ignore unless the task is explicitly infra or remote benchmarking.

## Task Routing

- Public API or re-export changes
  - Read: `harrow/src/lib.rs`
  - Then inspect the underlying crate that owns the behavior.

- Routing, path matching, 404/405/HEAD semantics, route groups
  - Read: `harrow-core/src/route.rs`
  - Also check: `harrow-core/src/path.rs`
  - Also check: `harrow-core/src/dispatch.rs`
  - Validate with: `harrow-server-tokio/tests/integration.rs`

- Request parsing, query/body handling, state access
  - Read: `harrow-core/src/request.rs`
  - Also check: `harrow-core/src/state.rs`
  - Also check: `harrow-core/src/response.rs`

- Middleware plumbing or execution order
  - Read: `harrow-core/src/middleware.rs`
  - Also check: `harrow-core/src/dispatch.rs`
  - Validate with: `harrow-server-tokio/tests/integration.rs`

- Specific middleware behavior
  - Read the target file in `harrow-middleware/src/`
  - Check feature wiring in:
    - `harrow-middleware/Cargo.toml`
    - `harrow/Cargo.toml`
    - `harrow/src/lib.rs`
  - Useful docs:
    - `docs/middleware/middleware.md`
    - `docs/middleware/security.md`
    - `docs/history/old/auth-middleware.md`
    - `docs/history/old/rate-limiting-middleware.md`

- Server lifecycle, connection handling, shutdown, timeouts
  - Read: `harrow-server-tokio/src/lib.rs`
  - Also check: `harrow-server/src/h1_lifecycle.rs` (shared across backends)
  - Validate with: `harrow-server-tokio/tests/integration.rs`

- HTTP/1 parsing, framing, chunked bodies, header limits
  - Read: `harrow-codec-h1/src/lib.rs`
  - Validate with: `harrow-codec-h1` tests, `mise run fuzz:check`, and `harrow-server-tokio/tests/integration.rs`

- Observability
  - Read: `harrow-o11y/src/lib.rs`
  - Also check: `harrow-middleware/src/o11y.rs`
  - Also check: `harrow/src/lib.rs`

- Serialization format support
  - Read: `harrow-serde/src/lib.rs`
  - Then inspect `json.rs` or `msgpack.rs`

- Performance claims, benchmarks, regressions
  - Read:
    - `harrow-bench/Cargo.toml`
    - relevant files in `harrow-bench/benches/`
    - relevant files in `harrow-bench/src/bin/`
    - `docs/operations/performance.md`
    - `docs/middleware/middleware.md`
  - Only read `perf/**` when the task is about a recorded benchmark run or artifact comparison.

- Verification or correctness strategy
  - Read: `docs/operations/verification.md`
  - Then inspect the corresponding crate tests and fuzz targets.

- Strategy or architecture discussions
  - Read selectively:
    - `docs/product/harrow-1.0.md`
    - `docs/overview/what-is-harrow.md`
    - `docs/overview/roadmap.md`
    - `docs/architecture/runtime-model/local-worker-webservers.md`
    - `docs/architecture/api/explicit-extractors.md`
    - `docs/history/old/prds/harrow-http-framework.md`
    - `docs/history/old/strategy-tpc.md`
    - `docs/history/old/strategy-io-uring.md`
    - `docs/history/old/opus-review.md`

- Infra or remote benchmarking
  - Read:
    - `mise.toml`
    - `infra/README.md`
    - `infra/ec2-spot/**`
    - `infra/ansible/**`
  - Also inspect `harrow-bench/src/bin/harrow_remote_perf_test.rs`

## High-Signal Files

- `README.md`
- `Cargo.toml`
- `mise.toml`
- `harrow/src/lib.rs`
- `harrow-core/src/lib.rs`
- `harrow-server-tokio/src/lib.rs`
- `harrow-server-tokio/tests/integration.rs`
- `docs/index.md`
- `docs/product/harrow-1.0.md`
- `docs/architecture/runtime-model/local-worker-webservers.md`
- `docs/operations/verification.md`
- `docs/middleware/middleware.md`

## Retrieval Rules

- Default exclude:
  - `perf/**`
  - `docs/history/old/flamegraphs/**`
  - `*.svg`
  - large benchmark logs and generated artifacts
- Default exclude `infra/**` unless the task mentions deployment, AWS, Terraform, Ansible, or remote perf runs.
- For feature-gated middleware work, always verify all three layers:
  - implementation in `harrow-middleware`
  - feature declarations in Cargo manifests
  - public re-exports in `harrow/src/lib.rs`
- For behavioral changes, check at least one test surface:
  - inline unit tests in the touched module
  - `harrow-server-tokio/tests/integration.rs` for end-to-end behavior

## Commands

- Format: `mise run fmt`
- Lint: `mise run clippy`
- Test all: `mise run test`
- Full verification: `mise run verify`
- Targeted crate tests:
  - `cargo test -p harrow-core`
  - `cargo test -p harrow-codec-h1`
  - `cargo test -p harrow-middleware`
  - `cargo test -p harrow-server-tokio`
  - `cargo test -p harrow-server-monoio`
  - `cargo test -p harrow --features tokio` (harrow requires explicit server backend)
  - `cargo test -p harrow --features monoio` (for io_uring tests)
- Benchmarks:
  - `cargo bench`
  - or a targeted bench via `cargo bench --bench <name>`

## Practical Notes

- This workspace is small in source code but noisy in artifacts. Bias hard toward source and tests.
- Most agent tasks should be solvable by reading fewer than 10 files.
- If a task starts drifting into broad repo search, stop and re-slice by crate and feature first.
