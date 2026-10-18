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

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn into_public_error(err: BoxError) -> Box<dyn std::error::Error> {
    err
}

/// Serve with one tokio `current_thread` runtime per worker, blocking until
/// the server stops. Same as `start_with_config(make_app, addr, config)?.wait()`;
/// use [`start_with_config`] to shut down gracefully on a signal.
pub fn serve_multi_worker<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    start_with_config(make_app, addr, config)?.wait()
}

/// Handle returned by [`start`] / [`start_with_config`].
///
/// Dropping the handle signals shutdown and waits for all workers to exit.
pub struct ServerHandle {
    inner: harrow_server::ThreadedServerHandle<BoxError>,
}

impl ServerHandle {
    /// The socket address the server bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    /// Stop accepting, let in-flight connections finish (up to
    /// `ServerConfig::drain_timeout`), and wait for all workers to exit.
    pub fn shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
        self.inner.shutdown().map_err(into_public_error)
    }

    /// Block until a worker exits, then shut down the rest.
    pub fn wait(self) -> Result<(), Box<dyn std::error::Error>> {
        self.inner.wait().map_err(into_public_error)
    }
}

/// Start with default configuration; see [`start_with_config`].
pub fn start<F>(make_app: F, addr: SocketAddr) -> Result<ServerHandle, Box<dyn std::error::Error>>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    start_with_config(make_app, addr, ServerConfig::default())
}

/// Start one tokio `current_thread` runtime per worker and return once every
/// worker is accepting connections.
///
/// Each worker builds its own `App` with `make_app`, binds the same address
/// via `SO_REUSEPORT`, and runs an independent accept loop: no work-stealing,
/// no cross-thread wakeups. Binding port 0 works; [`ServerHandle::local_addr`]
/// reports the port.
pub fn start_with_config<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<ServerHandle, Box<dyn std::error::Error>>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    let workers = config.worker_count();
    let inner = harrow_server::start_workers(
        addr,
        workers,
        harrow_server::noop_wake_workers as fn(SocketAddr, usize),
        move |ctx| run_worker(&make_app, &config, ctx),
    )
    .map_err(into_public_error)?;

    tracing::info!(
        "harrow listening on {} [{workers} workers, SO_REUSEPORT, harrow-codec-h1]",
        inner.local_addr()
    );
    Ok(ServerHandle { inner })
}

/// One worker thread: its own `current_thread` runtime, `LocalSet`, and
/// `SO_REUSEPORT` listener.
fn run_worker<F>(
    make_app: &F,
    config: &ServerConfig,
    ctx: harrow_server::WorkerContext,
) -> Result<(), BoxError>
where
    F: Fn() -> App,
{
    let shared = make_app().into_shared_state();
    if ctx.index == 0 {
        shared.route_table.print_routes();
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let std_listener = harrow_server::reuseport_listener(ctx.addr)?;
        let local_addr = std_listener.local_addr()?;
        let listener = TcpListener::from_std(std_listener)?;
        ctx.ready.listening(local_addr);

        LocalSet::new()
            .run_until(worker_loop(
                shared,
                listener,
                config.clone(),
                ctx.shutdown,
                ctx.index,
            ))
            .await;
        Ok(())
    })
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
