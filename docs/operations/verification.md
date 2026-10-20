# Verification Strategy

Harrow is a stateless, request-response HTTP framework. It has no replicas, no
consensus protocol, no on-disk durability, and no distributed state machine.
This means the techniques that shine in distributed databases (broad DST with
shadow-state oracles, broad TLA+ specifications, Stateright model checking,
Maelstrom linearizability tests) are largely inapplicable here. Applying them
too broadly would create maintenance burden without catching real bugs.

What harrow *does* have:

- A path-matching engine with three segment types (literal, param, glob).
- A recursive middleware dispatch chain with index arithmetic.
- A GCRA rate limiter with a lock-free CAS loop.
- Compression round-trip logic.
- Query-string parsing with percent-decoding.
- Body-size enforcement at two layers (Content-Length pre-check and frame accumulation).
- A finite shared connection/control state machine in the HTTP/1 backends.

These are the surfaces where bugs hide. The right tools are **property-based
testing** (proptest) for algebraic invariants, **fuzzing** (cargo-fuzz) for
parsing robustness, and **Kani** for bounded verification of small, pure
functions. A *small* deterministic lifecycle harness and a *small* formal model
also make sense where there is a genuine finite state machine to explore
(today: shared/backend-agnostic HTTP/1 connection lifecycle, not the framework
as a whole). Everything else is unit tests.

---

## Technique Applicability Matrix

| Technique | Applicable? | Where |
|-----------|-------------|-------|
| **proptest** | Yes | Path matching, routing, GCRA arithmetic, compression round-trip, middleware ordering |
| **cargo-fuzz** | Yes | Query-string parsing, path matching, Accept-Encoding parsing, HTTP/1 codec decoding |
| **Kani** | Maybe | GCRA single-step correctness, `ns_to_secs_ceil`, path segment classification |
| **DST** | Narrowly | Shared HTTP/1 lifecycle harness with replayable seeded scripts |
| **TLA+ / Quint** | Narrowly | Shared HTTP/1 connection/control lifecycle only (today: TLA+ only) |
| **Stateright** | No | No state-space explosion to explore |
| **Maelstrom** | No | Not a distributed system |

---

## Per-Crate Plan

### harrow-core

The most correctness-critical crate. Three areas warrant investment.

#### 1. Path matching (path.rs) — proptest + fuzz

**Properties to verify (proptest):**

- `match_path` and `matches` agree: if `match_path` returns `Some`, then
  `matches` returns `true`, and vice versa.
- Captured params round-trip: for pattern `/a/:x/:y` and path `/a/foo/bar`,
  `match.get("x") == Some("foo")` and `match.get("y") == Some("bar")`.
- Glob captures remainder: for pattern `/files/*rest` and any path starting
  with `/files/`, the glob value equals the suffix after `/files/`.
- No false positives on literals: a random path that does not start with the
  literal prefix never matches.
- Trailing-slash symmetry: `/users` and `/users/` both match pattern `/users`
  (current behavior — verify it stays consistent).

**Fuzz targets:**

- `fuzz_path_match(pattern: &[u8], path: &[u8])` — parse pattern, match path,
  no panics. Catches edge cases in segment splitting (empty segments, lone
  colons, lone asterisks, non-UTF-8 via lossy conversion).

#### 2. Middleware dispatch chain (dispatch.rs) — proptest

**Properties to verify:**

- Given N global middleware and M route middleware, the handler is called
  exactly once.
- Middleware execute in order: global[0], global[1], ..., route[0], ..., handler.
  Verify by having each middleware append its index to a shared `Vec<usize>`.
- Short-circuit: if middleware K returns early (does not call `next.run`), then
  middleware K+1 through N and the handler are never invoked.
- The fast path (no middleware) produces the same response as the slow path
  with an empty middleware vec.

These are integration-level proptests that construct an `App`, register N
identity middleware, and dispatch a synthetic request.

#### 3. Route table (route.rs) — proptest

**Properties to verify:**

