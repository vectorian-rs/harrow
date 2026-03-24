# harrow-server-monoio Issues

This directory contains GitHub issue specifications for the io_uring/monoio server implementation.

## Quick Reference

| # | Issue | Priority | Status | Depends On |
|---|-------|----------|--------|------------|
| 1 | [Observability Integration](./monoio-01-observability.md) | High | ✅ Done | — |
| 2 | [HTTP/2 Support](./monoio-02-http2-support.md) | Medium-High | 🔴 Not Started | — |
| 3 | [Buffer Pool & Registered Buffers](./monoio-03-buffer-pool.md) | High | 🔴 Not Started | — |
| 4 | [Multishot io_uring Operations](./monoio-04-multishot-ops.md) | High | 🔴 Not Started | #3 |
| 5 | [Cancellation Safety Audit](./monoio-05-cancellation-safety.md) | **Critical** | ✅ Done | — |
| 6 | [Main Crate Integration](./monoio-06-integration.md) | Medium | 🔴 Not Started | — |
| 7 | [Benchmark Parity](./monoio-07-benchmarks.md) | Medium | 🔴 Not Started | #1 |
| 8 | [Testing Infrastructure](./monoio-08-testing-infrastructure.md) | Medium | 🔴 Not Started | — |

## Recommended Implementation Order

### Phase 1: Safety & Foundation
1. **#5 Cancellation Safety** — Must fix before any production use
2. **#1 Observability** — Required to measure anything else

### Phase 2: Core Performance
3. **#3 Buffer Pool** — Foundation for io_uring optimizations
4. **#4 Multishot Operations** — The actual io_uring advantage

### Phase 3: Completeness
5. **#2 HTTP/2 Support** — Feature parity with tokio
6. **#7 Benchmark Parity** — Validate all the work
7. **#6 Main Crate Integration** — Make it usable

## Labels Used

- `monoio` — All issues related to io_uring/monoio server
- `performance` — Optimization work
- `safety` / `security` — Critical correctness issues
- `enhancement` — New features
- `bug` — Existing problems

## Creating Issues on GitHub

To create these issues on GitHub, copy the markdown from each file and paste into a new issue. The front matter (title, labels) should be set manually.

Example:
```bash
gh issue create --title "[monoio] Add Metrics & Observability Integration" \
  --label "enhancement,monoio,observability" \
  --body-file .github/issues/monoio-01-observability.md
```
