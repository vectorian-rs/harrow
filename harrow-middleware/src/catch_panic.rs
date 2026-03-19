use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

/// Middleware that catches panics in downstream handlers and returns a
/// **500 Internal Server Error** instead of killing the connection task.
///
/// Without this middleware, a panic inside a handler causes Tokio to drop the
/// connection task silently — the client sees a TCP reset with no HTTP response.
///
/// # How it works
///
/// Wraps `next.run(req)` with [`futures_util::FutureExt::catch_unwind`], which
/// calls `std::panic::catch_unwind` around every `poll()`. This catches panics
/// in both the sync future-creation path and async `.await` points.
///
/// On the happy path (no panic), `catch_unwind` has **zero runtime overhead** —
/// it uses the zero-cost exception model where unwind tables are only consulted
/// when a panic actually occurs.
///
/// # Limitations
///
/// - Does nothing when `panic = "abort"` is set — the whole process dies.
/// - The panic payload is **not** included in the response body for security.
pub async fn catch_panic_middleware(req: Request, next: Next) -> Response {
    match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(response) => response,
        Err(_panic) => Response::new(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    fn call(req: Request, next: Next) -> impl std::future::Future<Output = Response> + Send {
        harrow_core::middleware::Middleware::call(&catch_panic_middleware, req, next)
    }

    async fn make_request() -> Request {
        let inner = http::Request::builder()
            .method("GET")
            .uri("/")
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
    async fn normal_passthrough() {
        let req = make_request().await;
        let resp = call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn catches_async_panic() {
        let next = Next::new(|_req| {
            Box::pin(async {
                panic!("handler exploded");
            })
        });
        let req = make_request().await;
        let resp = call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn catches_sync_panic() {
        let next = Next::new(|_req| {
            panic!("panic during future creation");
        });
        let req = make_request().await;
        let resp = call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn preserves_response_headers() {
        let next =
            Next::new(|_req| Box::pin(async { Response::ok().header("x-custom", "preserved") }));
        let req = make_request().await;
        let resp = call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-custom").unwrap(), "preserved");
    }

    #[tokio::test]
    async fn string_panic_payload() {
        let next = Next::new(|_req| {
            Box::pin(async {
                panic!("{}", format!("detailed error: code={}", 42));
            })
        });
        let req = make_request().await;
        let resp = call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