- Any route registered with `app.get(pattern, handler)` is found by
  `match_route_idx(&GET, path)` for every path that the pattern matches.
- A path that matches a pattern but with the wrong method returns `None`
  from `match_route_idx` but `true` from `any_route_matches_path`.
- `allowed_methods` returns exactly the set of methods registered for a path.
- HEAD→GET fallback: a GET-only route is found when queried with HEAD.

#### 4. Query parsing (request.rs) — fuzz

**Fuzz targets:**

- `fuzz_query_pairs(raw_query: &[u8])` — exercise `query_pairs()` and
  `query_param()` with adversarial query strings. Verify no panics, result
  size <= MAX_QUERY_PAIRS (100), and that `query_param(k)` returns a value
  that is present in `query_pairs()`.

---

### harrow-middleware

#### 1. Rate limiter GCRA (rate_limit.rs) — proptest + Kani

**Properties to verify (proptest):**

- Burst property: for a backend with `burst=B`, the first B requests in a
  zero-elapsed-time window are all allowed.
- Rate property: over a long simulated time window, the number of allowed
  requests converges to `rate * elapsed_seconds` (within burst tolerance).
- Independence: two different keys never interfere with each other.
- Monotonicity: `remaining` is non-increasing for consecutive allowed requests
  without time advancement.

**Kani (bounded model checking):**

- `gcra_check` single-step: for all `(old_tat, now, t_ns, tau_ns)` in bounded
  ranges, verify:
  - If allowed, `new_tat > old_tat || old_tat == 0`.
  - `remaining <= burst`.
  - `retry_after_ns == 0` when allowed.
  - `retry_after_ns > 0` when denied.
- `ns_to_secs_ceil`: for all `n: u64` up to 2^32, verify
  `ns_to_secs_ceil(n) * 1_000_000_000 >= n` and
  `(ns_to_secs_ceil(n) - 1) * 1_000_000_000 < n` (unless n == 0).

#### 2. Compression (compression.rs) — proptest + fuzz

**Properties to verify:**

- Round-trip: for any byte sequence `data` where `len >= MIN_COMPRESS_SIZE`,
  `decompress(compress(data)) == data` for gzip and deflate.
- Encoding negotiation: `pick_encoding` prefers br > gzip > deflate > identity.
  Generate random Accept-Encoding strings and verify preference order holds.
- No double-compress: a response with `content-encoding` already set is
  returned unchanged.

**Fuzz targets:**

- `fuzz_accept_encoding(raw_header: &[u8])` — exercise the public
  `compression_middleware` with adversarial `Accept-Encoding` values. Verify no
  panics, only supported `content-encoding` values are emitted, and compressed
  bodies round-trip back to the original payload.

#### 3. CORS (cors.rs) — unit tests sufficient

CORS is header manipulation with well-defined RFC behavior. The existing unit
tests cover preflight, simple requests, and credential handling. No additional
verification technique adds value here.

#### 4. Timeout, catch-panic, body-limit, request-id — unit tests sufficient

These are thin wrappers around Tokio/std primitives. The existing tests are
adequate.

---

### harrow-serde

Delegates entirely to `serde_json` and `rmp_serde`. No harrow-specific logic
to verify beyond the existing round-trip tests.

---

### harrow-server / shared HTTP/1 lifecycle

The runtime-specific read/write loops still belong in the backend crates, but
the lifecycle invariants around:

- request head/body/dispatch/write sequencing
- early response before request EOF
- pipelined keep-alive reuse
- timeout closure vs pending CQEs
- shutdown/drain terminal behavior

are now covered by a shared deterministic lifecycle harness in
`harrow-server::h1_lifecycle` plus the focused TLA+ model in
`specs/tla/ConnectionLifecycle.tla`.

That remains intentionally narrow. We are not trying to simulate Tokio,
Monoio, or Meguri end-to-end. We are modeling the shared protocol lifecycle
boundary that all of those backends rely on.

---

### HTTP/1 adversarial coverage (#86)

