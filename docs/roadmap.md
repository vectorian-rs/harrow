# Harrow Plan and Status

This document is the short product/implementation plan for the 0.10 -> 1.0
line. The detailed product source of truth remains
[prds/harrow-1.0.md](./prds/harrow-1.0.md).

## Goal

Ship Harrow 1.0 as a small, explicit, production-ready HTTP framework with:

- stable HTTP/1.1 behavior on supported backends;
- a clear backend support policy;
- practical middleware and observability;
- good lifecycle/deployment documentation;
- benchmark claims tied to measured runs.

## Status Snapshot

| Area | Status |
| --- | --- |
| Core request/response/routing | Implemented |
| Custom HTTP/1 codec and dispatcher shape | Implemented |
| Tokio custom HTTP/1 backend | First-class, public |
| Monoio HTTP/1 backend | First-class, public Linux backend |
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

2. **Clarify backend support**
   - Tokio and Monoio are first-class.
   - Meguri is experimental.
   - HTTP/2 is not a broad 1.0 promise.

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
   - rerun runtime matrix on clean bench infrastructure;
   - update `docs/performance.md` only with trusted current measurements;
   - keep old benchmark archaeology in `docs/old/` or `docs/article.md`.

## Later / Research

- HTTP/2 stabilization policy after 1.0 scope is closed.
- PROXY protocol support for L4 load balancer deployments.
- SSE helper and realtime examples.
- Auth middleware beyond the current core operational middleware.
- SIMD JSON or zero-copy helpers only if benchmarks justify the complexity.
- HTTP/3, WebTransport, and built-in gRPC remain research topics.
