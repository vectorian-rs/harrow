# AGENTS.md

This file is the fast-start map for agents working in the Harrow workspace.
Use it to narrow the search space before opening files.

## Default Strategy

- Start with the smallest relevant surface, not a full-repo read.
- Read `README.md` and the target crate's `src/lib.rs` before opening deeper files.
- Prefer source files, tests, and high-signal design docs over large artifact directories.
- Use `rg --files` and `rg -n` for navigation. Do not browse `docs/perf/**` unless the task is explicitly about recorded benchmark runs.

## Repo Shape

- `harrow/`
  - Public crate. Re-exports core APIs, feature-gated middleware, and server entrypoints.
  - Start here for public API changes and feature wiring.
- `harrow-core/`
  - Core framework behavior: routing, dispatch, request/response wrappers, middleware trait, state.
  - Most correctness-sensitive changes land here.
- `harrow-middleware/`
  - Feature-gated middleware implementations: timeout, request-id, cors, o11y, catch-panic, body-limit, compression, rate-limit, session.
- `harrow-server/`
  - Hyper server binding, connection lifecycle, graceful shutdown, concurrency limits.
- `harrow-server-monoio/`
  - Monoio/io_uring server for high-performance Linux deployments.
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
  - Validate with: `harrow-server/tests/integration.rs`

- Request parsing, query/body handling, state access
  - Read: `harrow-core/src/request.rs`
  - Also check: `harrow-core/src/state.rs`
  - Also check: `harrow-core/src/response.rs`

- Middleware plumbing or execution order
  - Read: `harrow-core/src/middleware.rs`
  - Also check: `harrow-core/src/dispatch.rs`
  - Validate with: `harrow-server/tests/integration.rs`

- Specific middleware behavior
  - Read the target file in `harrow-middleware/src/`
  - Check feature wiring in:
    - `harrow-middleware/Cargo.toml`
    - `harrow/Cargo.toml`
    - `harrow/src/lib.rs`
  - Useful docs:
    - `docs/middleware.md`
    - `docs/auth-middleware.md`
    - `docs/rate-limiting-middleware.md`

- Server lifecycle, connection handling, shutdown, timeouts
  - Read: `harrow-server/src/lib.rs`
  - Validate with: `harrow-server/tests/integration.rs`

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
    - `docs/performance.md`
    - `docs/middleware.md`
  - Only read `docs/perf/**` when the task is about a recorded benchmark run or artifact comparison.

- Verification or correctness strategy
  - Read: `docs/verification.md`
  - Then inspect the corresponding crate tests and fuzz targets.

- Strategy or architecture discussions
  - Read selectively:
    - `docs/prds/harrow-http-framework.md`
    - `docs/explicit-extractors.md`
    - `docs/strategy-tpc.md`
    - `docs/strategy-io-uring.md`
    - `docs/opus-review.md`

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
- `harrow-server/src/lib.rs`
- `harrow-server/tests/integration.rs`
- `docs/prds/harrow-http-framework.md`
- `docs/verification.md`
- `docs/middleware.md`

## Retrieval Rules

- Default exclude:
  - `docs/perf/**`
  - `docs/flamegraphs/**`
  - `*.svg`
  - large benchmark logs and generated artifacts
- Default exclude `infra/**` unless the task mentions deployment, AWS, Terraform, Ansible, or remote perf runs.
- For feature-gated middleware work, always verify all three layers:
  - implementation in `harrow-middleware`
  - feature declarations in Cargo manifests
  - public re-exports in `harrow/src/lib.rs`
- For behavioral changes, check at least one test surface:
  - inline unit tests in the touched module
  - `harrow-server/tests/integration.rs` for end-to-end behavior

## Commands

- Format: `mise run fmt`
- Lint: `mise run clippy`
- Test all: `mise run test`
- Full verification: `mise run verify`
- Targeted crate tests:
  - `cargo test -p harrow-core`
  - `cargo test -p harrow-middleware`
  - `cargo test -p harrow-server`
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