Harrow owns its HTTP/1 stack, so request-smuggling resistance, response
framing, and connection lifecycle need explicit adversarial tests. The table
maps each acceptance criterion from issue #86 to the tests that pin it.
End-to-end tests drive the Tokio backend over real TCP and assert on the raw
bytes; codec tests cover pure parsing.

File keys:

- **codec**: `harrow-codec-h1/src/lib.rs` (`tests` module)
- **shared**: `harrow-server/src/h1.rs` (`tests` module)
- **tokio-e2e**: `harrow-server-tokio/tests/integration.rs`. Tests with a
  `h1_adversarial::` prefix are in the `h1_adversarial` module.
- **tokio-dst**: `harrow-server-tokio/tests/dst.rs` (duplex stream, paused time)
- **tokio-resp**: `harrow-server-tokio/src/h1/response.rs` (`tests` module)
- **meguri**: `harrow-server-meguri/src/connection.rs` (`tests` module)
- **monoio**: `harrow-server-monoio/tests/integration.rs` (Linux only)
- **multi**: `harrow-server-tokio/tests/multi_worker.rs`

#### Request parsing / smuggling

| Criterion | Tests |
|---|---|
| Oversized complete headers | codec `reject_complete_headers_over_max_header_buf`; tokio-dst `dst_oversized_headers` |
| Oversized incomplete headers | codec `reject_incomplete_headers_at_max_header_buf`; tokio-e2e `h1_adversarial::tcp_oversized_incomplete_headers_get_400_and_close` |
| Duplicate identical `Content-Length` | codec `accept_duplicate_same_content_length`; tokio-e2e `h1_adversarial::tcp_duplicate_identical_content_length_is_accepted` |
| Duplicate conflicting `Content-Length` | codec `reject_conflicting_content_lengths`; tokio-e2e `h1_adversarial::tcp_conflicting_content_lengths_are_rejected_without_dispatch` |
| `Transfer-Encoding` + `Content-Length` | codec `reject_content_length_and_chunked`, `reject_transfer_encoding_with_content_length`; tokio-e2e `h1_adversarial::tcp_transfer_encoding_with_content_length_is_rejected`; monoio `rejects_content_length_and_chunked_together` |
| Invalid `Content-Length` | codec `reject_invalid_content_length`, `reject_non_digit_content_length_forms`, `content_length_allows_surrounding_ows`; tokio-e2e `h1_adversarial::tcp_invalid_content_length_is_rejected` |
| Invalid or unsupported `Transfer-Encoding` | codec `reject_identity_transfer_encoding`, `reject_comma_separated_transfer_encoding`, `reject_unsupported_transfer_encoding`, `reject_empty_transfer_encoding_token`, `reject_invalid_transfer_encoding_bytes`, `reject_duplicate_transfer_encoding`, `reject_transfer_encoding_with_non_ows_whitespace`; tokio-e2e `h1_adversarial::tcp_invalid_or_unsupported_transfer_encoding_is_rejected` |
| HTTP/1.0 body and framing edge cases | codec `reject_transfer_encoding_on_http10`, `reject_http10_post_without_content_length`, `reject_http10_put_without_content_length`, `allow_http10_get_without_content_length`, `keep_alive_http10_close`, `keep_alive_http10_explicit`; tokio-e2e `h1_adversarial::tcp_http10_post_with_content_length_is_served_then_closed`, `h1_adversarial::tcp_http10_body_without_content_length_or_with_chunked_is_rejected`, `h1_adversarial::tcp_http10_streaming_response_is_not_chunked`, `h1_adversarial::tcp_http10_keep_alive_streaming_response_closes_the_connection`; monoio `http10_streaming_response_is_close_delimited`. The handling of `Expect` on HTTP/1.0 is covered in #95. |

#### Chunked request bodies

