use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

// ---------------------------------------------------------------------------
// Monotonic clock
// ---------------------------------------------------------------------------

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ns() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

/// Convert nanoseconds to seconds, rounding up.
fn ns_to_secs_ceil(ns: u64) -> u64 {
    ns.div_ceil(1_000_000_000)
}

// ---------------------------------------------------------------------------
// RateLimitOutcome
// ---------------------------------------------------------------------------

/// Result of a rate-limit check.
pub struct RateLimitOutcome {
    pub allowed: bool,
    pub limit: u64,
    pub remaining: u64,
    pub reset_after_ns: u64,
    pub retry_after_ns: u64,
}

// ---------------------------------------------------------------------------
// RateLimitBackend trait
// ---------------------------------------------------------------------------

/// Backend for rate-limit checks. Async-ready via RPITIT for future Redis backend.
pub trait RateLimitBackend: Send + Sync + 'static {
    fn check(&self, key: &str) -> impl Future<Output = RateLimitOutcome> + Send;
}

// ---------------------------------------------------------------------------
// InMemoryBackend (GCRA)
// ---------------------------------------------------------------------------

/// In-memory GCRA rate limiter using `DashMap` for lock-free concurrent access.
///
/// Each key stores a TAT (Theoretical Arrival Time) as an `AtomicU64` in nanoseconds.
pub struct InMemoryBackend {
    states: Arc<DashMap<String, AtomicU64>>,
    /// Emission interval in nanoseconds (time between allowed requests).
    t_ns: u64,
    /// Burst tolerance in nanoseconds.
    tau_ns: u64,
    /// Configured rate limit (requests per period).
    limit: u64,
    /// Configured burst size.
    burst: u64,
}

impl InMemoryBackend {
    /// Create a backend allowing `rate` requests per second.
    pub fn per_second(rate: u64) -> Self {
        let t_ns = 1_000_000_000 / rate;
        Self {
            states: Arc::new(DashMap::new()),
            t_ns,
            tau_ns: (rate - 1) * t_ns, // default burst = rate
            limit: rate,
            burst: rate,
        }
    }

    /// Create a backend allowing `rate` requests per minute.
    pub fn per_minute(rate: u64) -> Self {
        let t_ns = 60_000_000_000 / rate;
        Self {
            states: Arc::new(DashMap::new()),
            t_ns,
            tau_ns: (rate - 1) * t_ns,
            limit: rate,
            burst: rate,
        }
    }

    /// Set the burst size (max requests allowed in an instant).
    pub fn burst(mut self, burst: u64) -> Self {
        self.burst = burst;
        self.tau_ns = (burst - 1) * self.t_ns;
        self
    }

    /// Spawn a background task that evicts stale keys at `interval`.
    /// Keys whose TAT is in the past are removed.
    pub fn start_sweeper(&self, interval: Duration) {
        let states = Arc::clone(&self.states);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let now = now_ns();
                states.retain(|_, tat| tat.load(Ordering::Relaxed) > now);
            }
        });
    }

    /// GCRA check against a single AtomicU64 TAT.
    fn gcra_check(&self, tat: &AtomicU64, now: u64) -> RateLimitOutcome {
        loop {
            let old_tat = tat.load(Ordering::Relaxed);
            let tat_val = if old_tat == 0 { now } else { old_tat };

            // Check if request is within the burst window
            if now < tat_val.saturating_sub(self.tau_ns) {
                // Denied: too many requests
                let retry_after_ns = tat_val.saturating_sub(self.tau_ns) - now;
                let reset_after_ns = tat_val.saturating_sub(now);
                return RateLimitOutcome {
                    allowed: false,
                    limit: self.limit,
                    remaining: 0,
                    reset_after_ns,
                    retry_after_ns,
                };
            }

            let new_tat = tat_val.max(now) + self.t_ns;
            if tat
                .compare_exchange_weak(old_tat, new_tat, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // Allowed: remaining = how many more requests fit before TAT
                // exceeds the allowed window (now + tau + T).
                let max_tat = now + self.tau_ns + self.t_ns;
                let remaining = max_tat.saturating_sub(new_tat) / self.t_ns;
                let remaining = remaining.min(self.burst);
                let reset_after_ns = new_tat.saturating_sub(now);
                return RateLimitOutcome {
                    allowed: true,
                    limit: self.limit,
                    remaining,
                    reset_after_ns,
                    retry_after_ns: 0,
                };
            }
            // CAS failed, retry
        }
    }

    fn check_sync(&self, key: &str) -> RateLimitOutcome {
        let now = now_ns();
        // Fast path: existing key (read lock, no String alloc)
        if let Some(entry) = self.states.get(key) {
            return self.gcra_check(entry.value(), now);
        }
        // Slow path: new key (write lock, allocates String)
        let entry = self
            .states
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        self.gcra_check(entry.value(), now)
    }
}

