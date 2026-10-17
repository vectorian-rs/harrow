# Protocol Backend Strategy

**Status:** current decision checkpoint for the 1.0 line  
**Date:** 2026-04-30

Harrow currently owns a custom HTTP/1 stack for its server backends. That gives
us control over local-worker scheduling, request-body backpressure, and runtime
experiments, but it also makes Harrow responsible for protocol correctness that
mature servers normally delegate to Hyper or another battle-tested HTTP engine.

This document records the tradeoff and validation path: keep the custom
codec/dispatcher work as a reference path, and evaluate the additive
`harrow-server-tokio-hyper` prototype before committing to the custom H1 stack
as the stable 1.0 production path.

## Current Custom H1 Surface

Approximate source size of the custom HTTP/1 server path at this checkpoint:

| Area | Approx. total LOC | Approx. code-ish LOC |
| --- | ---: | ---: |
| `harrow-codec-h1` parser/framing/body codec | 1,383 | 1,127 |
| shared H1 response/lifecycle helpers in `harrow-server` | 921 | 823 |
| Tokio custom H1 transport | 1,846 | 1,599 |
| Monoio custom H1 transport | 2,251 | 1,575 |
| Meguri direct io_uring H1 transport | 2,927 | 2,414 |
| **Total custom H1 surface** | **~9.3k** | **~7.5k** |

Not every line is equally risky, but the risky categories are exactly the ones
that HTTP clients, proxies, and attackers exercise:

- request-line and header parsing;
- header size limits;
- `Content-Length` validation;
- `Transfer-Encoding` validation;
- chunked decoding;
- `Expect` handling;
- request smuggling resistance;
- response framing and hop-by-hop header normalization;
- keep-alive, close, pipelining, and EOF semantics;
- timeout and shutdown interactions;
- parity across Tokio, Monoio, and Meguri.

Safe Rust helps with memory safety, but it does not make HTTP semantics correct.
Because Harrow owns this stack, Harrow also owns the fuzzing, adversarial tests,
interop testing, and security review needed to ship it confidently.

## What Hyper Changes

A Hyper-based Tokio backend would still use Harrow's application model:

```text
Harrow App / Router / Middleware / Request / Response
        |
        v
Tokio Hyper server adapter
```

But Hyper would own most HTTP/1 and HTTP/2 protocol machinery:

- HTTP/1 request parsing;
- chunked decoding;
- transfer-coding and message framing rules;
- keep-alive and connection-close handling;
- response framing;
- pipelining behavior;
- upgrades;
- HTTP/2 protocol support through the Hyper/h2 stack.

Harrow would still maintain backend glue:

- mapping Hyper requests/responses to Harrow types;
- server configuration;
- graceful shutdown;
- connection limits;
- observability hooks;
- optional TLS/ALPN wiring;
- worker topology and benchmarking profiles.

That is a much smaller and less security-sensitive maintenance surface than a
custom HTTP parser/framer across multiple runtimes.

## Why Thread-Per-Core Still Matters

The relevant lesson from Tako's thread-per-core Hyper work is that much of the
performance upside may come from runtime topology rather than custom HTTP byte
handling:

```text
one OS thread per worker
one current-thread Tokio runtime per worker
local connection tasks
kernel distribution with SO_REUSEPORT where supported
optional CPU affinity
worker-local state
minimal hot-path refcount churn
```

Harrow can evaluate that topology without giving up the custom codec reference
path. The `harrow-server-tokio-hyper` prototype combines Hyper's protocol correctness
with Harrow's explicit server lifecycle and per-worker deployment model. The
first version is HTTP/1-focused; HTTP/2 and TLS/ALPN are follow-up work for the
same backend family.

## 1.0 Strategy Decision

For 1.0, the safest path may be:

- make a Hyper-based Tokio backend the candidate stable production backend if
  performance is close enough;
- keep the custom H1 stack as a reference/experimental high-control path until
  its hardening, fuzzing, and parity work is complete;
- continue treating Meguri as experimental unless it passes the same lifecycle,
  protocol, and unsafe-code review bar;
- use measured benchmark data, not architecture preference, to decide whether
  custom H1 should be stable by default.

This is not a decision to delete `harrow-codec-h1`. The custom path remains
valuable as:

- a reference implementation for Harrow-owned protocol policy;
- a path for Monoio/io_uring and Meguri experiments;
- a way to test local-worker and backpressure designs without Hyper's server
  abstraction boundaries;
- a possible future performance backend once its correctness evidence is strong.

## Hyper Backend Prototype Acceptance Criteria

Before changing the public support policy, Harrow should finish and measure the
Tokio Hyper backend with at least:

- Harrow `App`/router/middleware dispatch;
- HTTP/1 support; **first prototype implemented**
- HTTP/2 support plan or direct implementation path;
- current-thread/thread-per-core mode; **first prototype implemented**
- optional `SO_REUSEPORT` on supported Unix platforms; **first prototype implemented**
- graceful shutdown and drain timeout; **first prototype implemented**
- connection limits or a documented first prototype gap;
- request/response body mapping compatible with Harrow's public APIs;
- basic observability hooks;
- benchmark profile alongside current custom Tokio, Monoio, Meguri, Tako, and
  other baselines.

## Decision Gate

After the prototype exists, compare:

| Candidate | Question |
| --- | --- |
| Tokio + Hyper multithread | What is the boring baseline? |
| Tokio + Hyper thread-per-core | How much does worker topology buy us? |
| Tokio + custom H1 | How much does custom protocol ownership buy us? |
| Monoio custom H1/H2 | Is Linux io_uring performance worth the parity cost? |
| Meguri | Is direct io_uring worth stabilizing or should it remain experimental? |
| Tako thread-per-core | External reference for Hyper + per-thread topology |

If Hyper thread-per-core is close to Harrow's performance target, prefer it as
the stable 1.0 Tokio backend and keep custom H1 behind an explicit experimental
or advanced-performance support label. If custom H1 is materially faster, keep
it on the table but require the adversarial test/fuzz/lifecycle evidence before
calling it production-stable.
