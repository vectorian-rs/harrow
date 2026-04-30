# Backend Support

Harrow does not hide a server runtime behind a default feature. Applications
choose a backend explicitly with Cargo features or by depending on a backend
crate directly.

## Support Levels

| Backend | Crate | Root `harrow` API | Support level | Platform |
| --- | --- | --- | --- | --- |
| Tokio custom HTTP/1 | `harrow-server-tokio` | `harrow::runtime::tokio` and root `serve*` in single-backend mode | Public today; stable-by-default status under review | Linux, macOS, Windows |
| Tokio Hyper prototype | `harrow-server-tokio-hyper` | `harrow::runtime::tokio_hyper` and root `serve*` in single-backend mode with `tokio-hyper` | Prototype candidate for stable 1.0 Tokio path | Linux, macOS, Windows |
| Monoio/io_uring | `harrow-server-monoio` | `harrow::runtime::monoio` and root `run*` in single-backend mode | Public Linux backend; parity evidence pending for final 1.0 label | Linux 6.1+ recommended |
| Meguri direct io_uring | `harrow-server-meguri` | Not re-exported | Experimental | Linux only |

## Capability Matrix

| Capability | Tokio custom H1 | Tokio Hyper prototype | Monoio | Meguri |
| --- | --- | --- | --- | --- |
| HTTP/1.1 | Public today; Harrow-owned protocol stack | Prototype implemented through Hyper | Public today; Harrow-owned protocol stack | Experimental |
| Custom Harrow H1 path | Yes | No; Hyper owns protocol | Yes | Yes |
| HTTP/2 | Planned for 1.0; not implemented yet | Preferred evaluation path; not wired in first prototype | Partial/backend-local implementation and tests; needs stabilization | Planned for parity before stabilization |
| HTTP/3 / QUIC | No | No | No | No |
| WebSocket | Tokio feature (`ws`) | To evaluate | Not public root API | No |
| SSE / streaming responses | Framework response API; backend support expected through normal responses | To evaluate through Hyper bodies | Framework response API; verify per use case | Experimental |
| TLS | Tokio-oriented feature surface; reverse proxy termination also supported | To evaluate; likely Hyper + rustls/ALPN path | Prefer reverse proxy/load balancer termination unless documented otherwise | No public support |
| Graceful shutdown | Yes | Prototype implemented | Yes | Yes, experimental |
| Multi-worker mode | Yes | Prototype implemented with thread-per-core/current-thread + `SO_REUSEPORT` | Yes | Yes |
| Root crate re-export | Yes | No; not implemented | Yes | No |
| Recommended for 1.0 production | Under review | Candidate if benchmarks are close | Under review for Linux/io_uring deployments | No |

## Tokio

Use Tokio when you want the most portable and general-purpose Harrow backend today:

- local development on macOS, Windows, or Linux;
- Docker/container deployments;
- environments where io_uring is unavailable or blocked;
- WebSocket support through the `ws` feature.

```toml
harrow = { version = "0.10", features = ["tokio"] }
tokio = { version = "1", features = ["full"] }
```

## Tokio Hyper Prototype

Before the 1.0 backend policy is finalized, Harrow should evaluate the
`harrow-server-tokio-hyper` prototype. The goal is to test whether Hyper plus a
thread-per-core/current-thread worker topology gets close enough to Harrow's
performance target while removing most custom HTTP/1 protocol maintenance from
the stable production path. The first prototype supports Hyper HTTP/1 and the
same Harrow app/router dispatch model; HTTP/2, TLS/ALPN, WebSocket parity, and
benchmark harness integration still need follow-up work.

See [Protocol Backend Strategy](./protocol-backend-strategy.md) for the
maintenance tradeoff and decision gate.

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

HTTP/2 is now a 1.0 target for Harrow's server backends. HTTP/1.1 remains the
most mature path today, but the 1.0 support story should not leave HTTP/2 as a
Monoio-only or backend-local capability. The Tokio Hyper prototype may become
the preferred Tokio HTTP/2 path if it satisfies Harrow's lifecycle and
performance requirements.

Current status:

- Monoio has partial HTTP/2 implementation and tests in the backend crate.
- Tokio custom HTTP/1 does not yet expose an HTTP/2 server path.
- Tokio Hyper prototype exists for HTTP/1 and is intended to evaluate whether Hyper should own Tokio HTTP/1+HTTP/2 protocol handling for the stable path.
- Meguri does not yet support HTTP/2 and remains experimental.

Before 1.0, Harrow should either provide HTTP/2 parity across the server
backends or explicitly downgrade any backend that cannot meet that support bar.
HTTP/3/QUIC remains out of scope for 1.0.
