use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Full};

use crate::middleware::{Middleware, Next};
use crate::path::PathMatch;
use crate::request::{Body, Request};
use crate::response::{Response, ResponseBody};
use crate::route::{MethodNotAllowedHandler, NotFoundHandler, RouteTable};
use crate::state::TypeMap;

/// Shared state passed to every request. Constructed from `App::into_parts()`.
pub struct SharedState {
    pub route_table: RouteTable,
    pub middleware: Vec<Box<dyn Middleware>>,
    pub state: Arc<TypeMap>,
    pub max_body_size: usize,
}

/// Dispatch a request through the routing and middleware pipeline.
#[cfg_attr(feature = "profiling", inline(never))]
pub async fn dispatch(
    shared: Arc<SharedState>,
    hyper_req: http::Request<Body>,
) -> http::Response<ResponseBody> {
    let method = hyper_req.method().clone();
    let path = hyper_req.uri().path();

    // Try exact method match first, then HEAD→GET fallback (RFC 9110 §9.3.2).
    let (match_result, is_head_fallback) = match shared.route_table.match_route_idx(&method, path) {
        Some(found) => (Some(found), false),
        None if method == http::Method::HEAD => (
            shared.route_table.match_route_idx(&http::Method::GET, path),
            true,
        ),
        None => (None, false),
    };

    let (route_idx, path_match) = match match_result {
        Some(found) => found,
        None => {
            if shared.route_table.any_route_matches_path(path) {
                let methods = shared.route_table.allowed_methods(path);
                return method_not_allowed_response(&shared, hyper_req, methods).await;
            } else {
                return not_found_response(&shared, hyper_req).await;
            }
        }
    };

    // Content-Length pre-check: reject obviously oversized bodies before reading.
    if shared.max_body_size > 0
        && let Some(cl) = hyper_req
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
        && cl > shared.max_body_size
    {
        return Response::new(StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_inner();
    }

    let route = shared
        .route_table
        .get(route_idx)
        .expect("valid route index");
    let route_pattern = Some(route.pattern.as_arc_str());
    let mut req = Request::new(
        hyper_req,
        path_match,
        Arc::clone(&shared.state),
        route_pattern,
    );
    req.set_max_body_size(shared.max_body_size);

    // Fast path: no middleware at all — call handler directly, avoid chain setup.
    let response = if shared.middleware.is_empty() && route.middleware.is_empty() {
        (route.handler)(req).await.into_inner()
    } else {
        run_middleware_chain(shared, route_idx, 0, req)
            .await
            .into_inner()
    };

    // HEAD fallback: strip body, keep status + headers (RFC 9110 §9.3.2).
    if is_head_fallback {
        let (parts, _body) = response.into_parts();
        let empty = Full::new(Bytes::new())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
            .boxed();
        http::Response::from_parts(parts, empty)
    } else {
        response
    }
}

async fn not_found_response(
    shared: &SharedState,
    hyper_req: http::Request<Body>,
) -> http::Response<ResponseBody> {
    let is_head = hyper_req.method() == Method::HEAD;
    let req = unmatched_request(shared, hyper_req);
    let response = if let Some(handler) = shared.state.try_get::<NotFoundHandler>() {
        let fut = (handler.0)(req);
        fut.await.status(StatusCode::NOT_FOUND.as_u16())
    } else {
        Response::new(StatusCode::NOT_FOUND, "not found")
    };

    finalize_unmatched_response(response.into_inner(), is_head)
}

async fn method_not_allowed_response(
    shared: &SharedState,
    hyper_req: http::Request<Body>,
    methods: Vec<Method>,
) -> http::Response<ResponseBody> {
    let is_head = hyper_req.method() == Method::HEAD;
    let allow_value = methods
        .iter()
        .map(|method| method.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let req = unmatched_request(shared, hyper_req);

    let response = if let Some(handler) = shared.state.try_get::<MethodNotAllowedHandler>() {
        let fut = (handler.0)(req, methods);
        fut.await
            .status(StatusCode::METHOD_NOT_ALLOWED.as_u16())
            .header("allow", &allow_value)
    } else {
        Response::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            .header("allow", &allow_value)
    };

    finalize_unmatched_response(response.into_inner(), is_head)
}

fn unmatched_request(shared: &SharedState, hyper_req: http::Request<Body>) -> Request {
    let mut req = Request::new(
        hyper_req,
        PathMatch::default(),
        Arc::clone(&shared.state),
        None,
    );
    req.set_max_body_size(shared.max_body_size);
    req
}

fn finalize_unmatched_response(
    response: http::Response<ResponseBody>,
    is_head: bool,
) -> http::Response<ResponseBody> {
    if is_head {
        let (parts, _body) = response.into_parts();
        let empty = Full::new(Bytes::new())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
            .boxed();
        http::Response::from_parts(parts, empty)
    } else {
        response
    }
}

/// Recursively build and execute the middleware chain.
///
/// Uses a combined index over global middleware (0..global_len) and then
/// route-level middleware (global_len..global_len + route_mw_len). After
/// both are exhausted, calls the handler.
///
/// Each recursion captures a fresh `Arc` clone — one `Arc::clone` per middleware
/// layer per request. This is the only per-request allocation in the chain
/// beyond the boxed futures themselves.
#[cfg_attr(feature = "profiling", inline(never))]
fn run_middleware_chain(
    shared: Arc<SharedState>,
    route_idx: usize,
    mw_idx: usize,
    req: Request,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let global_len = shared.middleware.len();
    let route = shared
        .route_table
        .get(route_idx)
        .expect("valid route index");
    let total = global_len + route.middleware.len();

    if mw_idx >= total {
        // End of chain — call the handler.
        (route.handler)(req)
    } else if mw_idx < global_len {
        // Global middleware.
        let shared_for_next = Arc::clone(&shared);
        let next =
            Next::new(move |req| run_middleware_chain(shared_for_next, route_idx, mw_idx + 1, req));
        shared.middleware[mw_idx].call(req, next)
    } else {
        // Route-level middleware (from groups).
        let route_mw_idx = mw_idx - global_len;
        let shared_for_next = Arc::clone(&shared);
        let next =
            Next::new(move |req| run_middleware_chain(shared_for_next, route_idx, mw_idx + 1, req));
        route.middleware[route_mw_idx].call(req, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::App;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    use std::sync::Mutex;

    /// Middleware that logs its index into a shared vec when called.
    struct IndexMiddleware {
        index: usize,
        log: Arc<Mutex<Vec<usize>>>,
    }

    impl Middleware for IndexMiddleware {
        fn call(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
            self.log.lock().unwrap().push(self.index);
            Box::pin(async move { next.run(req).await })
        }
    }

    /// Middleware that short-circuits without calling next.
    struct ShortCircuitMiddleware {
        index: usize,
        log: Arc<Mutex<Vec<usize>>>,
    }

    impl Middleware for ShortCircuitMiddleware {
        fn call(
            &self,
            _req: Request,
            _next: Next,
        ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
            self.log.lock().unwrap().push(self.index);
            Box::pin(async { Response::new(http::StatusCode::FORBIDDEN, "blocked") })
        }
    }

    proptest! {
        /// Middleware execute in order: global[0..N], route[N..N+M], then handler.
        /// Handler is called exactly once.
        #[test]
        fn proptest_middleware_ordering(n_global in 0usize..=4, n_route in 0usize..=4) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let log = Arc::new(Mutex::new(Vec::new()));
                let handler_log = Arc::clone(&log);
                let sentinel = usize::MAX;

                let mut app = App::new();
                // Add N global middleware
                for i in 0..n_global {
                    app = app.middleware(IndexMiddleware {
                        index: i,
                        log: Arc::clone(&log),
                    });
                }

                // Add route with M route-level middleware via group
                if n_route > 0 {
                    app = app.group("/", |mut g| {
                        for i in 0..n_route {
                            g = g.middleware(IndexMiddleware {
                                index: n_global + i,
                                log: Arc::clone(&log),
                            });
                        }
                        g.get("/test", move |_req| {
                            let log = Arc::clone(&handler_log);
                            async move {
                                log.lock().unwrap().push(sentinel);
                                Response::ok()
                            }
                        })
                    });
                } else {
                    app = app.get("/test", move |_req| {
                        let log = Arc::clone(&handler_log);
                        async move {
                            log.lock().unwrap().push(sentinel);
                            Response::ok()
                        }
                    });
                }

                let client = app.client();
                let resp = client.get("/test").await;
                prop_assert_eq!(resp.status(), http::StatusCode::OK);

                let trace = log.lock().unwrap().clone();
                let total = n_global + n_route;
                // Expect [0, 1, ..., total-1, MAX]
                let mut expected: Vec<usize> = (0..total).collect();
                expected.push(sentinel);
                prop_assert_eq!(
                    &trace, &expected,
                    "n_global={}, n_route={}", n_global, n_route,
                );
                Ok::<_, TestCaseError>(())
            })?;
        }

        /// Short-circuit: if middleware K returns early, K+1..N and handler are skipped.
        #[test]
        fn proptest_short_circuit(
            n_total in 2usize..=5,
            cut_at in 0usize..=4,
        ) {
            let cut_at = cut_at.min(n_total - 1);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let log = Arc::new(Mutex::new(Vec::new()));
                let handler_log = Arc::clone(&log);

                let mut app = App::new();
                for i in 0..n_total {
                    if i == cut_at {
                        app = app.middleware(ShortCircuitMiddleware {
                            index: i,
                            log: Arc::clone(&log),
                        });
                    } else {
                        app = app.middleware(IndexMiddleware {
                            index: i,
                            log: Arc::clone(&log),
                        });
                    }
                }

                app = app.get("/test", move |_req| {
                    let log = Arc::clone(&handler_log);
                    async move {
                        log.lock().unwrap().push(usize::MAX);
                        Response::ok()
                    }
                });

                let client = app.client();
                let resp = client.get("/test").await;
                prop_assert_eq!(resp.status(), http::StatusCode::FORBIDDEN);

                let trace = log.lock().unwrap().clone();
                // Only middleware 0..=cut_at should have run
                let expected: Vec<usize> = (0..=cut_at).collect();
                prop_assert_eq!(
                    &trace, &expected,
                    "n_total={}, cut_at={}", n_total, cut_at,
                );
                Ok::<_, TestCaseError>(())
            })?;
        }

        /// Fast path (0 middleware) produces the same status as the slow path.
        #[test]
        fn proptest_fast_path_agrees_with_slow_path(n_identity in 0usize..=4) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // Fast path: no middleware
                let fast = App::new()
                    .get("/test", |_req| async { Response::text("hello") })
                    .client();
                let fast_resp = fast.get("/test").await;

                // Slow path: N identity middleware
                let log = Arc::new(Mutex::new(Vec::new()));
                let mut app = App::new();
                for i in 0..n_identity {
                    app = app.middleware(IndexMiddleware {
                        index: i,
                        log: Arc::clone(&log),
                    });
                }
                app = app.get("/test", |_req| async { Response::text("hello") });
                let slow = app.client();
                let slow_resp = slow.get("/test").await;

                prop_assert_eq!(fast_resp.status(), slow_resp.status());
                prop_assert_eq!(fast_resp.text(), slow_resp.text());
                Ok::<_, TestCaseError>(())
            })?;
        }
    }
}
