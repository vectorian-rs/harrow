mod codec;
mod connection;

use std::cell::Cell;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use monoio::net::TcpListener;

use harrow_core::dispatch::SharedState;
use harrow_core::route::{App, RouteTable};

/// Configuration for the monoio server.
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Timeout for reading HTTP headers from a new connection. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection. Default: Some(5 min).
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
///
/// This is an async function intended to run inside a monoio runtime:
///
/// ```ignore
/// fn main() {
///     let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
///         .enable_timer()
///         .build()
///         .unwrap();
///     rt.block_on(async {
///         let app = App::new().get("/hello", hello);
///         harrow_server_monoio::serve(app, addr).await.unwrap();
///     });
/// }
/// ```
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

    let listener = TcpListener::bind(addr)?;
    tracing::info!("harrow-monoio listening on {addr}");

    let active_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));

    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        monoio::select! {
            result = listener.accept() => {
                let (stream, _remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                        continue;
                    }
                };

                if active_count.get() >= config.max_connections {
                    drop(stream);
                    tracing::warn!("connection limit reached, dropping new connection");
                    continue;
                }

                let shared = Arc::clone(&shared);
                let header_read_timeout = config.header_read_timeout;
                let connection_timeout = config.connection_timeout;
                let counter = Rc::clone(&active_count);

                monoio::spawn(connection::handle_connection(
                    stream,
                    shared,
                    header_read_timeout,
                    connection_timeout,
                    counter,
                ));
            }
            () = &mut shutdown => {
                tracing::info!("harrow-monoio shutting down");
                break;
            }
        }
    }

    // Graceful drain: wait for in-flight connections to complete.
    let drain_start = std::time::Instant::now();
    while active_count.get() > 0 {
        if drain_start.elapsed() >= config.drain_timeout {
            tracing::warn!(
                "drain timeout ({}s) exceeded, {} connections still active",
                config.drain_timeout.as_secs(),
                active_count.get(),
            );
            break;
        }
        monoio::time::sleep(Duration::from_millis(10)).await;
    }

    if active_count.get() == 0 {
        tracing::info!("all connections drained");
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
