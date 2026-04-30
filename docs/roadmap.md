# Harrow Plan and Status

This document is the short product/implementation plan for the 0.10 -> 1.0
line. The detailed product source of truth remains
[prds/harrow-1.0.md](./prds/harrow-1.0.md).

## Goal

Ship Harrow 1.0 as a small, explicit, production-ready HTTP framework with:

- stable HTTP/1.1 behavior on supported backends;
- a clear Hyper-vs-custom-H1 backend decision based on measured performance and maintenance risk;
- a clear backend support policy;
- practical middleware and observability;
- good lifecycle/deployment documentation;
- benchmark claims tied to measured runs.

## Status Snapshot

| Area | Status |
| --- | --- |
| Core request/response/routing | Implemented |
| Custom HTTP/1 codec and dispatcher shape | Implemented; now reference/experimental candidate pending hardening evidence |
| Hyper-based Tokio backend | First HTTP/1 prototype implemented; benchmark/H2/TLS parity pending |
| HTTP/2 backend support | 1.0 target; Hyper backend may become preferred Tokio path; Monoio partial, custom Tokio/Meguri pending |
| Tokio custom HTTP/1 backend | Public today; stable-by-default status under review because Harrow owns protocol correctness |
| Monoio HTTP/1 backend | Public Linux backend; stable support depends on parity and protocol evidence |
| Meguri direct io_uring backend | Experimental workspace backend |
| Shared H1 lifecycle model | Implemented in `harrow-server` |
| Lifecycle verification docs/model | Present, still narrow |
| Middleware set | Implemented for common operational middleware; security headers added |
| Docs | Being consolidated |
| Runtime matrix benchmarks | Tooling exists; rerun needed after latest lifecycle refactor |

## Immediate Work

1. **Consolidate docs**
   - Keep a small current docs set.
   - Move uncertain/historical docs to `docs/old/`.
   - Link current docs from `README.md` and `docs/index.md`.

2. **Clarify and complete backend support**
   - Finish and benchmark `harrow-server-tokio-hyper` before finalizing the stable 1.0 Tokio backend.
   - Compare Hyper + thread-per-core against Harrow's custom H1 stack using the same app and benchmark harness.
   - Keep the custom codec/dispatcher as a reference and advanced-performance candidate, but require hardening evidence before calling it production-stable.
   - Meguri is experimental until it meets the same protocol/lifecycle/unsafe-code bar.
   - HTTP/2 support/parity is required before 1.0, or unsupported backends must be explicitly downgraded.

3. **Document operations**
   - server lifecycle;
   - graceful shutdown;
   - deployment;
   - observability;
   - request helper model.

4. **Polish middleware**
   - preserve opt-in feature gates;
   - keep middleware backend-neutral;
   - add small, high-value production hardening features first.

5. **Refresh performance evidence**
   - add a Hyper + thread-per-core Tokio backend profile;
   - rerun runtime matrix on clean bench infrastructure;
   - update `docs/performance.md` only with trusted current measurements;
   - keep old benchmark archaeology in `docs/old/` or `docs/article.md`.

## Later / Research

- Local/`!Send` per-worker app/router mode if the Hyper/thread-per-core prototype proves the topology valuable.
- PROXY protocol support for L4 load balancer deployments.
- SSE helper and realtime examples.
- Auth middleware beyond the current core operational middleware.
- SIMD JSON or zero-copy helpers only if benchmarks justify the complexity.
- HTTP/3, WebTransport, and built-in gRPC remain research topics.
