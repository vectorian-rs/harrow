use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::FutureExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::{App, RouteTable};
use harrow_core::state::TypeMap;
use std::pin::Pin;

/// Serve the application on the given address.
pub async fn serve(app: App, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let (route_table, middleware, state) = app.into_parts();
    let shared = Arc::new(SharedState {
        route_table,
        middleware,
        state: Arc::new(state),
    });

    print_route_table(&shared.route_table);

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("harrow listening on {addr}");

    loop {
        let (stream, _remote) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("accept error: {e}");
                // Exponential backoff or sleep could be added here to avoid log spam
                // during FD exhaustion, but continue is the minimal fix for correctness.
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let shared = Arc::clone(&shared);

        tokio::spawn(async move {
            let service = service_fn(move |req: http::Request<Incoming>| {
                let shared = Arc::clone(&shared);
                async move { Ok::<_, std::convert::Infallible>(dispatch_safe(shared, req).await) }
            });

            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await
            {
                tracing::error!("connection error: {e}");
            }
        });
    }
}

/// Serve with a graceful shutdown signal.
pub async fn serve_with_shutdown(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (route_table, middleware, state) = app.into_parts();
    let shared = Arc::new(SharedState {
        route_table,
        middleware,
        state: Arc::new(state),
    });

    print_route_table(&shared.route_table);

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("harrow listening on {addr}");

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let shared = Arc::clone(&shared);

                tokio::spawn(async move {
                    let service = service_fn(move |req: http::Request<Incoming>| {
                        let shared = Arc::clone(&shared);
                        async move { Ok::<_, std::convert::Infallible>(dispatch_safe(shared, req).await) }
                    });

                    if let Err(e) =
                        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                            .serve_connection(io, service)
                            .await
                    {
                        tracing::error!("connection error: {e}");
                    }
                });
            }
            () = &mut shutdown => {
                tracing::info!("harrow shutting down");
                break;
            }
        }
    }

    Ok(())
}

struct SharedState {
    route_table: RouteTable,
    middleware: Vec<Box<dyn Middleware>>,
    state: Arc<TypeMap>,
}

/// Catch panics from dispatch and convert to 500 responses.
async fn dispatch_safe(
    shared: Arc<SharedState>,
    hyper_req: http::Request<Incoming>,
) -> http::Response<Full<Bytes>> {
    match AssertUnwindSafe(dispatch(shared, hyper_req))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(panic_payload) => {
            let msg = panic_payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            tracing::error!("handler panicked: {msg}");
            Response::new(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
            )
            .into_inner()
        }
    }
}

#[cfg_attr(feature = "profiling", inline(never))]
async fn dispatch(
    shared: Arc<SharedState>,
    hyper_req: http::Request<Incoming>,
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

    let route = shared
        .route_table
        .get(route_idx)
        .expect("valid route index");
    let route_pattern = Some(route.pattern.as_arc_str());
    let req = Request::new(
        hyper_req,
        path_match,
        Arc::clone(&shared.state),
        route_pattern,
    );

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

fn print_route_table(table: &RouteTable) {
    if table.is_empty() {
        return;
    }
    for route in table.iter() {
        let method = format!("{:6}", route.method.as_str());
        let pattern = route.pattern.as_str();
        let name = route
            .metadata
            .name
            .as_deref()
            .map(|n| format!(" [{n}]"))
            .unwrap_or_default();
        let tags = if route.metadata.tags.is_empty() {
            String::new()
        } else {
            format!("  tags: {}", route.metadata.tags.join(", "))
        };
        let mw = if route.middleware.is_empty() {
            String::new()
        } else {
            format!("  ({}mw)", route.middleware.len())
        };
        tracing::info!("  {method} {pattern}{name}{tags}{mw}");
    }
}
