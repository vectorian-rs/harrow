use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;

use crate::middleware::{Middleware, Next};
use crate::request::{Body, Request};
use crate::response::Response;
use crate::route::RouteTable;
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
) -> http::Response<Full<Bytes>> {
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
            let resp = if shared.route_table.any_route_matches_path(path) {
                let methods = shared.route_table.allowed_methods(path);
                let allow_value = methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Response::new(http::StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                    .header("allow", &allow_value)
            } else {
                Response::new(http::StatusCode::NOT_FOUND, "not found")
            };
            return resp.into_inner();
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
        return Response::new(http::StatusCode::PAYLOAD_TOO_LARGE, "payload too large")
            .into_inner();
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
        http::Response::from_parts(parts, Full::new(Bytes::new()))
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
