use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::box_incoming;
use harrow_core::route::{App, RouteTable};

/// Configuration for server connection handling.
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Timeout for reading HTTP headers from a new connection. Default: Some(5s).
    /// Set to `None` to disable (eliminates per-connection timer overhead).
    pub header_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection. Default: Some(5 min).
    /// Set to `None` to disable (eliminates per-connection timer overhead).
    pub connection_timeout: Option<Duration>,
    /// Time to wait for in-flight requests to complete during shutdown. Default: 30s.
    pub drain_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            header_read_timeout: Some(Duration::from_secs(5)),
            connection_timeout: Some(Duration::from_secs(300)),
            drain_timeout: Duration::from_secs(30),
        }
    }
}

/// Serve the application on the given address.
pub async fn serve(app: App, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_config(
        app,
        addr,
        futures_util::future::pending(),
        ServerConfig::default(),
    )
    .await
}

/// Serve with a graceful shutdown signal.
pub async fn serve_with_shutdown(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_config(app, addr, shutdown, ServerConfig::default()).await
}

/// Serve with a graceful shutdown signal and custom configuration.
pub async fn serve_with_config(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
    config: ServerConfig,
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

    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections: JoinSet<()> = JoinSet::new();

    tokio::pin!(shutdown);

    loop {
        // Reap completed tasks before accepting new ones.
        while connections.try_join_next().is_some() {}

        tokio::select! {
            result = listener.accept() => {
                let (stream, _remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                        continue;
                    }
                };

                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        // At connection limit — drop the TCP stream immediately.
                        drop(stream);
                        tracing::warn!("connection limit reached, dropping new connection");
                        continue;
                    }
                };

                let io = TokioIo::new(stream);
                let shared = Arc::clone(&shared);
                let header_read_timeout = config.header_read_timeout;
                let connection_timeout = config.connection_timeout;

                connections.spawn(async move {
                    let _permit = permit; // held until task completes

                    let service = service_fn(move |req: http::Request<Incoming>| {
                        let shared = Arc::clone(&shared);
                        async move {
                            let boxed = req.map(box_incoming);
                            Ok::<_, std::convert::Infallible>(dispatch(shared, boxed).await)
                        }
                    });

                    let mut builder = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Some(hrt) = header_read_timeout {
                        builder.http1()
                            .timer(hyper_util::rt::TokioTimer::new())
                            .header_read_timeout(hrt);
                    }
                    let conn = builder.serve_connection(io, service);

                    if let Some(ct) = connection_timeout {
                        match tokio::time::timeout(ct, conn).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => tracing::error!("connection error: {e}"),
                            Err(_) => tracing::warn!("connection timed out"),
                        }
                    } else if let Err(e) = conn.await {
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

    // Graceful drain: wait for in-flight connections to complete.
    match tokio::time::timeout(config.drain_timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    {
        Ok(()) => tracing::info!("all connections drained"),
        Err(_) => {
            tracing::warn!(
                "drain timeout ({}s) exceeded, aborting remaining connections",
                config.drain_timeout.as_secs()
            );
            connections.abort_all();
        }
    }

    Ok(())
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
