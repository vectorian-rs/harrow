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

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::box_incoming;
use harrow_core::response::Response;
use harrow_core::route::{App, RouteTable};

/// Serve the application on the given address.
pub async fn serve(app: App, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let (route_table, middleware, state, max_body_size) = app.into_parts();
    let shared = Arc::new(SharedState {
        route_table,
        middleware,
        state: Arc::new(state),
        max_body_size,
    });

    print_route_table(&shared.route_table);

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("harrow listening on {addr}");

    loop {
        let (stream, _remote) = match listener.accept().await {
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
}

/// Serve with a graceful shutdown signal.
pub async fn serve_with_shutdown(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (route_table, middleware, state, max_body_size) = app.into_parts();
    let shared = Arc::new(SharedState {
        route_table,
        middleware,
        state: Arc::new(state),
        max_body_size,
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

/// Catch panics from dispatch and convert to 500 responses.
async fn dispatch_safe(
    shared: Arc<SharedState>,
    hyper_req: http::Request<Incoming>,
) -> http::Response<Full<Bytes>> {
    // Box the Incoming body at the server boundary.
    let boxed = hyper_req.map(box_incoming);
    match AssertUnwindSafe(dispatch(shared, boxed))
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
