
 ┌──────────────────────┬─────────────┬─────────────┬────────┬────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
 │ Crate                │ Correctness │ Performance │ Safety │ One-line assessment                                                                                            │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow               │ 7/10        │ 7/10        │ 7/10   │ Thin public API is clean, but feature/backends need more 1.0 matrix testing and HTTP/2 policy completion.      │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-core          │ 7/10        │ 7/10        │ 8/10   │ Core routing/request/response model is simple and mostly safe; request-helper edge cases and route semantics   │
 │                      │             │             │        │ need stronger adversarial tests.                                                                               │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-codec-h1      │ 7/10        │ 8/10        │ 7/10   │ Good custom H1 foundation with limits/fuzz targets; TE/chunked/response-framing strictness still needs         │
 │                      │             │             │        │ hardening.                                                                                                     │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-server        │ 7/10        │ 7/10        │ 7/10   │ Useful shared H1/lifecycle/control primitives; lifecycle model must be wired to or validated against real      │
 │                      │             │             │        │ backend behavior.                                                                                              │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-server-tokio  │ 7/10        │ 8/10        │ 7/10   │ Strongest production backend today; H1 custom path is credible, but HTTP/2 and more adversarial lifecycle      │
 │                      │             │             │        │ tests are missing.                                                                                             │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-server-monoio │ 6/10        │ 8/10        │ 6/10   │ High-performance direction is good; H2 is partial, cancellation/io_uring semantics require continued scrutiny. │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-server-meguri │ 5/10        │ 8/10        │ 4/10   │ Interesting experimental direct io_uring backend; unsafe surface and protocol parity are not production-stable │
 │                      │             │             │        │ yet.                                                                                                           │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-middleware    │ 7/10        │ 7/10        │ 7/10   │ Good operational middleware set; auth/CSRF/idempotency absent and middleware ordering needs more documented    │
 │                      │             │             │        │ test guarantees.                                                                                               │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-o11y          │ 7/10        │ 7/10        │ 7/10   │ Small, clear config wrapper; metrics story is integration-oriented, not full backend.                          │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-serde         │ 8/10        │ 8/10        │ 8/10   │ Small JSON/MessagePack helper crate with useful buffer reuse; SIMD/zero-copy claims should remain out of scope │
 │                      │             │             │        │ until benchmarked.                                                                                             │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-io            │ 7/10        │ 8/10        │ 7/10   │ Runtime-neutral buffer pooling is appropriate; pool bounds/lifecycle should be included in aggregate memory    │
 │                      │             │             │        │ docs.                                                                                                          │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ harrow-bench         │ 6/10        │ 7/10        │ 6/10   │ Useful benchmark harness, but not a production crate; needs fresh post-refactor results and Docker validation. │
 ├──────────────────────┼─────────────┼─────────────┼────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ meguri               │ 5/10        │ 8/10        │ 4/10   │ Ambitious io_uring library; safety claims need deep unsafe audit, Linux-only test coverage, and cancellation   │
 │                      │             │             │        │ proof.                                                                                                         │
 └──────────────────────┴─────────────┴─────────────┴────────┴────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘

 Findings ordered by severity

 1. High: HTTP/2 is now a 1.0 target but not implemented across public backends

 ### Why it matters

 The docs now make HTTP/2 backend parity a 1.0 target. Current state does not satisfy that.

 ### Code concern

 - harrow-server-tokio: no H2 server path.
 - harrow-server-monoio: H2 code exists but needs stabilization.
 - harrow-server-meguri: no H2 and likely too expensive to make first-class soon.

 ### Concrete improvement

 Track and complete:

 - #72 feat(server-tokio): add HTTP/2 support for 1.0 parity
 - #73 feat(server-monoio): stabilize HTTP/2 support for 1.0
 - #74 feat(server-meguri): decide and implement HTTP/2 parity or keep experimental
 - #75 test(server): add cross-backend HTTP/2 parity suite

 For 1.0, I would define stable support as:

 ```text
   Tokio + Monoio: HTTP/2 request/response APIs stable.
   Meguri: explicitly experimental unless H2 lands.
 ```

 ────────────────────────────────────────────────────────────────────────────────

 2. High: Transfer-Encoding parser still needs smuggling-hardening

 ### Why it matters

 Request smuggling risk is one of the central costs of owning H1 instead of using Hyper. Content-Length + Transfer-Encoding rejection is good, but TE parsing should be
 stricter.

 ### Code concern

 harrow-codec-h1/src/lib.rs currently accepts/handles TE values with more tolerance than I’d want for a strict production backend. Existing tests even say:

 ```rust
   fn chunked_in_comma_list_with_identity()
 ```

 This should probably become rejection, not acceptance.

 ### Concrete improvement

 Issue:

 ```text
   #77 fix(h1): make Transfer-Encoding parsing strict for smuggling resistance
 ```

 Recommended policy:

 ```text
   Accept only:
   Transfer-Encoding: chunked

   Reject:
   Transfer-Encoding: identity
   Transfer-Encoding: gzip
   Transfer-Encoding: gzip, chunked
   Transfer-Encoding: chunked, gzip
   HTTP/1.0 + Transfer-Encoding
   invalid/non-ASCII TE values
   TE + CL
 ```

 ────────────────────────────────────────────────────────────────────────────────

 3. High: Response framing normalization needs stronger guarantees

 ### Why it matters

 Harrow owns H1 response serialization. App-supplied Transfer-Encoding, Content-Length, or hop-by-hop headers can cause invalid or ambiguous framing if not normalized
 centrally.

 ### Code concern

 Relevant code:

 - harrow-server/src/h1.rs
 - harrow-codec-h1/src/lib.rs
 - backend response writers

 Current code adds chunked framing if selected, but should explicitly prevent duplicate/conflicting user-supplied framing.

 ### Concrete improvement

 Issue:

 ```text
   #78 fix(h1): normalize response framing and hop-by-hop headers
 ```

 Acceptance should include tests for:

 - app-set Transfer-Encoding;
 - app-set Content-Length + Transfer-Encoding;
 - wrong fixed Content-Length;
 - HEAD;
 - 204;
 - 304;
 - streaming responses.

 ────────────────────────────────────────────────────────────────────────────────

 4. High: Meguri unsafe/io_uring path needs a formal safety audit

 ### Why it matters

 Tokio/Monoio server code is mostly safe Rust. Meguri necessarily uses raw fd/io_uring operations and unsafe buffer manipulation. That is the highest memory-safety and
 lifecycle-risk surface in the workspace.

 ### Code concern

 Relevant areas:

 - harrow-server-meguri/src/lib.rs
 - harrow-server-meguri/src/connection.rs
 - meguri/src/*

 Examples include:

 - raw libc::socket, setsockopt, fcntl;
 - raw fd ownership;
 - BytesMut::set_len;
 - SQE buffer lifetime;
 - stale slab index/generation behavior.

 ### Concrete improvement

 Issue:

 ```text
   #82 chore(meguri): audit unsafe blocks and syscall error handling
 ```

 Every production unsafe block should have a // SAFETY: comment with concrete invariants.

 ────────────────────────────────────────────────────────────────────────────────

 5. Medium: Lifecycle model can drift from real backend behavior

 ### Why it matters

 harrow-server/src/h1_lifecycle.rs is valuable, but if backends do not consume it or replay traces against it, it can become documentation rather than verification.

 ### Code concern

 h1_lifecycle::Machine appears tested in isolation. The actual Tokio/Monoio/Meguri loops do not appear to use it directly.

 ### Concrete improvement

 Issue:

 ```text
   #80 test(h1): connect lifecycle model to backend behavior
 ```

 Either:

 1. use Machine in backend transitions; or
 2. instrument backend tests to emit event traces and replay through Machine.

 ────────────────────────────────────────────────────────────────────────────────

 6. Medium: Legacy stateless chunked decoder is less strict than stateful decoder

 ### Why it matters

 Even if the live path uses the stateful decoder, public or compatibility helpers should not accept malformed chunked framing.

 ### Code concern

 decode_chunked_with_limit advances past CRLF after chunk data without validating it.

 ### Concrete improvement

 Issue:

 ```text
   #79 fix(h1): align or remove legacy stateless chunked decoder
 ```

 Either remove/deprecate it or make it strict.

 ────────────────────────────────────────────────────────────────────────────────

 7. Medium: Aggregate memory model is not documented enough

 ### Why it matters

 Per-request queues are bounded, but aggregate memory under adversarial concurrency depends on:

 ```text
   max_connections
   read buffer size
   header buffer limit
   request body queue size
   response buffer size
   max body size
   app buffering
 ```

 ### Code concern

 Constants are spread across:

 - harrow-codec-h1
 - harrow-server-tokio
 - harrow-server-monoio
 - harrow-server-meguri
 - harrow-io

 ### Concrete improvement

 Issue:

 ```text
   #81 docs(server): document aggregate H1 memory limits and expose tunables where needed
 ```

 This is necessary before serious production claims.

 ────────────────────────────────────────────────────────────────────────────────

 8. Medium: Unsupported Expect headers need explicit behavior

 ### Why it matters

 Expect: 100-continue is handled. Unsupported expectations should not be silently ignored.

 ### Code concern

 harrow-codec-h1/src/lib.rs detects only 100-continue.

 ### Concrete improvement

 Issue:

 ```text
   #83 fix(h1): handle unsupported Expect headers explicitly
 ```

 Prefer:

 ```text
   Expect: 100-continue -> supported
   other Expect values  -> 417 or 400, documented
 ```

 ────────────────────────────────────────────────────────────────────────────────

 9. Low/medium: Tokio request-head path has avoidable ParsedRequest allocation

 ### Why it matters

 This is a hot path allocation and easy to remove unless enum-size concerns justify it.

 ### Code concern

 harrow-server-tokio/src/h1/request_head.rs:

 ```rust
   RequestHeadRead::Parsed(Box<ParsedRequest>)
 ```

 Caller immediately unboxes it.

 ### Concrete improvement

 Issue:

 ```text
   #84 perf(h1): remove unnecessary ParsedRequest boxing in Tokio request-head path
 ```

 ────────────────────────────────────────────────────────────────────────────────

 10. Low/medium: Request-body queue semantics are duplicated across backends

 ### Why it matters

 Tokio, Monoio, and Meguri have similar but separate body queue/pump logic. Some divergence is expected, but semantics need mirrored tests.

 ### Code concern

 Relevant files:

 - harrow-server-tokio/src/h1/request_body.rs
 - harrow-server-monoio/src/h1/request_body.rs
 - harrow-server-meguri/src/connection.rs

 ### Concrete improvement

 Issue:

 ```text
   #85 refactor(h1): reduce request-body queue divergence across backends
 ```

 Do not over-abstract prematurely. First add mirrored tests.

 ────────────────────────────────────────────────────────────────────────────────

 Verification gaps

 Existing positive signals

 - cargo check --workspace has passed recently.
 - cargo clippy on core H1/server crates passed recently.
 - cargo test -p harrow-codec-h1 passed after the header-limit fix.
 - Fuzz targets exist for H1 codec paths.
 - Integration tests exist for Tokio, Monoio, and Meguri.

 Required before 1.0

 Issues:

 ```text
   #86 test(h1): add adversarial parser, framing, and lifecycle coverage before 1.0
   #87 fuzz(h1): run and document codec fuzz campaigns before 1.0
 ```

 Minimum commands:

 ```sh
   cargo fmt --all
   cargo check --workspace
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   mise run fuzz:check
 ```

 Then targeted fuzz campaigns:

 ```sh
   cargo fuzz run --manifest-path harrow-codec-h1/fuzz/Cargo.toml fuzz_parse_request
   cargo fuzz run --manifest-path harrow-codec-h1/fuzz/Cargo.toml fuzz_chunked_decode
   cargo fuzz run --manifest-path harrow-codec-h1/fuzz/Cargo.toml fuzz_payload_decoder
   cargo fuzz run --manifest-path harrow-codec-h1/fuzz/Cargo.toml fuzz_roundtrip
 ```

 ────────────────────────────────────────────────────────────────────────────────

 3 strongest properties

 1. Clear explicit app model
 App, Request, Response, middleware, and state are simple and framework-owned.
 2. Custom H1 ownership is structured, not ad hoc
 There is a dedicated codec crate, shared H1 helper crate, lifecycle model, and backend-specific transport layers.
 3. Good direction on operational hardening
 Timeouts, body limits, request IDs, security headers, graceful shutdown, fuzz targets, and docs are all present or actively tracked.

 ────────────────────────────────────────────────────────────────────────────────

 3 weakest properties

 1. HTTP/2 parity is not yet real
 It is now a 1.0 target but not implemented/stabilized across public backends.
 2. Custom H1 still needs adversarial hardening
 TE strictness, response framing normalization, chunked legacy behavior, and lifecycle trace validation remain open.
 3. Meguri is not production-grade yet
 It has the highest unsafe/io_uring surface and should remain experimental until audited and parity-tested.

