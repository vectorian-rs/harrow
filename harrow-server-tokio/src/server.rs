use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::{JoinSet, LocalSet};

use harrow_core::dispatch::SharedState;
use harrow_core::route::App;

use crate::ServerConfig;
use crate::connection::handle_tcp_connection;

/// Serve the application on the given address (single-runtime mode).
pub async fn serve(app: App, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_config(
        app,
        addr,
        futures_util::future::pending(),
        ServerConfig::default(),
    )
    .await
}

/// Serve with one tokio `current_thread` runtime per CPU core.
///
/// Each worker binds to the same address via `SO_REUSEPORT` and runs an
/// independent accept loop. No work-stealing, no cross-thread wakeups.
pub fn serve_multi_worker(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let workers = config.worker_count();
    let shutdown = harrow_server::ShutdownSignal::new();

    tracing::info!("harrow listening on {addr} [{workers} workers, SO_REUSEPORT, harrow-codec-h1]");

    let handles = harrow_server::spawn_workers(workers, "harrow-w", {
        let shared = Arc::clone(&shared);
        let shutdown = shutdown.clone();
        let config = config.clone();
        move |worker_id| {
            let shared = Arc::clone(&shared);
            let shutdown = shutdown.clone();
            let config = config.clone();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");

            rt.block_on(async move {
                let std_listener = harrow_server::reuseport_listener(addr)
                    .expect("failed to bind SO_REUSEPORT listener");
                let listener =
                    TcpListener::from_std(std_listener).expect("failed to convert listener");
                let local = LocalSet::new();

                local
                    .run_until(worker_loop(shared, listener, config, shutdown, worker_id))
                    .await;
            });
        }
    })?;

    harrow_server::join_workers(handles).map_err(|e| -> Box<dyn std::error::Error> { e.into() })
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
    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("harrow listening on {addr} [harrow-codec-h1]");

    let shutdown_signal = harrow_server::ShutdownSignal::new();
    let local = LocalSet::new();

    tokio::pin!(shutdown);

    local
        .run_until(async move {
            let mut worker = std::pin::pin!(worker_loop(
                shared,
                listener,
                config,
                shutdown_signal.clone(),
                0
            ));

            tokio::select! {
                () = &mut worker => {}
                () = &mut shutdown => {
                    tracing::info!("harrow shutting down");
                    shutdown_signal.shutdown();
                    worker.await;
                }
            }
        })
        .await;

    Ok(())
}

async fn worker_loop(
    shared: Arc<SharedState>,
    listener: TcpListener,
    config: ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
    worker_id: usize,
) {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let per_worker_max = config.per_worker_max_connections();
    let mut connections = JoinSet::new();

    loop {
        reap_finished_connections(&mut connections);

        if shutdown.is_shutdown() {
            break;
        }

        let result = tokio::select! {
            r = listener.accept() => r,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                if shutdown.is_shutdown() { break; }
                continue;
            }
        };

        let (stream, _remote) = match result {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(worker = worker_id, "accept error: {e}");
                continue;
            }
        };

        if active.load(std::sync::atomic::Ordering::Relaxed) >= per_worker_max {
            drop(stream);
            continue;
        }

        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let shared = Arc::clone(&shared);
        let config2 = config.clone();
        let active2 = Arc::clone(&active);
        let shutdown2 = shutdown.clone();

        connections.spawn_local(async move {
            let _active = ActiveConnectionGuard::new(active2);
            if let Err(e) = handle_tcp_connection(stream, shared, &config2, shutdown2).await {
                tracing::debug!("connection error: {e}");
            }
        });
    }

    drain_connection_tasks(&mut connections, config.drain_timeout).await;
}

struct ActiveConnectionGuard {
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl ActiveConnectionGuard {
    fn new(active: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self { active }
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn reap_finished_connections(connections: &mut JoinSet<()>) {
    while let Some(result) = connections.try_join_next() {
        if let Err(err) = result {
            tracing::debug!("connection task join error: {err}");
        }
    }
}

async fn drain_connection_tasks(connections: &mut JoinSet<()>, drain_timeout: Duration) {
    let drained = tokio::time::timeout(drain_timeout, async {
        while let Some(result) = connections.join_next().await {
            if let Err(err) = result {
                tracing::debug!("connection task join error: {err}");
            }
        }
    })
    .await;

    if drained.is_err() {
        tracing::warn!(
            "drain timeout ({}s) exceeded, aborting remaining connections",
            drain_timeout.as_secs()
        );
        connections.abort_all();
        while let Some(result) = connections.join_next().await {
            if let Err(err) = result {
                tracing::debug!("connection task join error: {err}");
            }
        }
    }
}
