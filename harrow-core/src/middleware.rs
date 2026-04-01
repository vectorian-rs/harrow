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

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

/// Middleware that transforms the request before passing it to the next handler.
///
/// ```rust,ignore
/// app.middleware(map_request(|mut req| {
///     req.set_ext(RequestStart(std::time::Instant::now()));
///     req
/// }))
/// ```
pub fn map_request<F>(f: F) -> MapRequest<F>
where
    F: Fn(Request) -> Request + Send + Sync + 'static,
{
    MapRequest(f)
}

pub struct MapRequest<F>(F);

impl<F> Middleware for MapRequest<F>
where
    F: Fn(Request) -> Request + Send + Sync + 'static,
{
    fn call(&self, req: Request, next: Next) -> BoxFuture {
        let req = (self.0)(req);
        Box::pin(next.run(req))
    }
}

/// Middleware that transforms the response after the handler runs.
///
/// ```rust,ignore
/// app.middleware(map_response(|resp| {
///     resp.header("x-served-by", "harrow")
/// }))
/// ```
pub fn map_response<F>(f: F) -> MapResponse<F>
where
    F: Fn(Response) -> Response + Send + Sync + 'static,
{
    MapResponse(std::sync::Arc::new(f))
}

pub struct MapResponse<F>(std::sync::Arc<F>);

impl<F> Middleware for MapResponse<F>
where
    F: Fn(Response) -> Response + Send + Sync + 'static,
{
    fn call(&self, req: Request, next: Next) -> BoxFuture {
        let f = std::sync::Arc::clone(&self.0);
        Box::pin(async move {
            let resp = next.run(req).await;
            (f)(resp)
        })
    }
}

/// Middleware that conditionally applies another middleware based on a predicate.
///
/// ```rust,ignore
/// app.middleware(when(
///     |req| req.path().starts_with("/api"),
///     auth_middleware,
/// ))
/// ```
pub fn when<P, M>(predicate: P, middleware: M) -> When<P, M>
where
    P: Fn(&Request) -> bool + Send + Sync + 'static,
    M: Middleware + 'static,
{
    When {
        predicate,
        middleware,
    }
}

pub struct When<P, M> {
    predicate: P,
    middleware: M,
}

impl<P, M> Middleware for When<P, M>
where
    P: Fn(&Request) -> bool + Send + Sync + 'static,
    M: Middleware + 'static,
{
    fn call(&self, req: Request, next: Next) -> BoxFuture {
        if (self.predicate)(&req) {
            self.middleware.call(req, next)
        } else {
            Box::pin(next.run(req))
        }
    }
}

/// Middleware that applies another middleware only when the predicate is false.
///
/// ```rust,ignore
/// app.middleware(unless(
///     |req| req.path() == "/health",
///     logging_middleware,
/// ))
/// ```
pub fn unless<P, M>(predicate: P, middleware: M) -> Unless<P, M>
where
    P: Fn(&Request) -> bool + Send + Sync + 'static,
    M: Middleware + 'static,
{
    Unless {
        predicate,
        middleware,
    }
}

pub struct Unless<P, M> {
    predicate: P,
    middleware: M,
}

