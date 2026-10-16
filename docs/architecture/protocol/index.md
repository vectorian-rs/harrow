# Protocol Architecture

This folder covers Harrow's HTTP protocol/backend strategy and custom HTTP/1
dispatcher design.

- [Protocol Backend Strategy](./backend-strategy.md): Hyper vs custom H1 tradeoff, 1.0 backend decision gate, and benchmark plan.
- [HTTP/1 Dispatcher Design](./h1-dispatcher-design.md): reference design for Harrow's custom H1 dispatcher, streaming bodies, and protocol boundaries.

For runtime topology, read [Local-Worker Webservers](../runtime-model/local-worker-webservers.md).
