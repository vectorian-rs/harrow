# Rate Limiting Middleware Design Document

**Status:** Draft
**Date:** 2026-03-19
**Crate:** `harrow-middleware` (feature-gated as `rate-limit`)

---

## Table of Contents

1. [Overview](#1-overview)
2. [Rate Limiting Algorithms](#2-rate-limiting-algorithms)
3. [Storage Backends](#3-storage-backends)
4. [Rate Limit Key Extraction](#4-rate-limit-key-extraction)
5. [Existing Rust Implementations](#5-existing-rust-implementations)
6. [Response Headers](#6-response-headers)
7. [HTTP 429 Response Handling](#7-http-429-response-handling)
8. [Middleware vs Per-Route Rate Limiting](#8-middleware-vs-per-route-rate-limiting)
9. [Distributed Rate Limiting Challenges](#9-distributed-rate-limiting-challenges)
10. [Best Practices](#10-best-practices)
11. [Security](#11-security)
12. [Proposed Harrow API](#12-proposed-harrow-api)

---

## 1. Overview

Rate limiting controls how many requests a client can make within a given time
period. It protects APIs from abuse, ensures fair resource allocation across
consumers, and prevents a single bad actor from overwhelming infrastructure.

This document surveys algorithms, storage backends, key extraction strategies,
and security considerations, then proposes a concrete API for harrow's
middleware system. Harrow does not use Tower, so all designs target harrow's
native `Middleware` trait:

```rust
pub trait Middleware: Send + Sync {
    fn call(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>>;
}
```

The middleware short-circuits with a `429 Too Many Requests` response when the
limit is exceeded, and attaches standard rate limit headers to every response.

---

## 2. Rate Limiting Algorithms

### 2.1 Fixed Window Counter

**How it works:** Divide time into discrete, non-overlapping windows (e.g.,
60-second blocks). Each window maintains a counter that increments on every
request. When the counter exceeds the limit, subsequent requests are rejected
until the window resets.

**State:** One integer counter + window start timestamp per key.

**Pseudocode:**

```
window_id = floor(now / window_size)
count = store.increment(key, window_id)
if count > limit:
    reject
```

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | Minimal -- one counter per key per window |
| Accuracy | Approximate -- boundary burst problem |
| Complexity | Very low |
| Burst behavior | Allows 2x burst at window boundaries |

**Boundary burst problem:** A client can send `limit` requests at the end of
window N and another `limit` requests at the start of window N+1, effectively
getting `2 * limit` requests within a single `window_size` duration.

**When to use:** Login throttling, simple API limits, internal services where
approximate enforcement is acceptable.


### 2.2 Sliding Window Log

**How it works:** Record the exact timestamp of every request per key. On each
new attempt, remove entries older than the window, count remaining entries, and
allow if under the limit.

**State:** A sorted set of timestamps per key. Memory is O(n) where n is the
request limit.

**Pseudocode:**

```
remove entries where timestamp < (now - window_size)
count = entries.len()
if count < limit:
    entries.add(now, unique_id)
    allow
else:
    reject with retry_after = oldest_entry + window_size - now
```

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | O(n) per key -- high for large limits |
| Accuracy | Exact -- true sliding window |
| Complexity | Moderate |
| Burst behavior | No boundary bursts |

**When to use:** High-value APIs (payment processing, authentication) where
boundary accuracy matters and request volume per key is low-to-moderate.


### 2.3 Sliding Window Counter

**How it works:** A hybrid of fixed window and sliding window. Maintains two
adjacent fixed-window counters (current and previous) and computes a weighted
average based on the position within the current window.

**State:** Two counters per key.

**Formula:**

```
elapsed = (now % window_size) / window_size
estimate = prev_count * (1 - elapsed) + current_count
```

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | Low -- two counters per key |
| Accuracy | Near-exact (weighted approximation) |
| Complexity | Low |
| Burst behavior | Smoothed boundaries |

**When to use:** Best general-purpose default. Low memory, near-exact accuracy,
no boundary bursts. Recommended for most API rate limiting.


### 2.4 Token Bucket

**How it works:** Each key has a "bucket" that holds tokens, with a maximum
capacity (burst size). Tokens refill at a steady rate. Each request consumes
one token. When the bucket is empty, requests are rejected.

**State:** Token count + last refill timestamp per key.

**Pseudocode:**

```
elapsed = now - last_refill
tokens = min(max_tokens, tokens + elapsed * refill_rate)
if tokens >= 1:
    tokens -= 1
    last_refill = now
    allow
else:
    reject with retry_after = ceil(1 / refill_rate)
```

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | Low -- two values per key |
| Accuracy | Exact |
| Complexity | Low-moderate |
| Burst behavior | Allows controlled bursts up to capacity |

**When to use:** APIs with naturally bursty traffic (mobile apps batching
requests on launch, webhook bursts). The burst capacity is an explicit,
configurable parameter.


### 2.5 Leaky Bucket

**How it works:** Conceptually a bucket with a hole -- requests fill it, and it
drains at a fixed rate. Two modes exist:

- **Policing (meter):** Tracks a virtual "fill level" that drains over time.
  If adding a request would exceed capacity, reject immediately.
- **Shaping (queue):** Requests enter a FIFO queue drained at a fixed rate.
  If the queue is full, reject. Accepted requests experience a delay.

**State (policing):** Fill level + last drain timestamp per key.

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | Low -- two values per key |
| Accuracy | Exact |
| Complexity | Low-moderate |
| Burst behavior | No bursts -- strict constant rate |

**When to use:** Protecting downstream services that cannot handle bursty
traffic, even when long-term averages are fine. Shaping mode is appropriate
for background workers or proxy/gateway flows.


### 2.6 GCRA (Generic Cell Rate Algorithm)

**How it works:** GCRA is functionally equivalent to a leaky bucket but
eliminates the need for a background "drip" process. It tracks a single
timestamp called the **Theoretical Arrival Time (TAT)** and an **emission
interval (T)** derived from the desired rate.

**State:** A single 64-bit timestamp (TAT) per key. This is the most
memory-efficient algorithm.

**Decision logic:**

```
T = 1 / rate                    // emission interval
tau = burst_capacity * T        // burst tolerance window

if TAT == 0:                    // first request
    TAT = now + T
    allow
else:
    allow_at = TAT - tau
    if now >= allow_at:
        TAT = max(now, TAT) + T
        allow
    else:
        reject with retry_after = allow_at - now
```

The key insight: by measuring quota in units of time, bucket "leakage" becomes
trivial -- it is just time passing. No background process is required.

**Trade-offs:**

| Aspect | Assessment |
|---|---|
| Memory | Minimal -- single 64-bit timestamp per key |
| Accuracy | Exact |
| Complexity | Moderate (conceptually elegant but less intuitive) |
| Burst behavior | Smooth rate with configurable burst tolerance |
| Thread safety | CAS-friendly -- 64-bit atomic compare-and-swap |

**Implementation advantage:** The 64-bit state can be updated with a single
`AtomicU64::compare_exchange`, making it ~10x faster under contention than
mutex-based approaches (benchmarked by the `governor` crate). No lock is
needed.

**When to use:** High-performance, high-concurrency scenarios. The algorithm
used by the `governor` crate (the de facto Rust rate limiting library).

**This is the recommended algorithm for harrow.**


### Algorithm Comparison Summary

| Algorithm | State Size | Accuracy | Boundary Bursts | Background Process | CAS-Safe |
|---|---|---|---|---|---|
| Fixed Window | 1 counter | Approximate | Yes (2x) | No | Yes |
| Sliding Window Log | O(n) set | Exact | No | No | No (set mutation) |
| Sliding Window Counter | 2 counters | Near-exact | Smoothed | No | Yes |
| Token Bucket | 2 values | Exact | Controlled | No | Possible |
| Leaky Bucket | 2 values | Exact | No | No (policing) / Yes (shaping) | Possible |
| GCRA | 1 timestamp | Exact | Smooth | No | Yes (single CAS) |

---

## 3. Storage Backends

### 3.1 In-Memory: `AtomicU64` (GCRA direct limiter)

For a single global rate limit (not keyed), GCRA's 64-bit TAT can live in
a bare `AtomicU64`. Zero allocation, zero locking, zero dependencies.

```rust
use std::sync::atomic::{AtomicU64, Ordering};

struct GcraDirect {
    tat: AtomicU64,   // TAT in nanoseconds since epoch
    t_ns: u64,        // emission interval in nanoseconds
    tau_ns: u64,      // burst tolerance in nanoseconds
}
```

**Best for:** Single-rate global limits (e.g., "1000 req/s total").


### 3.2 In-Memory: `DashMap<K, AtomicU64>` (GCRA keyed limiter)

[DashMap](https://crates.io/crates/dashmap) is a sharded concurrent HashMap
that avoids a single global lock. Each shard has its own `RwLock`. For keyed
rate limiting (per-IP, per-user), it is the natural in-memory choice.

```rust
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;

struct GcraKeyed {
    states: DashMap<String, AtomicU64>,
    t_ns: u64,
    tau_ns: u64,
}
```

**Eviction:** DashMap does not have built-in TTL eviction. A background task
must periodically sweep stale entries (e.g., entries where
`now - TAT > tau + T`), or use a time-based eviction wrapper like `moka`.

**Best for:** Single-process keyed rate limiting. No external dependencies
beyond `dashmap`.

**Alternative: `moka`:** The [moka](https://crates.io/crates/moka) crate
provides a concurrent cache with TTL-based eviction, eliminating the need for
a manual sweeper. Trade-off: slightly higher per-entry overhead due to eviction
metadata.

**Alternative: `Arc<Mutex<HashMap<K, V>>>`:** Acceptable for low-concurrency
use cases but becomes a bottleneck under contention. Not recommended for
production rate limiting.


### 3.3 Redis

Redis is the standard choice for distributed, multi-instance rate limiting.
All five algorithms in Section 2 can be implemented in Redis using Lua scripts
executed via `EVAL` for atomicity.

**Why Lua scripts (not MULTI/EXEC):** Rate limiting requires
read-decide-write semantics. `MULTI/EXEC` cannot branch on intermediate
results. `WATCH/MULTI/EXEC` triggers retries under contention -- exactly when
rate limiting is most critical. Lua scripts execute atomically in a single
round trip with no retry loop.

**Recommended Redis data structures by algorithm:**

| Algorithm | Redis Type | Keys per Client |
|---|---|---|
| Fixed Window | STRING (INCR + EXPIRE) | 1 |
| Sliding Window Log | SORTED SET | 1 (O(n) members) |
| Sliding Window Counter | STRING x2 (hash-tagged) | 2 |
| Token Bucket | HASH (2 fields) | 1 |
| Leaky Bucket | HASH (1-2 fields) | 1 |
| GCRA | STRING (64-bit timestamp) | 1 |

**Redis Cluster consideration:** Multi-key scripts require all keys to hash
to the same slot. Use hash tags (e.g., `{user:123}:current`,
`{user:123}:previous`) to co-locate keys.

**Clock synchronization:** Use Redis `TIME` command or pass a consistent
server-side timestamp to Lua scripts to avoid clock drift between application
instances.


### 3.4 Backend Trait

To support pluggable backends, define a trait:

```rust
/// Result of a rate limit check.
pub struct RateLimitOutcome {
    /// Whether the request is allowed.
    pub allowed: bool,
    /// Remaining quota in the current window/bucket.
    pub remaining: u64,
    /// Total quota limit.
    pub limit: u64,
    /// Seconds until quota resets (for response headers).
    pub reset_after_secs: u64,
    /// Seconds until the client should retry (None if allowed).
    pub retry_after_secs: Option<u64>,
}

/// A rate limiting backend. Implementations must be Send + Sync for
/// use inside harrow's async middleware.
pub trait RateLimitBackend: Send + Sync {
    fn check(&self, key: &str) -> impl Future<Output = RateLimitOutcome> + Send;
}
```

This trait abstracts over in-memory and Redis backends, and enables testing
with a mock implementation.

---

## 4. Rate Limit Key Extraction

The rate limit key determines what entity is throttled. Different strategies
suit different use cases.

### 4.1 Key Extraction Trait

```rust
/// Extracts a rate limit key from the request.
pub trait KeyExtractor: Send + Sync {
    /// Returns the key string, or None to skip rate limiting for this request.
    fn extract(&self, req: &Request) -> Option<String>;
}
```

Returning `None` allows skipping rate limiting for certain requests (e.g.,
health checks, internal traffic).


### 4.2 Built-in Key Extractors

**Peer IP (default):**

Extracts the remote IP from the connection. This is the most common default
but requires the IP to be available on the request (set at the server layer).

```rust
pub struct PeerIpKeyExtractor;

impl KeyExtractor for PeerIpKeyExtractor {
    fn extract(&self, req: &Request) -> Option<String> {
        // Extract from connection info or a trusted header
        req.peer_ip().map(|ip| ip.to_string())
    }
}
```

**Header-based (API key, bearer token):**

```rust
pub struct HeaderKeyExtractor {
    header_name: String,
}

impl KeyExtractor for HeaderKeyExtractor {
    fn extract(&self, req: &Request) -> Option<String> {
        req.header(&self.header_name).map(|v| v.to_string())
    }
}
```

**Smart IP (proxy-aware):**

Checks a prioritized list of headers: `x-forwarded-for` (rightmost trusted
entry), `x-real-ip`, `forwarded`, then falls back to peer IP. See Section 11
for security considerations.

```rust
pub struct SmartIpKeyExtractor {
    trusted_proxy_count: usize,
}
```

**Route-scoped:**

Combines the route pattern with another key to enable per-endpoint limits:

```rust
pub struct RouteKeyExtractor<Inner: KeyExtractor> {
    inner: Inner,
}

impl<Inner: KeyExtractor> KeyExtractor for RouteKeyExtractor<Inner> {
    fn extract(&self, req: &Request) -> Option<String> {
        let route = req.route_pattern().unwrap_or("unknown");
        let inner_key = self.inner.extract(req)?;
        Some(format!("{route}:{inner_key}"))
    }
}
```

**Custom closure:**

For maximum flexibility, accept a closure:

```rust
impl<F> KeyExtractor for F
where
    F: Fn(&Request) -> Option<String> + Send + Sync,
{
    fn extract(&self, req: &Request) -> Option<String> {
        (self)(req)
    }
}
```

### 4.3 Composite Keys

For multi-dimensional rate limiting (e.g., "100 req/min per IP AND
1000 req/hour per API key"), stack multiple middleware instances with
different extractors and limits.

---

## 5. Existing Rust Implementations

### 5.1 `governor` (crate)

- **Algorithm:** GCRA exclusively
- **State:** 64-bit atomic compare-and-swap, ~10x faster than mutex-based
  approaches under multi-threaded contention
- **Modes:** Direct (single flow) and keyed (per-key) rate limiters
- **Configuration:** `Quota::per_second(n)`, `Quota::per_minute(n)`, with
  configurable burst size via `NonZeroU32`
- **Clock:** Pluggable clock trait; supports `no_std` with fake clocks for
  testing
- **No framework coupling:** Pure rate limiting logic, no HTTP concepts
- **Recommendation:** Use `governor` as the underlying algorithm engine if we
  want battle-tested GCRA without reimplementing it. Harrow's middleware would
  wrap `governor::RateLimiter` with key extraction and response header logic.

### 5.2 `tower-governor`

- **Framework:** Tower middleware wrapping `governor`
- **Key extractors:** `PeerIpKeyExtractor`, `SmartIpKeyExtractor`,
  `GlobalKeyExtractor`, custom via `KeyExtractor` trait
- **Headers:** Optional `x-ratelimit-limit`, `x-ratelimit-remaining`,
  `x-ratelimit-after`, `retry-after`
- **429 response:** `GovernorError` type with automatic conversion to HTTP 429
- **Relevance to harrow:** Tower-coupled. We cannot use the middleware
  directly, but the API design (configuration builder, key extractor trait,
  error handler callback) is a good reference.

### 5.3 `actix-extensible-rate-limit`

- **Framework:** Actix-web middleware
- **Algorithm:** Fixed window (in-memory via DashMap, Redis)
- **Key features:**
  - Pluggable backends via trait
  - Dynamic rate limits derived from request context (per-user tiers)
  - Response rollback -- rate limit counts can reset based on response status
    (e.g., don't count 5xx errors against the caller)
  - Standard `x-ratelimit-*` headers
- **Relevance to harrow:** The response rollback pattern and dynamic limits
  per request context are worth considering.

### 5.4 `actix-limitation`

- **Framework:** Actix-web
- **Algorithm:** Fixed window counter, Redis-backed
- **Key extraction:** Header-based (configurable header name) or cookie-based
- **Relevance:** Simple reference for Redis fixed-window implementation.

### 5.5 Crate Comparison

| Crate | Algorithm | Backend | Framework | Headers | Custom Keys |
|---|---|---|---|---|---|
| `governor` | GCRA | In-memory (atomic) | None | No | Keyed generic |
| `tower-governor` | GCRA | In-memory (atomic) | Tower | Yes | Trait-based |
| `actix-extensible-rate-limit` | Fixed Window | DashMap, Redis | Actix | Yes | Trait-based |
| `actix-limitation` | Fixed Window | Redis | Actix | Yes | Header/cookie |
| `actix-ratelimit` | Token Bucket | Memory, Redis | Actix | Yes | IP-based |

---

## 6. Response Headers

### 6.1 De Facto Standard (widely deployed)

Most APIs use these headers (not yet an official RFC, but universally
recognized):

```
X-RateLimit-Limit: 100        # total quota in the window
X-RateLimit-Remaining: 42     # remaining requests in the window
X-RateLimit-Reset: 1710849600 # Unix timestamp when the window resets
Retry-After: 30               # seconds until the client should retry (on 429)
```

### 6.2 IETF Draft Standard (draft-ietf-httpapi-ratelimit-headers-10)

The IETF is standardizing rate limit headers using Structured Fields
(RFC 9651). The draft defines two fields:

**`RateLimit-Policy`** -- advertises quota policies (relatively static):

```
RateLimit-Policy: "default";q=100;w=60, "daily";q=1000;w=86400
```

Parameters:
- `q` -- quota allocation (required)
- `w` -- window size in seconds
- `qu` -- quota unit ("requests", "content-bytes", "concurrent-requests")
- `pk` -- partition key

**`RateLimit`** -- communicates current status (changes per request):

```
RateLimit: "default";r=42;t=30
```

Parameters:
- `r` -- remaining quota units (required)
- `t` -- seconds until quota restoration

**Precedence rule:** If both `RateLimit` and `Retry-After` are present,
`Retry-After` takes precedence.

### 6.3 Recommendation for Harrow

Support both the de facto `X-RateLimit-*` headers (for backward compatibility)
and the IETF draft headers. Let users choose via configuration:

```rust
pub enum RateLimitHeaderStyle {
    /// X-RateLimit-Limit, X-RateLimit-Remaining, X-RateLimit-Reset
    Legacy,
    /// RateLimit-Policy, RateLimit (IETF draft)
    Ietf,
    /// Emit both legacy and IETF headers
    Both,
    /// No rate limit headers (still sends Retry-After on 429)
    None,
}
```

**Always** send `Retry-After` on 429 responses regardless of header style.
Use seconds (not HTTP-date) for `Retry-After` to avoid clock synchronization
issues.

**Security note:** Do not expose operational capacity information (e.g., total
server capacity) in headers sent to untrusted clients. The `limit` value
should reflect the per-client quota, not infrastructure capacity.

---

## 7. HTTP 429 Response Handling

### 7.1 Default 429 Response

```rust
fn rate_limited_response(outcome: &RateLimitOutcome) -> Response {
    let retry_after = outcome.retry_after_secs.unwrap_or(1);
    Response::new(StatusCode::TOO_MANY_REQUESTS, "too many requests")
        .header("retry-after", &retry_after.to_string())
}
```

### 7.2 Custom 429 Handler

Users should be able to customize the 429 response body (e.g., to return JSON
error responses matching their API's error format):

```rust
pub type RateLimitErrorHandler = Arc<dyn Fn(&RateLimitOutcome) -> Response + Send + Sync>;
```

Usage:

```rust
let mw = rate_limit_middleware(config)
    .error_handler(|outcome| {
        Response::new(StatusCode::TOO_MANY_REQUESTS, r#"{"error":"rate_limited"}"#)
            .header("content-type", "application/json")
            .header("retry-after", &outcome.retry_after_secs.unwrap_or(1).to_string())
    });
```

### 7.3 Response Rollback

Optionally, do not count requests that result in server errors (5xx) against
the caller's quota. This prevents infrastructure failures from unfairly
penalizing clients. Implementation: check the response status after calling
`next.run(req)` and decrement the counter if the status is 5xx.

This is a post-response concern and adds complexity. It should be opt-in and
only supported by backends that implement a `rollback` method.

---

## 8. Middleware vs Per-Route Rate Limiting

Harrow supports both global middleware and route-group-scoped middleware.
Rate limiting can be applied at either level.

### 8.1 Global Middleware

Applied to every request via `App::middleware()`:

```rust
let app = App::new()
    .middleware(rate_limit_middleware(
        RateLimitConfig::new()
            .requests_per_second(100)
            .burst_size(50)
    ))
    .get("/health", health)
    .get("/api/users", list_users);
```

**Use case:** Baseline protection for the entire service.

### 8.2 Per-Route Group Middleware

Applied to specific route groups via `Group::middleware()`:

```rust
let app = App::new()
    .get("/health", health)  // no rate limit
    .group("/api", |g| {
        g.middleware(rate_limit_middleware(
            RateLimitConfig::new()
                .requests_per_minute(1000)
                .burst_size(100)
                .key_extractor(HeaderKeyExtractor::new("x-api-key"))
        ))
        .get("/users", list_users)
        .post("/users", create_user)
    })
    .group("/auth", |g| {
        g.middleware(rate_limit_middleware(
            RateLimitConfig::new()
                .requests_per_minute(10)  // strict limit on auth
                .burst_size(5)
                .key_extractor(PeerIpKeyExtractor)
        ))
        .post("/login", login)
        .post("/register", register)
    });
```

**Use case:** Different limits for different API tiers or endpoint sensitivity.

### 8.3 Layered Rate Limiting

Stack multiple rate limiters for multi-dimensional protection:

```rust
let app = App::new()
    // Global: 10,000 req/min per IP (DDoS protection)
    .middleware(rate_limit_middleware(
        RateLimitConfig::new()
            .requests_per_minute(10_000)
            .burst_size(1000)
            .key_extractor(PeerIpKeyExtractor)
    ))
    .group("/api", |g| {
        // Per-API-key: 100 req/min
        g.middleware(rate_limit_middleware(
            RateLimitConfig::new()
                .requests_per_minute(100)
                .burst_size(20)
                .key_extractor(HeaderKeyExtractor::new("x-api-key"))
        ))
        .get("/data", get_data)
    });
```

Global middleware runs first (outer), then route-group middleware (inner).
If the global limiter rejects, the per-route limiter is never consulted.

---

## 9. Distributed Rate Limiting Challenges

### 9.1 Clock Synchronization

Rate limiting algorithms that depend on timestamps (all of them) are
vulnerable to clock drift between application instances. Mitigations:

- **Use the store's clock:** For Redis backends, use the Redis `TIME` command
  inside Lua scripts rather than application-side timestamps.
- **NTP / chrony:** Ensure all instances synchronize clocks via NTP. Clock
  drift of a few milliseconds is acceptable for most rate limiting windows.
- **Relative time:** GCRA's TAT-based approach is more resilient to small
  clock differences because it operates on durations, not absolute timestamps.

### 9.2 Race Conditions

Without atomic operations, concurrent requests across multiple instances can
all read the same counter value, all decide to allow, and all write back --
exceeding the limit. This is a classic TOCTOU (time-of-check-time-of-use)
problem.

**Redis Lua scripts** solve this: the entire read-decide-write sequence
executes atomically on the Redis server. No other command interleaves.

**In-memory AtomicU64** with CAS (compare-and-swap) solves this for
single-process deployments: the `compare_exchange` loop retries if another
thread updated the value between read and write.

### 9.3 Redis Availability

If Redis becomes unavailable, the rate limiter must decide:

- **Fail-open (allow all):** Preserves availability at the cost of losing rate
  limiting protection. Appropriate for most non-critical APIs.
- **Fail-closed (deny all):** Preserves protection at the cost of availability.
  Appropriate for sensitive endpoints (auth, payments).
- **Fall back to local limiter:** Use an in-memory limiter as a degraded-mode
  fallback. Per-instance limits are less accurate but better than nothing.

The choice should be configurable:

```rust
pub enum FailStrategy {
    Open,
    Closed,
    LocalFallback,
}
```

### 9.4 Consistency vs Availability

In a distributed system, strict rate limiting consistency (never exceeding the
exact limit) requires synchronous coordination (Redis Lua, distributed locks).
This adds latency to every request.

For most use cases, **approximate consistency** is acceptable: if the limit is
100 req/min, allowing 105 occasionally is fine. The sliding window counter
algorithm inherently provides this trade-off -- its weighted approximation
slightly over-counts or under-counts in rare edge cases.

### 9.5 Redis Cluster and Multi-Key Scripts

Redis Cluster distributes keys across slots. Lua scripts that access multiple
keys require all keys to be on the same slot. Use hash tags:

```
{user:123}:current_window
{user:123}:previous_window
```

Only the substring inside `{...}` determines the slot, so both keys land on
the same node.

Single-key algorithms (GCRA, token bucket, leaky bucket, fixed window) are
inherently cluster-safe.

### 9.6 Network Latency

Redis round-trip adds 0.1-1ms per request. For latency-sensitive services:

- Use in-memory rate limiting for per-instance protection.
- Use Redis for cross-instance aggregate limits.
- Pipeline Redis calls with other per-request operations if possible.

---

## 10. Best Practices

### 10.1 Graceful Degradation

- Configure fail-open vs fail-closed per endpoint sensitivity.
- Monitor rate limiter backend health with the o11y middleware.
- Set timeouts on Redis calls (e.g., 5ms) -- better to skip rate limiting
  than to add 100ms of latency when Redis is slow.
- Log rate limit rejections with the request ID for debugging.

### 10.2 Burst Handling

- Always configure an explicit burst size. A rate of "100 req/min" without
  burst tolerance forces perfectly smooth traffic, which penalizes legitimate
  batch operations.
- Use GCRA or token bucket for natural burst support with a controlled
  ceiling.
- Set burst size relative to the rate: a burst of 10-50% of the per-second
  rate is a reasonable starting point.

### 10.3 Rate Limit Tiers

Support multiple tiers for different client classes:

```rust
// Example: dynamic limits based on app state
let mw = rate_limit_middleware(
    RateLimitConfig::new()
        .key_extractor(|req: &Request| {
            req.header("x-api-key").map(|k| k.to_string())
        })
        .quota_resolver(|req: &Request| {
            // Look up the API key's tier from app state
            match req.try_state::<ApiKeyTiers>() {
                Some(tiers) => {
                    let key = req.header("x-api-key").unwrap_or("anonymous");
                    tiers.quota_for(key)
                }
                None => Quota::per_minute(100), // default
            }
        })
);
```

This requires a `QuotaResolver` trait or callback that determines the quota
dynamically per request, rather than a single static quota at middleware
construction time.

### 10.4 Exemptions

Exempt certain requests from rate limiting:

- Health check endpoints (`/health`, `/ready`)
- Internal service-to-service traffic (identified by header or IP range)
- Monitoring and metrics endpoints

Implement via the `KeyExtractor` returning `None` for exempt requests.

### 10.5 Observability

Emit metrics for rate limiting:

- `rate_limit.allowed` -- counter of allowed requests
- `rate_limit.rejected` -- counter of rejected requests (429s)
- `rate_limit.remaining` -- gauge of remaining quota
- `rate_limit.backend_latency` -- histogram of backend check duration

Log structured events on rejection:

```json
{
  "event": "rate_limited",
  "key": "192.168.1.1",
  "limit": 100,
  "remaining": 0,
  "retry_after_secs": 12,
  "request_id": "a1b2c3d4"
}
```

### 10.6 Thundering Herd Prevention

When many clients are rate-limited simultaneously and their `retry_after`
values are identical, they will all retry at exactly the same moment,
potentially overwhelming the server. Add jitter to the reset value:

```rust
let jitter = rand::random::<f64>() * 0.1 * base_retry_after;
let retry_after = base_retry_after + jitter;
```

The IETF draft explicitly recommends this: "servers should add some jitter to
the reset value."

---

## 11. Security

### 11.1 X-Forwarded-For Header Spoofing

**The vulnerability:** When rate limiting keys off client IP derived from
`X-Forwarded-For`, an attacker can vary the header value on each request,
creating a separate rate limit bucket per spoofed IP. This completely bypasses
rate limiting.

This is one of the most common rate limit bypass techniques. Real-world CVEs
include Mastodon (GHSA-c2r5-cfqr-c553), Litestar (GHSA-hm36-ffrh-c77c), and
many others.

**Prevention:**

1. **Never trust `X-Forwarded-For` unconditionally.** The header is a
   comma-separated list of IPs. Clients can prepend arbitrary entries.

2. **Configure trusted proxy count.** If you know your infrastructure has
   exactly N reverse proxies between the internet and the application, extract
   the client IP from position `len - N` in the `X-Forwarded-For` list
   (counting from the right). Only proxies append to the right side; the
   client can only control entries to the left.

   ```
   X-Forwarded-For: <spoofed>, <spoofed>, <real-client>, <proxy-1>, <proxy-2>
                                            ^^^^^^^^^^^
                                            rightmost minus trusted_count
   ```

3. **Prefer the peer (socket) IP** when no trusted proxy is configured. This
   is unforgeable at the network layer.

4. **Validate proxy IPs.** Optionally maintain an allowlist of known proxy IP
   ranges and only trust `X-Forwarded-For` entries when the immediate
   connection comes from a known proxy.

**Harrow's `SmartIpKeyExtractor` must require a `trusted_proxy_count` parameter
and refuse to compile without it:**

```rust
// GOOD: explicit trust boundary
SmartIpKeyExtractor::new(trusted_proxy_count: 2)

// BAD: would trust client-supplied headers unconditionally
// SmartIpKeyExtractor::new()  // should not exist
```

### 11.2 Other Header Spoofing

Beyond `X-Forwarded-For`, attackers may attempt:

- **`X-Real-Ip`**: Same vulnerability. Only trust from known proxies.
- **`Forwarded`**: RFC 7239 standard header. Same trust boundary applies.
- **`X-Api-Key` rotation:** If rate limiting by API key, attackers may rotate
  between stolen or generated keys. Combine with per-IP limits as a backstop.
- **`Host` header manipulation:** Some implementations incorrectly include
  the `Host` header in the rate limit key. Attackers can vary it.

### 11.3 Rate Limit Key Enumeration

Do not include sensitive information in rate limit keys that could leak via
timing attacks. The time to process a rate-limited vs. allowed request should
not reveal whether a particular key exists in the system.

### 11.4 Resource Exhaustion via Key Cardinality

An attacker generating unique keys (e.g., unique `X-Forwarded-For` values)
can exhaust the rate limiter's memory by creating millions of entries.
Mitigations:

- **Maximum key cardinality:** Cap the number of tracked keys. When exceeded,
  either reject all new keys (fail-closed) or evict least-recently-used
  entries.
- **Key normalization:** Normalize keys to reduce cardinality (e.g., hash
  long strings, truncate to /24 subnets for IPs).
- **TTL-based eviction:** All entries should have a maximum TTL. DashMap does
  not provide this natively; consider `moka` or a background sweeper.
- **Rate limit the rate limiter:** Apply a coarse per-IP connection limit at
  the load balancer or reverse proxy layer before requests reach the
  application.

---

## 12. Proposed Harrow API

### 12.1 Feature Gate

```toml
# harrow-middleware/Cargo.toml
[features]
rate-limit = ["dep:dashmap"]
rate-limit-redis = ["rate-limit", "dep:redis"]
```

The `rate-limit` feature provides in-memory GCRA rate limiting with zero
external service dependencies. The `rate-limit-redis` feature adds the Redis
backend for distributed deployments.

### 12.2 Configuration

```rust
use std::num::NonZeroU32;

pub struct RateLimitConfig<K: KeyExtractor = PeerIpKeyExtractor> {
    /// Maximum sustained requests per second.
    pub rate: NonZeroU32,
    /// Maximum burst size (requests allowed instantaneously).
    pub burst: NonZeroU32,
    /// Key extractor.
    pub key_extractor: K,
    /// Response header style.
    pub header_style: RateLimitHeaderStyle,
    /// Custom error handler (None = default 429 response).
    pub error_handler: Option<RateLimitErrorHandler>,
    /// Behavior when the backend is unavailable.
    pub fail_strategy: FailStrategy,
}

impl RateLimitConfig {
    pub fn per_second(rate: u32) -> Self { /* ... */ }
    pub fn per_minute(rate: u32) -> Self { /* ... */ }
}
```

### 12.3 Middleware Construction

```rust
use harrow_middleware::rate_limit::{rate_limit_middleware, RateLimitConfig};

// Simple: 100 req/s per IP with burst of 50
let mw = rate_limit_middleware(
    RateLimitConfig::per_second(100).burst(50)
);

// Custom key: rate limit by API key
let mw = rate_limit_middleware(
    RateLimitConfig::per_minute(1000)
        .burst(100)
        .key_extractor(HeaderKeyExtractor::new("x-api-key"))
);

// With custom 429 response
let mw = rate_limit_middleware(
    RateLimitConfig::per_second(50)
        .burst(10)
        .error_handler(|outcome| {
            Response::new(StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"rate_limited\"}")
                .header("content-type", "application/json")
                .header("retry-after", &outcome.retry_after_secs.unwrap_or(1).to_string())
        })
);
```

### 12.4 Middleware Implementation Sketch

```rust
pub struct RateLimitMiddleware<K: KeyExtractor, B: RateLimitBackend> {
    config: Arc<RateLimitConfig<K>>,
    backend: Arc<B>,
}

impl<K: KeyExtractor, B: RateLimitBackend> Middleware for RateLimitMiddleware<K, B> {
    fn call(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let config = Arc::clone(&self.config);
        let backend = Arc::clone(&self.backend);

        Box::pin(async move {
            // 1. Extract key
            let key = match config.key_extractor.extract(&req) {
                Some(k) => k,
                None => return next.run(req).await, // skip rate limiting
            };

            // 2. Check rate limit
            let outcome = backend.check(&key).await;

            // 3. If denied, return 429
            if !outcome.allowed {
                return match &config.error_handler {
                    Some(handler) => handler(&outcome),
                    None => default_429_response(&outcome),
                };
            }

            // 4. Call next, attach rate limit headers to response
            let resp = next.run(req).await;
            attach_rate_limit_headers(resp, &outcome, config.header_style)
        })
    }
}

fn attach_rate_limit_headers(
    resp: Response,
    outcome: &RateLimitOutcome,
    style: RateLimitHeaderStyle,
) -> Response {
    match style {
        RateLimitHeaderStyle::Legacy => resp
            .header("x-ratelimit-limit", &outcome.limit.to_string())
            .header("x-ratelimit-remaining", &outcome.remaining.to_string())
            .header("x-ratelimit-reset", &outcome.reset_after_secs.to_string()),
        RateLimitHeaderStyle::None => resp,
        // ... other styles
    }
}
```

### 12.5 Full Example

```rust
use harrow::App;
use harrow_middleware::rate_limit::*;
use harrow_middleware::catch_panic::catch_panic_middleware;
use harrow_middleware::timeout::timeout_middleware;
use std::time::Duration;

async fn health(_req: Request) -> Response {
    Response::ok()
}

async fn list_users(req: Request) -> Response {
    Response::text("users")
}

async fn login(req: Request) -> Response {
    Response::text("login")
}

let app = App::new()
    // Outer middleware: catch panics, timeout
    .middleware(catch_panic_middleware)
    .middleware(timeout_middleware(Duration::from_secs(30)))
    // Global rate limit: 10k req/min per IP
    .middleware(rate_limit_middleware(
        RateLimitConfig::per_minute(10_000).burst(1000)
    ))
    // Unprotected health endpoint
    .get("/health", health)
    // API routes with per-key limits
    .group("/api", |g| {
        g.middleware(rate_limit_middleware(
            RateLimitConfig::per_minute(500)
                .burst(50)
                .key_extractor(HeaderKeyExtractor::new("x-api-key"))
                .header_style(RateLimitHeaderStyle::Both)
        ))
        .get("/users", list_users)
    })
    // Auth routes with strict IP-based limits
    .group("/auth", |g| {
        g.middleware(rate_limit_middleware(
            RateLimitConfig::per_minute(10)
                .burst(5)
                .key_extractor(SmartIpKeyExtractor::new(2))
        ))
        .post("/login", login)
    });
```

---

## References

- [governor crate](https://crates.io/crates/governor) -- GCRA rate limiting for Rust
- [tower-governor](https://github.com/benwis/tower-governor) -- Tower middleware wrapping governor
- [actix-extensible-rate-limit](https://github.com/jacob-pro/actix-extensible-rate-limit) -- Flexible Actix rate limiting
- [Rate Limiting, Cells, and GCRA](https://brandur.org/rate-limiting) -- GCRA algorithm deep dive
- [GCRA: leaky buckets without the buckets](https://dotat.at/@/2024-08-30-gcra.html) -- GCRA implementation details
- [Generic Cell Rate Algorithm (Wikipedia)](https://en.wikipedia.org/wiki/Generic_cell_rate_algorithm)
- [Build 5 Rate Limiters with Redis](https://redis.io/tutorials/howtos/ratelimiting/) -- Redis algorithm comparison
- [IETF draft-ietf-httpapi-ratelimit-headers-10](https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/) -- RateLimit header standardization
- [DashMap](https://crates.io/crates/dashmap) -- Concurrent HashMap for Rust
- [moka](https://crates.io/crates/moka) -- Concurrent cache with TTL eviction
- [X-Forwarded-For spoofing](https://www.stackhawk.com/blog/do-you-trust-your-x-forwarded-for-header/) -- Header trust security
- [Mastodon rate limit bypass CVE](https://github.com/mastodon/mastodon/security/advisories/GHSA-c2r5-cfqr-c553)