impl RateLimitBackend for InMemoryBackend {
    fn check(&self, key: &str) -> impl Future<Output = RateLimitOutcome> + Send {
        let outcome = self.check_sync(key);
        std::future::ready(outcome)
    }
}

// ---------------------------------------------------------------------------
// KeyExtractor
// ---------------------------------------------------------------------------

/// Extracts a rate-limit key from a request. Returns `None` to skip rate limiting.
pub trait KeyExtractor: Send + Sync + 'static {
    fn extract(&self, req: &Request) -> Option<String>;
}

/// Blanket impl for closures.
impl<F> KeyExtractor for F
where
    F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
{
    fn extract(&self, req: &Request) -> Option<String> {
        (self)(req)
    }
}

/// Extracts a rate-limit key from a specific header.
pub struct HeaderKeyExtractor {
    header_name: &'static str,
}

impl HeaderKeyExtractor {
    pub fn new(header_name: &'static str) -> Self {
        Self { header_name }
    }
}

impl KeyExtractor for HeaderKeyExtractor {
    fn extract(&self, req: &Request) -> Option<String> {
        req.header(self.header_name).map(|s| s.to_string())
    }
}

// ---------------------------------------------------------------------------
// RateLimitHeaderStyle
// ---------------------------------------------------------------------------

/// Controls which rate-limit headers are added to responses.
pub enum RateLimitHeaderStyle {
    /// Add `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`.
    Legacy,
    /// No rate-limit headers (only `Retry-After` on 429).
    None,
}

// ---------------------------------------------------------------------------
// RateLimitMiddleware
// ---------------------------------------------------------------------------

/// Rate-limiting middleware.
pub struct RateLimitMiddleware<K: KeyExtractor, B: RateLimitBackend> {
    key_extractor: K,
    backend: Arc<B>,
    header_style: RateLimitHeaderStyle,
}

/// Create a rate-limiting middleware with `Legacy` header style.
pub fn rate_limit_middleware<K: KeyExtractor, B: RateLimitBackend>(
    backend: B,
    key_extractor: K,
) -> RateLimitMiddleware<K, B> {
    RateLimitMiddleware {
        key_extractor,
        backend: Arc::new(backend),
        header_style: RateLimitHeaderStyle::Legacy,
    }
}

impl<K: KeyExtractor, B: RateLimitBackend> RateLimitMiddleware<K, B> {
    /// Set the header style.
    pub fn header_style(mut self, style: RateLimitHeaderStyle) -> Self {
        self.header_style = style;
        self
    }
}

