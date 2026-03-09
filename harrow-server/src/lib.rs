use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
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
                async move { Ok::<_, std::convert::Infallible>(dispatch(shared, req).await) }
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
                        async move { Ok::<_, std::convert::Infallible>(dispatch(shared, req).await) }
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

#[cfg_attr(feature = "profiling", inline(never))]
async fn dispatch(
    shared: Arc<SharedState>,
    hyper_req: http::Request<Incoming>,
) -> http::Response<Full<Bytes>> {
    // Borrow method and path directly from hyper_req — no clone, no to_string().
    // Both borrows end before hyper_req is moved into Request::new().
    let (route_idx, path_match) = match shared
        .route_table
        .match_route_idx(hyper_req.method(), hyper_req.uri().path())
    {
        Some(found) => found,
        None => {
            // Zero-alloc 405 vs 404 check — PathPattern::matches does not
            // capture params, so no String allocations.
            let resp = if shared
                .route_table
                .any_route_matches_path(hyper_req.uri().path())
            {
                Response::new(http::StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            } else {
                Response::new(http::StatusCode::NOT_FOUND, "not found")
            };
            return resp.into_inner();
        }
    };

    let route = shared.route_table.get(route_idx).expect("valid route index");
    let route_pattern = Some(route.pattern.as_arc_str());
    let req = Request::new(hyper_req, path_match, Arc::clone(&shared.state), route_pattern);

    // Fast path: no middleware at all — call handler directly, avoid chain setup.
    if shared.middleware.is_empty() && route.middleware.is_empty() {
        let resp = (route.handler)(req).await;
        return resp.into_inner();
    }

    let resp = run_middleware_chain(shared, route_idx, 0, req).await;
    resp.into_inner()
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
    let route = shared.route_table.get(route_idx).expect("valid route index");
    let total = global_len + route.middleware.len();

    if mw_idx >= total {
        // End of chain — call the handler.
        (route.handler)(req)
    } else if mw_idx < global_len {
        // Global middleware.
        let shared_for_next = Arc::clone(&shared);
        let next = Next::new(move |req| {
            run_middleware_chain(shared_for_next, route_idx, mw_idx + 1, req)
        });
        shared.middleware[mw_idx].call(req, next)
    } else {
        // Route-level middleware (from groups).
        let route_mw_idx = mw_idx - global_len;
        let shared_for_next = Arc::clone(&shared);
        let next = Next::new(move |req| {
            run_middleware_chain(shared_for_next, route_idx, mw_idx + 1, req)
        });
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
