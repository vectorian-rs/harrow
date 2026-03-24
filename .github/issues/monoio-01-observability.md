# [monoio] Add Metrics & Observability Integration

## Problem
The `harrow-server-monoio` crate currently operates as a black box. Without visibility into request latency, connection counts, and error rates, we cannot:
- Identify performance bottlenecks
- Detect regressions in production
- Compare fairly against the tokio/hyper baseline

## Goals
Integrate with `harrow-o11y` to provide parity with the tokio server's observability.

## Proposed Metrics

### Request Metrics (per route)
- [ ] Request count (total, by status code class: 2xx, 4xx, 5xx)
- [ ] Request duration histogram (p50, p95, p99, p999)
- [ ] Request/response body sizes

### Connection Metrics
- [ ] Active connections gauge
- [ ] Connection duration histogram
- [ ] Connection limit drops counter
- [ ] Keep-alive reuse count

### Error Metrics
- [ ] Parse error count (400s)
- [ ] Timeout count (header read, connection)
- [ ] Dispatch errors

## Implementation Notes

The tokio server uses `tracing` for structured logging. We should:
1. Add `tracing` spans per request
2. Integrate with `harrow_o11y::O11yConfig` if applicable
3. Ensure minimal overhead when observability is disabled

## Acceptance Criteria
- [ ] All metrics above are emitted via `tracing` events
- [ ] Zero-allocation fast path when tracing level is disabled
- [ ] Integration test verifying metrics output
- [ ] Documentation update in `docs/o11y.md` (if exists)

## Priority
**High** — Blocks meaningful performance comparison work.

## Labels
`enhancement`, `monoio`, `observability`
