# Runtime Model

This folder describes Harrow's async/server execution model: local workers,
Tokio `current_thread` runtimes with `LocalSet`, Monoio thread-per-core
alignment, and the parts of nginx/`ntex` architecture Harrow adopts.

- [Local-Worker Webservers](./local-worker-webservers.md): canonical current design doc for Harrow's local-worker runtime direction and `ntex` influence.

Related docs:

- [Server Lifecycle](../../server/lifecycle.md)
- [Connection Safety](../../server/connection-safety.md)
- [Protocol Backend Strategy](../protocol/backend-strategy.md)
- [HTTP/1 Dispatcher Design](../protocol/h1-dispatcher-design.md)
- [Historical local-worker strategy note](../../history/old/strategy-local-workers.md)
