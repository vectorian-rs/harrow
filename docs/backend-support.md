# Backend Support

Harrow does not hide a server runtime behind a default feature. Applications
choose a backend explicitly with Cargo features or by depending on a backend
crate directly.

## Support Levels

| Backend | Crate | Root `harrow` API | Support level | Platform |
| --- | --- | --- | --- | --- |
| Tokio custom HTTP/1 | `harrow-server-tokio` | `harrow::runtime::tokio` and root `serve*` in single-backend mode | First-class | Linux, macOS, Windows |
| Monoio/io_uring | `harrow-server-monoio` | `harrow::runtime::monoio` and root `run*` in single-backend mode | First-class Linux backend | Linux 6.1+ recommended |
| Meguri direct io_uring | `harrow-server-meguri` | Not re-exported | Experimental | Linux only |

## Capability Matrix

| Capability | Tokio | Monoio | Meguri |
| --- | --- | --- | --- |
| HTTP/1.1 | First-class | First-class | Experimental |
| Custom Harrow H1 path | Yes | Yes | Yes |
| HTTP/2 | Not a 1.0 public promise | Present in backend crate; policy experimental/narrow | No |
| HTTP/3 / QUIC | No | No | No |
| WebSocket | Tokio feature (`ws`) | Not public root API | No |
| SSE / streaming responses | Framework response API; backend support expected through normal responses | Framework response API; verify per use case | Experimental |
| TLS | Tokio-oriented feature surface; reverse proxy termination also supported | Prefer reverse proxy/load balancer termination unless documented otherwise | No public support |
| Graceful shutdown | Yes | Yes | Yes, experimental |
| Multi-worker mode | Yes | Yes | Yes |
| Root crate re-export | Yes | Yes | No |
| Recommended for 1.0 production | Yes | Yes, on Linux/io_uring deployments | No |

## Tokio

Use Tokio when you want the most portable and general-purpose Harrow backend:

- local development on macOS, Windows, or Linux;
- Docker/container deployments;
- environments where io_uring is unavailable or blocked;
- WebSocket support through the `ws` feature.

```toml
harrow = { version = "0.10", features = ["tokio"] }
tokio = { version = "1", features = ["full"] }
```

## Monoio

Use Monoio when you are intentionally deploying on Linux and want Harrow's
io_uring/thread-per-core path.

```toml
harrow = { version = "0.10", features = ["monoio"] }
```

The root `harrow` crate exposes the high-level `run` / `run_with_config`
surface. Advanced lifecycle control remains available from
`harrow-server-monoio` directly.

## Meguri

Meguri is a workspace backend for direct io_uring experimentation. It is useful
for implementation learning and benchmark comparisons, but it is not part of
Harrow's stable root API for the 1.0 line.

## HTTP/2 Policy

The 1.0 product line is HTTP/1-centered. Monoio has HTTP/2 implementation and
tests in the backend crate, but Harrow should not market broad HTTP/2 support
until the support policy, examples, interoperability expectations, and benchmark
story are explicit.

For now:

- HTTP/1.1 is the stable transport target.
- Monoio HTTP/2 is backend-local and should be treated as experimental/narrow.
- Tokio HTTP/2 is not part of the current custom server surface.
- HTTP/3/QUIC is out of scope for 1.0.