| Criterion | Tests |
|---|---|
| Invalid CRLF after chunk body | codec `payload_chunked_rejects_bad_delimiter_after_chunk_data`; tokio-e2e `h1_adversarial::tcp_chunked_body_framing_errors_get_400`. The one-shot decoder is covered in #96. |
| Invalid final CRLF | codec `payload_chunked_rejects_bad_final_crlf`; tokio-e2e `h1_adversarial::tcp_chunked_body_framing_errors_get_400`. The one-shot decoder is covered in #96. |
| Chunk size overflow | codec `payload_chunked_rejects_chunk_size_overflow`; tokio-e2e `h1_adversarial::tcp_chunked_body_framing_errors_get_400`. A chunk-size line with no hex digits is covered in #96. |
| EOF mid-chunk | codec `payload_chunked_eof_mid_chunk_is_incomplete_not_eof`; tokio-e2e `h1_adversarial::tcp_eof_mid_chunk_closes_without_response`; tokio-dst `dst_eof_mid_body` (Content-Length) |
| Body limit exceeded across chunks | codec `payload_chunked_cumulative_max_body`, `decode_chunked_too_large`; tokio-e2e `h1_adversarial::tcp_chunked_body_over_limit_across_chunks_gets_413`; monoio `body_limit_chunked_rejection` |

#### Response writing

| Criterion | Tests |
|---|---|
| App-set `Transfer-Encoding` | shared `prepare_response_removes_app_supplied_transfer_encoding`; tokio-e2e `h1_adversarial::tcp_app_set_transfer_encoding_is_replaced_by_server_framing` |
| App-set `Content-Length` + `Transfer-Encoding` | shared `prepare_response_strips_body_framing_for_204`; tokio-e2e `h1_adversarial::tcp_app_set_content_length_and_transfer_encoding_uses_content_length` |
| Wrong fixed `Content-Length` | shared `prepare_response_rejects_wrong_exact_content_length`, `fixed_response_length_helpers_validate_bounds`; tokio-resp `fixed_length_response_shorter_than_declared_errors`; meguri `stream_response_fixed_length_mismatch_reports_error`; tokio-e2e `h1_adversarial::tcp_wrong_fixed_content_length_closes_without_writing_a_response` |
| `HEAD`, `204`, `304` body suppression | shared `response_body_rule_matches_http_bodyless_cases`, `prepare_response_preserves_head_content_length`; tokio-e2e `tcp_head_response_is_not_chunked_on_wire`, `tcp_204_and_304_responses_are_bodyless_on_wire`, `tcp_head_generated_404_and_405_strip_body`, `h1_adversarial::tcp_bodyless_responses_keep_pipelined_connection_in_sync`; meguri `stream_response_omits_chunked_for_bodyless_status`, `stream_response_omits_head_body_bytes`; monoio `head_response_has_no_body_or_chunked_framing` |
| Streaming response error mid-body | tokio-e2e `h1_adversarial::tcp_streaming_response_error_mid_body_truncates_and_closes` |

#### Lifecycle

