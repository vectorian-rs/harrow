# Harrow Documentation

This is the entrypoint for Harrow's current documentation. The docs are arranged
by subject. Each subject folder has its own `index.md` with a short map and links
to the documents in that area.

If a document is in [History](./history/), treat it as archaeology unless a
current doc links to it as supporting evidence.

## Subject Areas

- [Overview](./overview/): product identity, feature status, and roadmap.
- [Product](./product/): product requirements and release scope.
- [Architecture](./architecture/): runtime model, protocol design, and public API design.
- [Server](./server/): backend support, lifecycle, timeouts, limits, and connection safety.
- [Middleware](./middleware/): middleware model, security middleware, and observability.
- [Operations](./operations/): deployment, performance workflow, and verification strategy.
- [History](./history/): engineering journal, old strategy notes, reviews, and benchmark archaeology.

## High-Signal Current Docs

Start here for the most important current docs:

1. [What Harrow Is](./overview/what-is-harrow.md)
2. [Harrow 1.0 PRD](./product/harrow-1.0.md)
3. [Local-Worker Webservers](./architecture/runtime-model/local-worker-webservers.md)
4. [Protocol Backend Strategy](./architecture/protocol/backend-strategy.md)
5. [Server Lifecycle](./server/lifecycle.md)
6. [Backend Support](./server/backend-support.md)
7. [Performance](./operations/performance.md)
8. [Verification](./operations/verification.md)

## Recommended Reading By Task

### Understand Harrow

1. [Overview](./overview/)
2. [What Harrow Is](./overview/what-is-harrow.md)
3. [Feature Status](./overview/features.md)
4. [Roadmap](./overview/roadmap.md)
5. [README](../README.md)

### Runtime, async, or server architecture

1. [Architecture](./architecture/)
2. [Local-Worker Webservers](./architecture/runtime-model/local-worker-webservers.md)
3. [Server Lifecycle](./server/lifecycle.md)
4. [Protocol Backend Strategy](./architecture/protocol/backend-strategy.md)
5. [HTTP/1 Dispatcher Design](./architecture/protocol/h1-dispatcher-design.md)
6. [Connection Safety](./server/connection-safety.md)

### Backend or lifecycle work

1. [Server](./server/)
2. [Backend Support](./server/backend-support.md)
3. [Server Lifecycle](./server/lifecycle.md)
4. [Connection Safety](./server/connection-safety.md)
5. [Verification](./operations/verification.md)

### Public API or handler model work

1. [API Architecture](./architecture/api/)
2. [Request Helpers](./architecture/api/request-helpers.md)
3. [Explicit Extractors](./architecture/api/explicit-extractors.md)
4. [README](../README.md)

### Middleware, security, or observability work

1. [Middleware](./middleware/)
2. [Middleware Architecture](./middleware/middleware.md)
3. [Security](./middleware/security.md)
4. [Observability](./middleware/observability.md)

### Deployment or operations work

1. [Operations](./operations/)
2. [Deployment](./operations/deployment.md)
3. [Server Lifecycle](./server/lifecycle.md)
4. [Observability](./middleware/observability.md)
5. [Security](./middleware/security.md)

### Performance work

1. [Performance](./operations/performance.md)
2. [Local-Worker Webservers](./architecture/runtime-model/local-worker-webservers.md)
3. [Protocol Backend Strategy](./architecture/protocol/backend-strategy.md)
4. [Backend Support](./server/backend-support.md)
5. [History](./history/) only when investigating old benchmark runs or design archaeology.

## Documentation Rules

- Current source-of-truth docs live under the subject folders above.
- Historical docs live under [History](./history/).
- Do not treat historical docs as current product commitments unless a current
  doc explicitly says so.
- Large generated benchmark or flamegraph artifacts should stay out of the
  default reading path.
