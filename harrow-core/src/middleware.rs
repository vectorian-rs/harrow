use std::future::Future;
use std::pin::Pin;

use crate::request::Request;
use crate::response::Response;

/// A boxed, `Send` future that resolves to a `Response`.
type BoxFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// A middleware function. Receives the request and a `Next` handle to call
/// the remainder of the chain (or the final handler).
pub trait Middleware: Send + Sync {
    fn call(&self, req: Request, next: Next) -> BoxFuture;
}

/// Blanket impl: any matching async function is a Middleware.
impl<F, Fut> Middleware for F
where
    F: Fn(Request, Next) -> Fut + Send + Sync,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn call(&self, req: Request, next: Next) -> BoxFuture {
        Box::pin((self)(req, next))
    }
}

/// Handle to the next middleware or the final handler.
pub struct Next {
    inner: Box<dyn FnOnce(Request) -> BoxFuture + Send>,
}

impl Next {
    pub fn new(f: impl FnOnce(Request) -> BoxFuture + Send + 'static) -> Self {
        Self { inner: Box::new(f) }
    }

    /// Call the next middleware/handler in the chain.
    pub async fn run(self, req: Request) -> Response {
        (self.inner)(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::PathMatch;
    use crate::request::test_util::make_request;
    use crate::state::TypeMap;

    #[tokio::test]
    async fn next_calls_inner_handler() {
        let next = Next::new(|_req| Box::pin(async { Response::text("inner") }));
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = next.run(req).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn async_fn_implements_middleware() {
        async fn my_mw(req: Request, next: Next) -> Response {
            next.run(req).await
        }

        let next = Next::new(|_req| Box::pin(async { Response::text("handler") }));
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&my_mw, req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_can_modify_response() {
        async fn add_header(req: Request, next: Next) -> Response {
            next.run(req).await.header("x-added", "true")
        }

        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&add_header, req, next).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-added").unwrap(), "true");
    }

    #[tokio::test]
    async fn middleware_can_short_circuit() {
        async fn auth_mw(req: Request, next: Next) -> Response {
            if req.header("authorization").is_none() {
                return Response::new(http::StatusCode::UNAUTHORIZED, "unauthorized");
            }
            next.run(req).await
        }

        // Without auth header — short circuits
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&auth_mw, req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::UNAUTHORIZED);

        // With auth header — passes through
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/",
            &[("authorization", "Bearer tok")],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&auth_mw, req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }
}
