# Server

This folder documents Harrow's server backends and operational lifecycle.

- [Backend Support](./backend-support.md): Tokio, Tokio Hyper prototype, Monoio, and Meguri support matrix.
- [Server Lifecycle](./lifecycle.md): startup, workers, connection limits, timeouts, graceful shutdown, and worker model.
- [Connection Safety](./connection-safety.md): transport-level timeout/limit design and abuse-defense invariants.

For runtime architecture, see [Local-Worker Webservers](../architecture/runtime-model/local-worker-webservers.md).
