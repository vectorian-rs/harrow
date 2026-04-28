# Server Lifecycle and Graceful Shutdown

Harrow's server backends share the same operational goals even though their I/O
loops are runtime-specific:

- bounded connection counts;
- bounded request bodies;
- header/body/lifetime timeouts;
- graceful shutdown with a drain period;
- worker-local execution where possible.

The shared lifecycle and worker-control primitives live in `harrow-server`.
The runtime-specific accept/read/write loops remain in the backend crates.

## Configuration Defaults

The common server defaults are:

| Setting | Default | Meaning |
| --- | ---: | --- |
| `max_connections` | `8192` | Maximum concurrent connections, split across workers in multi-worker mode |
| `header_read_timeout` | `5s` | Time allowed to receive request headers |
| `body_read_timeout` | `30s` | Time allowed to receive request body data |
| `connection_timeout` | `300s` | Maximum lifetime of a connection |
| `drain_timeout` | `30s` | Time allowed for in-flight work during shutdown |
| `max_body_size` | `2 MiB` | Maximum buffered request body size |
| `workers` | available parallelism | Number of worker threads/runtimes |

Backend crates may expose additional settings. For example, Monoio exposes
HTTP/2-related settings in its backend crate.

## Worker Model

Tokio, Monoio, and Meguri all run toward a local-worker model:

- each worker owns its runtime/event loop;
- connection limits are divided across workers;
- `SO_REUSEPORT` is used on supported platforms to let workers accept from the
  same address;
- application state is built once per backend API contract and shared through
  Harrow's state model.

Use `ServerConfig::workers` when you need a fixed worker count. Leave it unset
for CPU-count based defaults.

## Graceful Shutdown

Graceful shutdown follows this shape:

1. stop accepting new connections;
2. signal workers;
3. allow in-flight requests to finish until the drain timeout;
4. close remaining connections;
5. join worker threads/runtimes.

Tokio exposes async server functions and a shutdown-aware entrypoint:

```rust,ignore
harrow::runtime::tokio::serve_with_shutdown(app, addr, shutdown_future).await?;
```

Monoio's root API exposes high-level blocking bootstraps:

```rust,ignore
harrow::runtime::monoio::run(|| app.clone(), addr)?;
```

For advanced Monoio lifecycle control, depend on `harrow-server-monoio`
directly and use its `start*` APIs and `ServerHandle`.

## Timeouts and Limits

Timeouts are part of Harrow's abuse-defense model:

- header timeout protects against slowloris-style clients;
- body timeout protects upload/body reads;
- connection lifetime bounds keep-alive resource retention;
- drain timeout prevents shutdown from waiting forever;
- body size limits prevent unbounded memory growth.

At the request helper level, `Request::body_bytes`, `Request::body_json`, and
`Request::body_msgpack` enforce the configured body limit and return
`BodyError::TooLarge` when exceeded.

## Verification

See also:

- [Connection Safety](./connection-safety.md)
- [Verification](./verification.md)
- [HTTP/1 Dispatcher Design](./h1-dispatcher-design.md)
