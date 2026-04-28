# Documentation Index

This is the docs entrypoint for Harrow. Start here, then open the smallest set
of documents needed for the task.

## Current Docs Set

- [What Harrow Is](./what-is-harrow.md): short product identity and non-goals.
- [Plan and Status](./roadmap.md): current 0.10 -> 1.0 implementation plan.
- [Harrow 1.0 PRD](./prds/harrow-1.0.md): detailed product/support source of truth.
- [Backend Support](./backend-support.md): Tokio, Monoio, and Meguri support matrix.
- [Server Lifecycle](./server-lifecycle.md): startup, workers, timeouts, limits, graceful shutdown.
- [Deployment](./deployment.md): production deployment notes for Tokio and Monoio.
- [Request Helpers](./request-helpers.md): explicit request-first handler model.
- [Observability](./observability.md): tracing, request IDs, route labels, metrics status.
- [Security](./security.md): security-related middleware and operational guidance.
- [Middleware](./middleware.md): middleware architecture and available middleware.
- [Performance](./performance.md): benchmark workflow and current measured conclusions.
- [Verification](./verification.md): test/fuzz/model-checking strategy.
- [Connection Safety](./connection-safety.md): transport-level timeout/limit design.
- [HTTP/1 Dispatcher Design](./h1-dispatcher-design.md): current H1 backend architecture.
- [Article](./article.md): engineering progress journal and historical narrative.

## Recommended Reading By Task

### Understand Harrow

1. [What Harrow Is](./what-is-harrow.md)
2. [Plan and Status](./roadmap.md)
3. [Backend Support](./backend-support.md)
4. [README](../README.md)

### Backend or server lifecycle work

1. [Backend Support](./backend-support.md)
2. [Server Lifecycle](./server-lifecycle.md)
3. [Connection Safety](./connection-safety.md)
4. [HTTP/1 Dispatcher Design](./h1-dispatcher-design.md)
5. [Verification](./verification.md)

### Public API / handler model work

1. [Request Helpers](./request-helpers.md)
2. [Explicit Extractors](./explicit-extractors.md)
3. [README](../README.md)

### Middleware or security work

1. [Middleware](./middleware.md)
2. [Security](./security.md)
3. [Observability](./observability.md) if tracing/request IDs are involved

### Deployment or operations work

1. [Deployment](./deployment.md)
2. [Server Lifecycle](./server-lifecycle.md)
3. [Observability](./observability.md)
4. [Security](./security.md)

### Performance work

1. [Performance](./performance.md)
2. [Backend Support](./backend-support.md)
3. [Connection Safety](./connection-safety.md)
4. Historical profiling docs in [old](./old/) only if needed

## Historical / Uncertain Docs

Docs that are useful for archaeology but not current source-of-truth have been
moved to [docs/old](./old/). They include old strategy notes, external review
notes, old H2 design notes, profiling setup notes, and visual assets.

Do not treat files in `docs/old/` as current product commitments unless a
current doc explicitly references them.
