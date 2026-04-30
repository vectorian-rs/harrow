//! Tokio + Hyper server backend for Harrow.
//!
//! This backend is intentionally additive to `harrow-server-tokio`, which uses
//! Harrow's custom HTTP/1 codec. Here Hyper owns HTTP protocol parsing,
//! request-body decoding, keep-alive, pipelining, and response framing while
//! Harrow still owns the app/router/middleware model and worker topology.
//!
//! The multi-worker entrypoint follows the Tako-style topology Harrow wants to
//! evaluate before 1.0:
//!
//! - one OS thread per worker;
//! - one Tokio `current_thread` runtime per worker;
//! - one `LocalSet` per worker;
//! - one listener per worker via `SO_REUSEPORT` where supported;
//! - local connection tasks with no cross-thread task migration.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body as HarrowBody;
use harrow_core::route::App;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinSet, LocalSet};

pub use harrow_server::ServerConfig;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Serve the application on the given address using Hyper in single-runtime mode.
pub async fn serve(app: App, addr: SocketAddr) -> Result<(), BoxError> {
    serve_with_config(
        app,
        addr,
        futures_util::future::pending(),
        ServerConfig::default(),
    )
    .await
}

/// Serve with a graceful shutdown signal using Hyper in single-runtime mode.
pub async fn serve_with_shutdown(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<(), BoxError> {
    serve_with_config(app, addr, shutdown, ServerConfig::default()).await
}

/// Serve with a graceful shutdown signal and custom configuration.
pub async fn serve_with_config(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
    config: ServerConfig,
) -> Result<(), BoxError> {
    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("harrow listening on {addr} [hyper/http1]");

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
                0,
            ));

            tokio::select! {
                () = &mut worker => {}
                () = &mut shutdown => {
                    tracing::info!("harrow hyper backend shutting down");
                    shutdown_signal.shutdown();
                    worker.await;
                }
            }
        })
        .await;

    Ok(())
}

/// Serve with one Tokio `current_thread` runtime per worker.
///
/// Each worker binds to the same address via `SO_REUSEPORT` and runs an
/// independent accept loop. Hyper owns HTTP/1 protocol handling; Harrow owns
/// dispatch and server lifecycle policy.
pub fn serve_multi_worker<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), BoxError>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    let workers = config.worker_count();
    let shutdown = harrow_server::ShutdownSignal::new();

    tracing::info!("harrow listening on {addr} [{workers} workers, SO_REUSEPORT, hyper/http1]");

    let handles = harrow_server::spawn_workers(workers, "harrow-hyper-w", {
        let make_app = make_app.clone();
        let shutdown = shutdown.clone();
        let config = config.clone();
        move |worker_id| {
            let app = make_app();
            let shared = app.into_shared_state();
            if worker_id == 0 {
                shared.route_table.print_routes();
            }
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

    harrow_server::join_workers(handles).map_err(|e| -> BoxError { e.into() })
}

async fn worker_loop(
    shared: Arc<SharedState>,
    listener: TcpListener,
    config: ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
    worker_id: usize,
) {
    let active = Arc::new(AtomicUsize::new(0));
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

        let (stream, remote_addr) = match result {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(worker = worker_id, "accept error: {e}");
                continue;
            }
        };

        if active.load(Ordering::Relaxed) >= per_worker_max {
            drop(stream);
            continue;
        }

        active.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::clone(&shared);
        let config2 = config.clone();
        let active2 = Arc::clone(&active);
        let shutdown2 = shutdown.clone();

        connections.spawn_local(async move {
            let _active = ActiveConnectionGuard::new(active2);
            if let Err(e) =
                handle_connection(stream, remote_addr, shared, &config2, shutdown2).await
            {
                if e.is::<hyper::Error>() {
                    tracing::debug!(worker = worker_id, "hyper connection error: {e}");
                } else {
                    tracing::debug!(worker = worker_id, "connection error: {e}");
                }
            }
        });
    }

    drain_connection_tasks(&mut connections, config.drain_timeout).await;
}

async fn handle_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    shared: Arc<SharedState>,
    config: &ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
) -> Result<(), BoxError> {
    stream.set_nodelay(true)?;
    let io = TokioIo::new(stream);

    let svc = service_fn(move |mut req: http::Request<Incoming>| {
        let shared = Arc::clone(&shared);
        async move {
            req.extensions_mut().insert(remote_addr);
            let req = req.map(hyper_body_to_harrow_body);
            let response = dispatch(shared, req).await;
            Ok::<_, Infallible>(response)
        }
    });

    let mut http = http1::Builder::new();
    http.keep_alive(true);
    http.pipeline_flush(true);
    http.timer(TokioTimer::new());
    http.header_read_timeout(config.header_read_timeout);

    let conn = http.serve_connection(io, svc).with_upgrades();

    if let Some(timeout) = config.connection_timeout {
        tokio::select! {
            result = conn => {
                result.map_err(|err| -> BoxError { Box::new(err) })?;
            }
            () = tokio::time::sleep(timeout) => {
                tracing::debug!("hyper connection lifetime exceeded; closing");
            }
            () = wait_for_shutdown(shutdown) => {}
        }
    } else {
        tokio::select! {
            result = conn => {
                result.map_err(|err| -> BoxError { Box::new(err) })?;
            }
            () = wait_for_shutdown(shutdown) => {}
        }
    }

    Ok(())
}

fn hyper_body_to_harrow_body(body: Incoming) -> HarrowBody {
    HarrowBody::new(body.map_err(|err| -> BoxError { Box::new(err) }))
}

async fn wait_for_shutdown(shutdown: harrow_server::ShutdownSignal) {
    while !shutdown.is_shutdown() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct ActiveConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl ActiveConnectionGuard {
    fn new(active: Arc<AtomicUsize>) -> Self {
        Self { active }
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
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
            "drain timeout exceeded; aborting {} remaining hyper connections",
            connections.len()
        );
        connections.abort_all();
    }
}
