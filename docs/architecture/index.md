# Architecture

This folder contains current design docs for Harrow's runtime model, protocol
boundaries, and public API shape.

## Sections

- [Runtime Model](./runtime-model/): local-worker webservers, Tokio `current_thread`/`LocalSet`, Monoio thread-per-core alignment, and the `ntex`/nginx ideas Harrow adopts.
- [Protocol](./protocol/): Hyper-vs-custom-H1 strategy and HTTP/1 dispatcher design.
- [API](./api/): request-first handler model and explicit extraction approach.

## Start Here

1. [Local-Worker Webservers](./runtime-model/local-worker-webservers.md)
2. [Protocol Backend Strategy](./protocol/backend-strategy.md)
3. [HTTP/1 Dispatcher Design](./protocol/h1-dispatcher-design.md)
4. [Request Helpers](./api/request-helpers.md)
