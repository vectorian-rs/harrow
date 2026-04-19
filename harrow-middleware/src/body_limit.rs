use harrow_core::handler::HandlerFuture;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

/// Middleware that rejects requests whose body exceeds `max_size` bytes.
///
/// If a `Content-Length` header is present and exceeds the limit, the request
/// is rejected immediately with **413 Payload Too Large** without reading the
/// body.  For chunked requests (no `Content-Length`), the streaming
/// `max_body_size` on [`Request`] is set so the limit is enforced during
/// `body_bytes()`.
pub struct BodyLimitMiddleware {
    max_size: usize,
}

/// Create a [`BodyLimitMiddleware`] that rejects bodies larger than `max_size` bytes.
pub fn body_limit_middleware(max_size: usize) -> BodyLimitMiddleware {
    BodyLimitMiddleware { max_size }
}

impl Middleware for BodyLimitMiddleware {
    fn call(&self, mut req: Request, next: Next) -> HandlerFuture {
        let max_size = self.max_size;
        Box::pin(async move {
            if let Some(content_length) = req.header("content-length")
                && let Ok(len) = content_length.parse::<u64>()
                && len > max_size as u64
            {
                return Response::new(http::StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
            }
            req.set_max_body_size(max_size);
            next.run(req).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    fn make_request_with_content_length(len: u64) -> Request {
        let inner = http::Request::builder()
            .method("POST")
            .uri("/upload")
            .header("content-length", len.to_string())
            .body(harrow_core::request::full_body(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
            .unwrap();
        Request::new(inner, PathMatch::default(), Arc::new(TypeMap::new()), None)
    }

    fn make_request_no_content_length() -> Request {
        let inner = http::Request::builder()
            .method("POST")
            .uri("/upload")
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
    async fn over_limit_returns_413() {
        let mw = body_limit_middleware(1024);
        let req = make_request_with_content_length(2048);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn under_limit_passes_through() {
        let mw = body_limit_middleware(1024);
        let req = make_request_with_content_length(512);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn exact_limit_passes_through() {
        let mw = body_limit_middleware(1024);
        let req = make_request_with_content_length(1024);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn no_content_length_passes_through() {
        let mw = body_limit_middleware(1024);
        let req = make_request_no_content_length();
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }
}
