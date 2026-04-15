use std::future::Future;
use std::sync::Arc;

use harrow_core::handler::HandlerFuture;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

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
    fn call(&self, req: Request, next: Next) -> HandlerFuture {
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
