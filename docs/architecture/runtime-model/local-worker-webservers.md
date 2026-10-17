# Local-Worker Webservers

**Status:** Current architecture direction  
**Scope:** Harrow server backends, especially Tokio custom H1 / Tokio Hyper worker-topology experiments and Monoio

Harrow's performance-sensitive server backends are moving toward a local-worker
webserver model. This is the useful part of the nginx/`ntex` design space:
multiple workers, each with local ownership of accepted connections and protocol
state, instead of a single global work-stealing execution fabric for the hot HTTP
path.

This does **not** mean Harrow is cloning `ntex`'s public API or service stack.
It means Harrow is adopting the worker-local runtime and transport principles
that make `ntex` fast while keeping Harrow's request-first API.

## 1. Problem: Generic Work-Stealing Is Not Always the Best HTTP Shape

The default Tokio/Hyper mental model is general-purpose:

- one multi-threaded Tokio runtime;
- accepted connections become tasks;
- tasks may be scheduled across runtime workers;
- shared state and wakeups must be safe across threads;
- ecosystem compatibility is excellent.

That is a good default for many applications. It is not always the best shape
for a high-throughput HTTP server hot path where most connection state naturally
belongs to one place:

- socket readiness;
- parser state;
- request-body buffering;
- handler future state;
- response serialization and writes;
- timeout/lifecycle bookkeeping.

If those pieces bounce across workers, the server pays for cross-thread wakeups,
cache misses, synchronization, and work-stealing machinery in a workload that
usually benefits from locality.

## 2. The nginx/`ntex` Shape

The local-worker model uses N independent workers:

```text
listener / reuseport
        │
        ├── worker 0: runtime/event loop + local connections
        ├── worker 1: runtime/event loop + local connections
        ├── worker 2: runtime/event loop + local connections
        └── worker N: runtime/event loop + local connections
```

Each worker owns its active connections. For a given connection, the same worker
keeps ownership of:

- socket reads and writes;
- HTTP parser/framer state;
- request payload queue;
- app dispatch for that request;
- response body streaming;
- connection lifecycle and timeout state.

The important property is local ownership, not a specific crate. `ntex` showed
that this model applies on Tokio too: its Tokio path uses a local-task runtime
shape with `LocalSet`/local tasks, an explicit HTTP/1 dispatcher, and a local
payload queue.

## 3. Tokio Version: `current_thread` + `LocalSet`

For Tokio, Harrow's target local-worker shape is:

- one OS thread per Harrow worker;
- one Tokio `current_thread` runtime per worker;
- one `LocalSet` per worker for connection/protocol tasks;
- `spawn_local` for worker-local connection and dispatcher work;
- no work-stealing on the hot connection path;
- worker-local buffers and protocol state where possible;
- `SO_REUSEPORT` on platforms where it is available, so workers can accept from
  the same address without a global accept bottleneck.

This keeps Tokio as an operationally familiar backend while avoiding the parts
of a generic multi-threaded runtime that do not help a mostly local connection
state machine.

Tokio remains useful. Harrow is not Tokio-hostile. The direction is to use Tokio
in a webserver-shaped topology instead of assuming the default work-stealing
shape is always the right server architecture.

## 4. Monoio Version: Native Local Workers

Monoio is already aligned with this direction:

- no work-stealing runtime;
- worker-local event loop;
- connection state stays local;
- thread-per-core style deployment is natural.

The main Harrow requirement for Monoio is parity in protocol behavior and
request-body backpressure, not forcing Monoio through Tokio-shaped abstractions.

## 5. Protocol Consequence: Explicit Dispatchers

A local-worker runtime model only pays off if the protocol layer also preserves
locality. That is why Harrow's custom H1 work introduced an explicit connection
dispatcher boundary.

The dispatcher should own:

- request-head parsing;
- request-body pumping;
- bounded request-body buffering;
- stop-reading backpressure when the handler-side payload queue is full;
- response framing and writing;
- connection keep-alive/lifetime decisions.

`harrow-core` should continue to own application dispatch: route matching,
middleware, handlers, request/response types, and state access. Runtime and
socket integration stay in backend crates.

The split is:

```text
harrow-core
  application dispatch: routes, middleware, handlers

H1 dispatcher / backend protocol layer
  connection state, parser/framer, payload backpressure, response writes

backend runtime crate
  Tokio, Hyper experiment, Monoio, socket integration, worker lifecycle
```

See [HTTP/1 Dispatcher Design](../protocol/h1-dispatcher-design.md) for the
custom H1 protocol boundary.

## 6. Request-Body Backpressure

The most important `ntex` lesson is not merely `LocalSet`; it is the combination
of local execution with explicit payload backpressure.

The target behavior is:

1. parse the request head;
2. create the request and attach a live payload stream/queue;
3. run the handler while the dispatcher continues to pump body data;
4. keep the body queue byte-bounded;
5. stop reading from the socket when the queue is full;
6. resume reading when the handler drains enough bytes.

This prevents unbounded buffering and avoids coupling a slow upload to global
runtime coordination.

## 7. What Harrow Copies From `ntex`

Harrow should copy these ideas:

- worker-local connection ownership;
- Tokio local-task worker topology where it wins;
- explicit protocol dispatcher state machines;
- request payload attached before the handler runs;
- bounded request-body buffering;
- stop-reading backpressure;
- thin runtime adapters below the protocol loop.

## 8. What Harrow Does Not Copy

Harrow should not blindly copy:

- the full `ntex` service/control/filter abstraction stack;
- `ntex`'s public programming model;
- backend-agnostic traits before Harrow's Tokio and Monoio shapes converge;
- abstractions driven by `ntex` internals rather than Harrow's request-first API.

Harrow's public API remains intentionally small: explicit server backend
selection, concrete route tables, request-first handlers, and feature-gated
middleware.

## 9. Current Status

Current direction:

- Harrow's public docs and README now describe local-worker/runtime architecture
  as a core design point.
- The custom Tokio H1 path is the main implementation vehicle for explicit
  dispatchers, local response writing, and local-worker experiments.
- Monoio already matches the local-worker/thread-per-core runtime shape, but
  backend parity still needs evidence.
- The Tokio Hyper prototype exists to test whether Hyper-owned protocol
  correctness can be combined with a Harrow-controlled current-thread /
  thread-per-core worker topology.

Open product decision:

- Harrow 1.0 has not permanently committed to custom H1 as the stable Tokio
  protocol backend. The current decision gate is documented in
  [Protocol Backend Strategy](../protocol/backend-strategy.md).

## 10. Design Rules

When working on server backends, prefer these rules:

1. Keep a connection on one worker for its active lifetime.
2. Keep parser state, payload state, app dispatch, and response writing local to
   that worker where possible.
3. Bound request-body buffering by bytes, not just request count.
4. Stop reading from sockets when local payload queues are full.
5. Keep application dispatch separate from transport dispatch.
6. Put shared abstractions at protocol boundaries, not above them prematurely.
7. Treat runtime-specific socket and lifecycle code as backend-owned.

## 11. Related Docs

- [Runtime Model index](./index.md)
- [HTTP/1 Dispatcher Design](../protocol/h1-dispatcher-design.md)
- [Protocol Backend Strategy](../protocol/backend-strategy.md)
- [Server Lifecycle](../../server/lifecycle.md)
- [Connection Safety](../../server/connection-safety.md)
- [Performance](../../operations/performance.md)
- [Historical strategy note](../../history/old/strategy-local-workers.md)