| Criterion | Tests |
|---|---|
| Keep-alive reuse | tokio-dst `dst_keep_alive_two_requests`, `dst_keep_alive_five_requests`; tokio-e2e `h1_adversarial::tcp_keep_alive_reuses_connection_for_sequential_requests`; monoio `keep_alive_multiple_requests` |
| Pipelined or buffered next request | tokio-dst `dst_pipelined_requests_correct_count` (proptest); codec `payload_length_with_trailing_data`, `payload_chunked_preserves_pipelined_bytes_after_terminator`; meguri `resume_after_keep_alive_reuses_buffered_pipelined_request`, `chunked_body_preserves_pipelined_data`; shared lifecycle `keep_alive_reset_reports_buffered_request_bytes`; tokio-e2e `h1_adversarial::tcp_pipelined_requests_after_bodies_are_parsed_from_buffer` |
| Body too large: early response | tokio-dst `dst_oversized_content_length`; tokio-e2e `h1_adversarial::tcp_oversized_content_length_gets_413_before_body_is_sent`, `h1_adversarial::tcp_early_response_without_reading_body_closes_connection`; meguri `content_length_too_large`; shared lifecycle `early_response_moves_from_body_to_write`; monoio `body_limit_content_length_rejection` |
| Slow header timeout | tokio-dst `dst_slowloris_header_timeout`; tokio-e2e `h1_adversarial::tcp_slow_header_timeout_closes_connection`; meguri `read_timed_out_uses_header_timeout_for_headers_state`; monoio `header_read_timeout` |
| Slow body timeout | tokio-dst `dst_slowloris_body_timeout`; tokio-e2e `tcp_body_read_timeout_returns_400` (Content-Length), `h1_adversarial::tcp_slow_chunked_body_timeout_gets_408`; meguri `read_timed_out_uses_body_timeout_for_body_phase`; monoio `body_read_timeout` |
| Shutdown during active request | tokio-e2e `tcp_graceful_drain_completes_inflight_request`, `tcp_graceful_shutdown_waits_for_drain_before_returning`, `h1_adversarial::tcp_shutdown_during_keep_alive_request_completes_then_closes`, `h1_adversarial::tcp_shutdown_closes_idle_keep_alive_connection_promptly`, `h1_adversarial::tcp_shutdown_lets_a_partially_received_request_finish`; multi `shutdown_drains_in_flight_requests_then_stops_accepting`; shared lifecycle `shutdown_scripts_can_drain_to_free`; monoio `graceful_shutdown_drains_connections` |

#### Behaviour these tests pin

- The server rejects framing errors in the request head or chunked body with
  one `400` (or `413` for the body limit) and `connection: close`. It does
  not dispatch the request and does not parse the bytes that follow.
- A header read timeout closes the connection without a response. A body
  read timeout sends `408`.
- EOF in the middle of a request body closes the connection without a
  response.
- If a response body does not match its declared `Content-Length`, the
  server closes the connection without writing that response. If a chunked
  stream fails after its first bytes are sent, the server closes the
  connection without the `0\r\n\r\n` terminator. The client then sees a
  truncated body.
- The server always replaces application-set `Transfer-Encoding` and
  `Connection` headers with its own framing.
- Chunked trailer fields are rejected (`400`). They are not ignored.

#### Known gaps (ignored tests)

None: the #98 and #100 tests, previously ignored, now run.

---

## First-Class Commands

- `cargo test -p harrow-core`
- `cargo test -p harrow-middleware --features rate-limit,compression`
- `cargo check --manifest-path harrow-core/fuzz/Cargo.toml`
- `cargo check --manifest-path harrow-middleware/fuzz/Cargo.toml`
- `cargo check --manifest-path harrow-codec-h1/fuzz/Cargo.toml`
- `mise run fuzz:check`
- `mise run fuzz:core:path`
- `mise run fuzz:core:query`
- `mise run fuzz:middleware:accept-encoding`
- `mise run fuzz:codec:parse-request`
- `mise run fuzz:codec:chunked-decode`
- `mise run fuzz:codec:response-head`
- `mise run fuzz:codec:payload-decoder`
- `mise run fuzz:codec:roundtrip`

---

### harrow-o11y

Configuration struct with no logic. Nothing to verify.

---

### harrow (umbrella)

Re-exports and the `AppO11yExt` trait. The only interesting behavior is that
the `TelemetryGuard` stays alive for the application's lifetime, which is
ensured by storing it in `TypeMap` state. Verified by existing integration
tests.

---

## Fuzz Campaigns

Every HTTP/1 codec fuzz target runs for at least **15 minutes** from the shared
corpus (`harrow-codec-h1/fuzz/corpus/<target>`) before a release, and each run
is recorded here. Any crash is fixed or filed as an issue before the release.
CI builds every fuzz target on each pull request (`Fuzz targets build` job,
same as `mise run fuzz:check`); it does not run them.

To run one target: `mise run fuzz:codec:<name>` (add
`-- -max_total_time=900` for a 15-minute run).

### 2026-10-19: HTTP/1 codec, pre-1.0 (#87)

