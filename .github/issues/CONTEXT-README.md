# AI Agent Context Files for monoio Issues

This directory contains XML context specifications for AI coding agents working on the monoio/io_uring server implementation.

## Context Files

| Issue | Context File | Description | Status |
|-------|--------------|-------------|--------|
| #27 | [monoio-01-observability-context.xml](./monoio-01-observability-context.xml) | Tracing/metrics integration | ✅ Closed |
| #30 | [monoio-02-http2-context.xml](./monoio-02-http2-context.xml) | HTTP/2 support via monoio-http | Open |
| #28 | [monoio-03-buffer-pool-context.xml](./monoio-03-buffer-pool-context.xml) | Buffer pooling & registered buffers | Open |
| #29 | [monoio-04-multishot-context.xml](./monoio-04-multishot-context.xml) | Multishot io_uring operations | Open |
| #26 | [monoio-05-cancellation-context.xml](./monoio-05-cancellation-context.xml) | **CRITICAL: Cancellation safety audit** | ✅ Closed |
| #31 | [monoio-06-integration-context.xml](./monoio-06-integration-context.xml) | Main crate integration | Open |
| #32 | [monoio-07-benchmarks-context.xml](./monoio-07-benchmarks-context.xml) | Benchmark parity | Open |
| #33 | [monoio-08-testing-context.xml](./monoio-08-testing-context.xml) | Testing infrastructure | Open |

## Usage

These XML files are designed to be copied into the description field of a task when assigning to an AI coding agent:

```bash
# Example: Using with Kimi CLI agent:kimi
kimi agent -p "$(cat .github/issues/monoio-05-cancellation-context.xml)"
```

Or paste the XML content directly into a coding agent's context window.

## Context Structure

Each XML file follows the Precise Context Specification:

- **Instructions**: What to do, objectives, constraints
- **Context**:
  - `tree`: Project structure for navigation
  - `files`: Files needed (full, codemap, or slice modes)
  - `dependencies`: External APIs/crates as codemaps
- **Guidance**: Role, style, implementation notes

## File Selection Modes

| Mode | Use For |
|------|---------|
| `full` | Files being actively modified |
| `codemap` | Reference files (signatures/types only) |
| `slice` | Large files, specific line ranges |

## Recommended Agent Assignment Order

1. **#26 Cancellation Safety** - Critical safety fix ✅
2. **#27 Observability** - Foundation for measurement
3. **#28 Buffer Pool** → **#29 Multishot** - Performance core (linked)
4. **#32 Benchmarks** - Validate optimizations
5. **#33 Testing Infrastructure** - Quality assurance
6. **#30 HTTP/2** + **#31 Integration** - Completeness
