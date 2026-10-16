# Old Docs

This folder contains historical and superseded docs. Use these for archaeology,
not as source-of-truth product commitments.

Useful historical docs include:

- [Strategy: Local Worker Runtime Model](./strategy-local-workers.md): original local-worker decision note, now superseded by the current [Local-Worker Webservers](../../architecture/runtime-model/local-worker-webservers.md) design doc.
- [Strategy: Thread Per Core](./strategy-tpc.md): broader research note on TPC systems.
- [Strategy: io_uring](./strategy-io-uring.md): lower-level io_uring strategy and safety notes.
- [Middleware Plan](./middleware-plan.md): historical middleware architecture plan.
- [Migration From Axum](./migration-from-axum.md): old migration guide.
- [Old PRDs](./prds/): previous product requirement drafts.
- [Docker Optimization](./docker-optimization.md): March 2026 review of the Docker build setup, from before the move to `docker/` and wrk3.
- [Bench Comparison](./bench_comparison.svg): March 2026 framework comparison chart.

Reviews and profiling docs in this folder may mention paths or behavior that no
longer match the current codebase.