- Host: macOS, Apple Silicon (aarch64); `cargo-fuzz` 0.13.1; rustc
  1.97.0-nightly (2026-04-12); libFuzzer; 900 s per target, existing corpora.
- Code fuzzed: `main` at `7bea8c5` with the then-open codec PRs #95 (`Expect`)
  and #96 (strict chunk-size) merged in; `fuzz_roundtrip` additionally with the
  harness fixes from this PR.

| Target | Executions | Coverage (edges) | Corpus | Crashes |
| --- | ---: | ---: | ---: | ---: |
| `fuzz_parse_request` | 61,640,389 | 2,049 | 4,066 | 0 |
| `fuzz_response_head` | 40,641,042 | 1,698 | 4,278 | 0 |
| `fuzz_chunked_decode` | 80,060,439 | 183 | 212 | 0 |
| `fuzz_payload_decoder` | 63,054,136 | 185 | 212 | 0 |
| `fuzz_roundtrip` | 81,537,223 | 521 | 88 | 0 |

Findings:

- `fuzz_roundtrip` had never fuzzed anything: its harness panicked on the first
  (empty) input, slicing `data[2..]` without a length check, and treated a
  zero-length `Content-Length` body (which has no payload decoder) as a crash.
  Fixed; the 15-minute run above is with the fixed harness.
- Fixing it exposed a codec trap: `encode_chunk_into(b"")` wrote `0\r\n\r\n`,
  the end-of-body marker. The servers already skip empty frames; the codec now
  writes nothing for an empty chunk.
- The same zero-length-body mistake made the proptest
  `request_body_roundtrip_with_content_length` fail in roughly one run in
  eight. Fixed.
- Two older crash inputs in `fuzz/artifacts/fuzz_chunked_decode/` (very large
  chunk sizes) replay cleanly: fixed earlier by checked size arithmetic.

## Implementation Priority

Ordered by expected bug-finding value per hour of effort:

| Priority | Target | Technique | Effort | Why |
|----------|--------|-----------|--------|-----|
| 1 | `path.rs` match_path/matches agreement | proptest | Low | Two implementations of the same logic — disagreement is a real bug class |
| 2 | `rate_limit.rs` GCRA burst/rate properties | proptest | Low | Arithmetic-heavy, easy to get off-by-one |
| 3 | `request.rs` query parsing | cargo-fuzz | Low | Untrusted input, percent-decoding, edge cases |
| 4 | `dispatch.rs` middleware ordering | proptest | Medium | Integration-level, needs App scaffolding |
| 5 | `compression.rs` round-trip | proptest | Low | Straightforward but catches encoding bugs |
| 6 | `rate_limit.rs` gcra_check | Kani | Medium | Bounded model checking for the CAS loop |
| 7 | `path.rs` adversarial patterns | cargo-fuzz | Low | Defense against malformed route definitions |
| 8 | `route.rs` 404/405 correctness | proptest | Medium | Needs route table construction in test |

---

## What We Explicitly Skip

- **DST / shadow-state oracles**: Harrow processes independent HTTP requests
  with no cross-request state (except the rate limiter, which is covered by
  proptest). There is no command sequence whose interleaving could produce a
  consistency violation. A shadow oracle for "given this request, what should
  the response be" is just the handler itself.

- **TLA+ / Quint for the whole framework**: No. The middleware chain is a
  linear pipeline, not a protocol worth modeling end-to-end. The exception is
  the backend connection/control lifecycle, which is finite enough to justify a
  focused TLA+ state-machine model. A Quint mirror would currently duplicate
  that maintenance burden without adding coverage.

- **Maelstrom / Jepsen**: Not a distributed system.

- **Kani for the middleware chain**: The chain is recursive with dynamic
  dispatch (`dyn Middleware`). Kani cannot reason about trait objects or async
  in a useful way here. Proptest covers this better.

If harrow later grows more protocol-shaped state (sessions, distributed rate
limiting, request queuing), revisit DST and broader TLA+/Quint modeling at
that point.