impl<P, M> Middleware for Unless<P, M>
where
    P: Fn(&Request) -> bool + Send + Sync + 'static,
    M: Middleware + 'static,
{
    fn call(&self, req: Request, next: Next) -> BoxFuture {
        if (self.predicate)(&req) {
            Box::pin(next.run(req))
        } else {
            self.middleware.call(req, next)
        }
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
    async fn map_request_transforms_before_handler() {
        let mw = map_request(|mut req| {
            req.set_ext(42u32);
            req
        });

        let next = Next::new(|req| {
            Box::pin(async move {
                let val = req.ext::<u32>().copied().unwrap_or(0);
                Response::text(format!("{val}"))
            })
        });
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
        let resp = Middleware::call(&mw, req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn map_response_transforms_after_handler() {
        let mw = map_response(|resp| resp.header("x-added", "true"));

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
        let resp = Middleware::call(&mw, req, next).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-added").unwrap(), "true");
    }

    #[tokio::test]
    async fn when_applies_middleware_on_match() {
        async fn add_header(req: Request, next: Next) -> Response {
            next.run(req).await.header("x-auth", "checked")
        }

        let mw = when(|req| req.path().starts_with("/api"), add_header);

        // Matching path — middleware runs
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/api/users",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&mw, req, next).await;
        assert!(resp.into_inner().headers().contains_key("x-auth"));

        // Non-matching path — middleware skipped
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/health",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&mw, req, next).await;
        assert!(!resp.into_inner().headers().contains_key("x-auth"));
    }

    #[tokio::test]
    async fn unless_skips_middleware_on_match() {
        async fn logging(req: Request, next: Next) -> Response {
            next.run(req).await.header("x-logged", "true")
        }

        let mw = unless(|req| req.path() == "/health", logging);

        // Health path — middleware skipped
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/health",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&mw, req, next).await;
        assert!(!resp.into_inner().headers().contains_key("x-logged"));

        // Other path — middleware runs
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(
            "GET",
            "/api/users",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = Middleware::call(&mw, req, next).await;
        assert!(resp.into_inner().headers().contains_key("x-logged"));
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

    #[tokio::test]
    async fn map_response_on_error_status() {
        let mw = map_response(|resp| resp.header("x-always", "yes"));

        let next = Next::new(|_req| {
            Box::pin(async { Response::new(http::StatusCode::INTERNAL_SERVER_ERROR, "fail") })
        });
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
        let resp = Middleware::call(&mw, req, next).await;
        let inner = resp.into_inner();
        assert_eq!(inner.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(inner.headers().get("x-always").unwrap(), "yes");
    }

    #[tokio::test]
    async fn chained_combinators() {
        use crate::dispatch::{SharedState, dispatch};
        use std::sync::Arc;

        let app = crate::route::App::new()
            .middleware(map_request(|mut req| {
                req.set_ext(42u32);
                req
            }))
            .middleware(map_response(|resp| resp.header("x-framework", "harrow")))
            .middleware(when(
                |req| req.path().starts_with("/api"),
                map_response(|resp| resp.header("x-api", "true")),
            ))
            .get("/api/test", |req: crate::request::Request| async move {
                let val = req.ext::<u32>().copied().unwrap_or(0);
                Response::text(format!("{val}"))
            })
            .get("/health", |_req: crate::request::Request| async move {
                Response::text("ok")
            });

        let client = app.client();

        // /api/test — all three combinators apply
        let resp = client.get("/api/test").await;
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(resp.header("x-framework"), Some("harrow"));
        assert_eq!(resp.header("x-api"), Some("true"));

        // /health — map_request + map_response apply, when skips
        let resp = client.get("/health").await;
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(resp.header("x-framework"), Some("harrow"));
        assert!(resp.header("x-api").is_none());
    }

    #[tokio::test]
    async fn unless_with_chained_middleware() {
        async fn logging(req: crate::request::Request, next: Next) -> Response {
            next.run(req).await.header("x-logged", "true")
        }

        let app = crate::route::App::new()
            .middleware(unless(|req| req.path() == "/health", logging))
            .middleware(map_response(|resp| resp.header("x-always", "yes")))
            .get("/health", |_req: crate::request::Request| async move {
                Response::text("ok")
            })
            .get("/api/data", |_req: crate::request::Request| async move {
                Response::text("data")
            });

        let client = app.client();

        let resp = client.get("/health").await;
        assert!(resp.header("x-logged").is_none());
        assert_eq!(resp.header("x-always"), Some("yes"));

        let resp = client.get("/api/data").await;
        assert_eq!(resp.header("x-logged"), Some("true"));
        assert_eq!(resp.header("x-always"), Some("yes"));
    }

    #[tokio::test]
    async fn when_with_group_middleware() {
        let app = crate::route::App::new()
            .middleware(when(
                |req| req.path().starts_with("/api"),
                map_response(|resp| resp.header("x-api", "true")),
            ))
            .group("/api", |g| {
                g.middleware(map_response(|resp| resp.header("x-group", "yes")))
                    .get("/users", |_req: crate::request::Request| async move {
                        Response::text("users")
                    })
            })
            .get("/health", |_req: crate::request::Request| async move {
                Response::text("ok")
            });

        let client = app.client();

        let resp = client.get("/api/users").await;
        assert_eq!(resp.header("x-api"), Some("true"));
        assert_eq!(resp.header("x-group"), Some("yes"));

        let resp = client.get("/health").await;
        assert!(resp.header("x-api").is_none());
        assert!(resp.header("x-group").is_none());
    }

    use proptest::prelude::*;

    proptest! {
        /// Combinator composition: N map_response layers each add a header.
        /// The final response should have all N headers.
        #[test]
        fn proptest_chained_map_response(n in 1usize..=8) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let mut app = crate::route::App::new()
                    .get("/test", |_req: crate::request::Request| async move {
                        Response::text("ok")
                    });

                for i in 0..n {
                    let key = format!("x-test-{}", i);
                    app = app.middleware(map_response(move |resp| {
                        resp.header(&key, "yes")
                    }));
                }

                let client = app.client();
                let resp = client.get("/test").await;
                prop_assert_eq!(resp.status(), http::StatusCode::OK);

                for i in 0..n {
                    let key = format!("x-test-{}", i);
                    prop_assert!(
                        resp.header(&key).is_some(),
                        "missing header {} after {} layers",
                        key,
                        n
                    );
                }
                Ok(())
            })?;
        }

        /// when + unless are complementary: for any predicate, exactly one fires.
        #[test]
        fn proptest_when_unless_complementary(path_is_api: bool) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let predicate = |req: &crate::request::Request| req.path().starts_with("/api");

                let app = crate::route::App::new()
                    .middleware(when(predicate, map_response(|r| r.header("x-when", "yes"))))
                    .middleware(unless(predicate, map_response(|r| r.header("x-unless", "yes"))))
                    .get("/api/test", |_req: crate::request::Request| async move {
                        Response::text("api")
                    })
                    .get("/other", |_req: crate::request::Request| async move {
                        Response::text("other")
                    });

                let client = app.client();
                let path = if path_is_api { "/api/test" } else { "/other" };
                let resp = client.get(path).await;

                let has_when = resp.header("x-when").is_some();
                let has_unless = resp.header("x-unless").is_some();

                // Exactly one should fire
                prop_assert!(
                    has_when ^ has_unless,
                    "path={}, when={}, unless={} — expected exactly one",
                    path, has_when, has_unless
                );

                if path_is_api {
                    prop_assert!(has_when);
                } else {
                    prop_assert!(has_unless);
                }
                Ok(())
            })?;
        }
    }
}
