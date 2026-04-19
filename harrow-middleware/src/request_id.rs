use std::sync::Arc;

use harrow_core::handler::HandlerFuture;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

/// The default header name used for request IDs.
pub const DEFAULT_HEADER: &str = "x-request-id";

/// Middleware that sets a request ID header on every response.
///
/// If the request already carries the header, its value is preserved;
/// otherwise a random 32-character hex ID is generated (no external deps).
///
/// Uses `x-request-id` by default. Call [`request_id_middleware_with_header`]
/// to use a custom header name (e.g. `x-amz-cf-id`).
pub async fn request_id_middleware(req: Request, next: Next) -> Response {
    run(DEFAULT_HEADER, req, next).await
}

/// Returns a middleware that uses a custom header name for the request ID.
///
/// ```ignore
/// app.middleware(request_id_middleware_with_header("x-amz-cf-id"))
/// ```
pub fn request_id_middleware_with_header(header: impl Into<String>) -> RequestIdMiddleware {
    RequestIdMiddleware {
        header: Arc::from(header.into()),
    }
}

pub struct RequestIdMiddleware {
    header: Arc<str>,
}

impl Middleware for RequestIdMiddleware {
    fn call(&self, req: Request, next: Next) -> HandlerFuture {
        let header = Arc::clone(&self.header);
        Box::pin(async move { run(&header, req, next).await })
    }
}

async fn run(header: &str, req: Request, next: Next) -> Response {
    let id = req
        .header(header)
        .map(|s| s.to_string())
        .unwrap_or_else(generate_hex_id);

    let resp = next.run(req).await;
    resp.header(header, &id)
}

/// Generate a 32-character hex string using the standard library's
/// random `DefaultHasher` seed as an entropy source.
fn generate_hex_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut buf = [0u8; 16];
    // Two 64-bit hashes → 16 bytes → 32 hex chars.
    let h1 = RandomState::new().build_hasher().finish();
    let h2 = RandomState::new().build_hasher().finish();
    buf[..8].copy_from_slice(&h1.to_le_bytes());
    buf[8..].copy_from_slice(&h2.to_le_bytes());

    let mut hex = String::with_capacity(32);
    for byte in &buf {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;

    async fn make_request(headers: &[(&str, &str)]) -> Request {
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

    #[tokio::test]
    async fn sets_request_id_when_absent() {
        let req = make_request(&[]).await;
        let resp = Middleware::call(&request_id_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        let rid = inner
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(rid.len(), 32);
        assert!(rid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn preserves_existing_request_id() {
        let req = make_request(&[("x-request-id", "my-custom-id")]).await;
        let resp = Middleware::call(&request_id_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        let rid = inner
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(rid, "my-custom-id");
    }

    #[tokio::test]
    async fn custom_header_name() {
        let mw = request_id_middleware_with_header("x-amz-cf-id");
        let req = make_request(&[("x-amz-cf-id", "cloudfront-abc123")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner
                .headers()
                .get("x-amz-cf-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "cloudfront-abc123"
        );
        // Default header should NOT be set.
        assert!(inner.headers().get("x-request-id").is_none());
    }

    #[tokio::test]
    async fn custom_header_generates_id_when_absent() {
        let mw = request_id_middleware_with_header("x-amz-cf-id");
        let req = make_request(&[]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        let rid = inner
            .headers()
            .get("x-amz-cf-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(rid.len(), 32);
        assert!(rid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn generates_unique_ids() {
        let id1 = generate_hex_id();
        let id2 = generate_hex_id();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn empty_header_value_treated_as_present() {
        let req = make_request(&[("x-request-id", "")]).await;
        let resp = Middleware::call(&request_id_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        let rid = inner
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        // Empty string is preserved — the header existed, so no ID is generated.
        assert_eq!(rid, "");
    }
}
