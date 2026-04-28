# Deployment Guide

This guide covers normal production deployment choices for Harrow's public
backends: Tokio and Monoio.

## Choosing a Backend

Use [Backend Support](./backend-support.md) as the source of truth. In short:

- choose **Tokio** for portability, containers, and general-purpose services;
- choose **Monoio** for Linux deployments where io_uring/thread-per-core is a
  deliberate performance choice.

## Tokio Deployment

Tokio is the recommended default for most services.

```toml
harrow = { version = "0.10", features = ["tokio", "json"] }
tokio = { version = "1", features = ["full"] }
```

Typical production setup:

- run Harrow behind a load balancer or reverse proxy;
- terminate TLS at the proxy/load balancer unless the application has a reason
  to own TLS directly;
- expose `/health`, `/live`, and/or `/ready` routes;
- wire shutdown signals into `serve_with_shutdown`;
- enable request IDs and observability middleware.

## Monoio Deployment

Monoio is for Linux deployments where io_uring is available and desired.

```toml
harrow = { version = "0.10", features = ["monoio", "json"] }
```

Deployment notes:

- Linux 6.1+ is the recommended baseline;
- container seccomp profiles may block io_uring operations;
- validate the detected I/O driver at startup if performance depends on
  io_uring;
- prefer reverse proxy/load balancer TLS termination unless the backend docs say
  otherwise;
- benchmark on the same kernel, instance type, container runtime, and allocator
  you plan to use in production.

The root `harrow` crate exposes `harrow::runtime::monoio::run` and
`run_with_config`. Use `harrow-server-monoio` directly if you need lower-level
server handles.

## TLS

Harrow can be deployed either with in-process TLS where supported or behind a
TLS-terminating proxy/load balancer. For most deployments, external TLS
termination keeps backend behavior simpler and lets the platform own certificate
rotation.

Document the chosen TLS boundary in service docs, especially if enabling HSTS
through security headers.

## Health Checks

Harrow's `App` supports health/readiness-style routes. Prefer separate endpoints
for:

- liveness: process is up;
- readiness: process can serve real traffic;
- dependency-specific checks if needed.

## Shutdown

Use graceful shutdown so deploys and autoscaling events do not drop in-flight
requests unnecessarily. See [Server Lifecycle](./server-lifecycle.md) for the
full model and timeout defaults.

## Observability

For production services, enable:

- request IDs;
- structured logs/traces;
- route labels where possible;
- service/version/environment metadata.

See [Observability](./observability.md).

## Bench Infrastructure vs Deployment

The `infra/` and `harrow-bench/` tooling is for repeatable benchmark runs. It is
not the recommended template for every production deployment. Use the benchmark
infra when measuring Harrow itself; use your platform's normal deployment path
for applications.