impl<K: KeyExtractor, B: RateLimitBackend> Middleware for RateLimitMiddleware<K, B> {
    fn call(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let key = self.key_extractor.extract(&req);

        match key {
            None => {
                // No key → skip rate limiting
                Box::pin(next.run(req))
            }
            Some(key) => {
                let backend = Arc::clone(&self.backend);
                let use_legacy = matches!(self.header_style, RateLimitHeaderStyle::Legacy);

                Box::pin(async move {
                    let outcome = backend.check(&key).await;

                    if !outcome.allowed {
                        let retry_after = ns_to_secs_ceil(outcome.retry_after_ns);
                        let mut resp =
                            Response::new(http::StatusCode::TOO_MANY_REQUESTS, "too many requests");
                        resp = resp.header("retry-after", &retry_after.to_string());
                        if use_legacy {
                            resp = resp
                                .header("x-ratelimit-limit", &outcome.limit.to_string())
                                .header("x-ratelimit-remaining", "0")
                                .header(
                                    "x-ratelimit-reset",
                                    &ns_to_secs_ceil(outcome.reset_after_ns).to_string(),
                                );
                        }
                        return resp;
                    }

                    let resp = next.run(req).await;

                    if use_legacy {
                        resp.header("x-ratelimit-limit", &outcome.limit.to_string())
                            .header("x-ratelimit-remaining", &outcome.remaining.to_string())
                            .header(
                                "x-ratelimit-reset",
                                &ns_to_secs_ceil(outcome.reset_after_ns).to_string(),
                            )
                    } else {
                        resp
                    }
                })
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    fn make_request(headers: &[(&str, &str)]) -> Request {
        let mut builder = http::Request::builder().method("GET").uri("/");
        for &(name, value) in headers {
            builder = builder.header(name, value);
        }
        let inner = builder
            .body(harrow_core::request::full_body(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
            .unwrap();
        Request::new(inner, PathMatch::default(), Arc::new(TypeMap::new()), None)
    }

    fn ok_next() -> Next {
        Next::new(|_req| Box::pin(async { Response::ok() }))
    }

    // -- GCRA algorithm tests -----------------------------------------------

    #[tokio::test]
    async fn gcra_allows_under_rate() {
        let backend = InMemoryBackend::per_second(10).burst(10);
        for _ in 0..10 {
            let outcome = backend.check("key").await;
            assert!(outcome.allowed);
        }
    }

    #[tokio::test]
    async fn gcra_denies_over_rate() {
        let backend = InMemoryBackend::per_second(5).burst(5);
        // Use up all the burst
        for _ in 0..5 {
            let outcome = backend.check("key").await;
            assert!(outcome.allowed);
        }
        // Next request should be denied
        let outcome = backend.check("key").await;
        assert!(!outcome.allowed);
        assert!(outcome.retry_after_ns > 0);
    }

    #[tokio::test]
    async fn gcra_burst_allows_n_requests_instantly() {
        let backend = InMemoryBackend::per_second(2).burst(5);
        for i in 0..5 {
            let outcome = backend.check("key").await;
            assert!(outcome.allowed, "request {i} should be allowed");
        }
        let outcome = backend.check("key").await;
        assert!(!outcome.allowed, "request after burst should be denied");
    }

    #[tokio::test]
    async fn gcra_recovery_after_waiting() {
        let backend = InMemoryBackend::per_second(10).burst(1);
        // First request allowed
        let outcome = backend.check("key").await;
        assert!(outcome.allowed);
        // Second denied (burst=1)
        let outcome = backend.check("key").await;
        assert!(!outcome.allowed);
        // Wait for the emission interval (100ms for 10/s)
        tokio::time::sleep(Duration::from_millis(110)).await;
        // Should be allowed again
        let outcome = backend.check("key").await;
        assert!(outcome.allowed);
    }

    #[tokio::test]
    async fn gcra_different_keys_independent() {
        let backend = InMemoryBackend::per_second(1).burst(1);
        let outcome = backend.check("key-a").await;
        assert!(outcome.allowed);
        // key-a is exhausted
        let outcome = backend.check("key-a").await;
        assert!(!outcome.allowed);
        // key-b is independent
        let outcome = backend.check("key-b").await;
        assert!(outcome.allowed);
    }

    #[tokio::test]
    async fn gcra_remaining_decrements() {
        let backend = InMemoryBackend::per_second(5).burst(5);
        let o1 = backend.check("key").await;
        let o2 = backend.check("key").await;
        assert!(o1.remaining > o2.remaining, "remaining should decrease");
    }

    #[tokio::test]
    async fn gcra_per_minute_rate() {
        let backend = InMemoryBackend::per_minute(60).burst(1);
        // 60/minute = 1/second, burst=1
        let outcome = backend.check("key").await;
        assert!(outcome.allowed);
        let outcome = backend.check("key").await;
        assert!(!outcome.allowed);
    }

    // -- Key extractor tests ------------------------------------------------

    #[test]
    fn header_key_extractor_present() {
        let extractor = HeaderKeyExtractor::new("x-api-key");
        let req = make_request(&[("x-api-key", "abc123")]);
        assert_eq!(extractor.extract(&req), Some("abc123".to_string()));
    }

    #[test]
    fn header_key_extractor_missing() {
        let extractor = HeaderKeyExtractor::new("x-api-key");
        let req = make_request(&[]);
        assert_eq!(extractor.extract(&req), None);
    }

    #[test]
    fn closure_key_extractor() {
        let extractor = |req: &Request| req.header("x-forwarded-for").map(|s| s.to_string());
        let req = make_request(&[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(extractor.extract(&req), Some("1.2.3.4".to_string()));
    }

    // -- Middleware tests ---------------------------------------------------

    #[tokio::test]
    async fn middleware_429_with_retry_after_when_denied() {
        let backend = InMemoryBackend::per_second(1).burst(1);
        let mw = rate_limit_middleware(backend, HeaderKeyExtractor::new("x-api-key"));
        let req = make_request(&[("x-api-key", "test")]);
        // First request: allowed
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);

        // Second request: denied
        let req = make_request(&[("x-api-key", "test")]);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::TOO_MANY_REQUESTS);
        let inner = resp.into_inner();
        assert!(
            inner.headers().get("retry-after").is_some(),
            "expected retry-after header on 429"
        );
    }

    #[tokio::test]
    async fn middleware_rate_limit_headers_when_allowed() {
        let backend = InMemoryBackend::per_second(10).burst(10);
        let mw = rate_limit_middleware(backend, HeaderKeyExtractor::new("x-api-key"));
        let req = make_request(&[("x-api-key", "test")]);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
        let inner = resp.into_inner();
        assert!(inner.headers().get("x-ratelimit-limit").is_some());
        assert!(inner.headers().get("x-ratelimit-remaining").is_some());
        assert!(inner.headers().get("x-ratelimit-reset").is_some());
    }

    #[tokio::test]
    async fn middleware_skips_when_key_missing() {
        let backend = InMemoryBackend::per_second(1).burst(1);
        let mw = rate_limit_middleware(backend, HeaderKeyExtractor::new("x-api-key"));
        // No x-api-key header → skip rate limiting, always allowed
        for _ in 0..5 {
            let req = make_request(&[]);
            let resp = mw.call(req, ok_next()).await;
            assert_eq!(resp.status_code(), http::StatusCode::OK);
            let inner = resp.into_inner();
            assert!(
                inner.headers().get("x-ratelimit-limit").is_none(),
                "should not have rate limit headers when key missing"
            );
        }
    }

    #[tokio::test]
    async fn middleware_no_headers_with_none_style() {
        let backend = InMemoryBackend::per_second(10).burst(10);
        let mw = rate_limit_middleware(backend, HeaderKeyExtractor::new("x-api-key"))
            .header_style(RateLimitHeaderStyle::None);
        let req = make_request(&[("x-api-key", "test")]);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
        let inner = resp.into_inner();
        assert!(inner.headers().get("x-ratelimit-limit").is_none());
        assert!(inner.headers().get("x-ratelimit-remaining").is_none());
        assert!(inner.headers().get("x-ratelimit-reset").is_none());
    }

    // -- Helper tests -------------------------------------------------------

    #[test]
    fn ns_to_secs_ceil_rounds_up() {
        assert_eq!(ns_to_secs_ceil(0), 0);
        assert_eq!(ns_to_secs_ceil(1), 1);
        assert_eq!(ns_to_secs_ceil(999_999_999), 1);
        assert_eq!(ns_to_secs_ceil(1_000_000_000), 1);
        assert_eq!(ns_to_secs_ceil(1_000_000_001), 2);
        assert_eq!(ns_to_secs_ceil(2_500_000_000), 3);
    }
}
