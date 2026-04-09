//! Monoio-based HTTP/1.1 and HTTP/2 server for Harrow.
//!
//! This crate provides a high-performance HTTP server using io_uring.
//! It supports HTTP/1.1 with keep-alive and chunked transfer encoding,
//! and HTTP/2 with multiplexed streams.
//!
//! # Features
//!
//! - **io_uring-based I/O**: Zero-copy where possible, minimal syscalls
//! - **Cancellation Safety**: Proper handling of io_uring operation cancellation
//! - **Buffer Pooling**: Reusable buffers to reduce allocator pressure
//! - **HTTP/2 Support**: Multiplexed streams with flow control
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │              Server (lib.rs)                │
//! └─────────────────────┬───────────────────────┘
//!                       │ TcpStream
//!                       ▼
//! ┌─────────────────────────────────────────────┐
//! │           Connection Handler                │
//! │            (connection.rs)                  │
//! └─────────────────────┬───────────────────────┘
//!                       │
//!           ┌───────────┴───────────┐
//!           ▼                       ▼
//! ┌─────────────────┐   ┌─────────────────────┐
//! │   H1 Handler    │   │   H2 Handler        │
//! │    (h1.rs)      │   │    (h2.rs)          │
//! └─────────────────┘   └─────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! fn main() {
//!     let app = App::new().get("/hello", hello);
//!
//!     // High-level thread-per-core bootstrap.
//!     harrow_server_monoio::run(app, "127.0.0.1:3000".parse().unwrap()).unwrap();
//! }
//! ```
//!
//! For advanced cases where you already own a monoio runtime, use the async
//! `serve` / `serve_with_shutdown` / `serve_with_config` entrypoints instead.
//!
//! # Cancellation Safety
//!
//! This crate uses io_uring for async I/O. Unlike epoll-based runtimes,
//! io_uring submits actual kernel operations. Dropping a Rust future does
//! NOT automatically cancel the in-flight kernel operation.
//!
//! This can lead to use-after-free (UAF) vulnerabilities:
//! 1. A read operation is submitted with a user buffer
//! 2. The future is dropped (e.g., due to timeout)
//! 3. The kernel writes to the buffer after it's been freed/reused
//!
//! ## Mitigation
//!
//! All I/O operations with timeout paths use `CancelableAsyncReadRent` and
//! explicitly cancel kernel operations before returning:
//!
//! ```rust,ignore
//! let canceller = Canceller::new();
//! let handle = canceller.handle();
//!
//! monoio::select! {
//!     result = stream.cancelable_read(buf, handle) => result,
//!     _ = timeout => {
//!         canceller.cancel(); // Explicit kernel cancellation
//!         // Await the operation to reclaim buffer
//!         let (_, buf) = read_fut.await;
//!         release_buffer(buf);
//!         return Err(Timeout);
//!     }
//! }
//! ```
//!
//! See `cancel.rs` for the implementation details.

mod buffer;
mod cancel;
mod codec;
mod connection;
mod h1;
mod h2;
/// Kernel version and io_uring availability checks.
pub mod kernel_check;
mod o11y;
mod protocol;

use std::cell::Cell;
use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use monoio::net::{ListenerOpts, TcpListener};

use harrow_core::dispatch::SharedState;
use harrow_core::route::App;

use connection::ProtocolVersion;

type BoxError = Box<dyn Error + Send + Sync>;

/// Configuration for the monoio server.
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Maximum concurrent HTTP/2 streams per connection. Default: 256.
    pub max_h2_streams: u32,
    /// Total worker threads for the high-level `run` / `start` APIs.
    ///
    /// `None` defaults to `std::thread::available_parallelism()`. The async
    /// `serve*` APIs always run on a single monoio runtime and will reject
    /// values greater than 1.
    pub workers: Option<usize>,
    /// Timeout for reading HTTP headers from a new connection. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Timeout for reading request bodies after headers are complete. Default: Some(30s).
    pub body_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection. Default: Some(5 min).
    pub connection_timeout: Option<Duration>,
    /// Time to wait for in-flight requests to complete during shutdown. Default: 30s.
    pub drain_timeout: Duration,
    /// Enable HTTP/2 support (prior knowledge). Default: false.
    ///
    /// When enabled, connections are assumed to use HTTP/2 directly
    /// (no protocol negotiation). This is suitable for internal services
    /// or load balancers that route H2 traffic to dedicated ports.
    pub enable_http2: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            max_h2_streams: 256,
            workers: None,
            header_read_timeout: Some(Duration::from_secs(5)),
            body_read_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(300)),
            drain_timeout: Duration::from_secs(30),
            enable_http2: false,
        }
    }
}

/// Handle returned by the high-level monoio bootstrap APIs.
///
/// Dropping the handle signals shutdown and waits for all workers to exit.
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    completion: mpsc::Receiver<Result<(), String>>,
    workers: Vec<thread::JoinHandle<Result<(), BoxError>>>,
}

impl ServerHandle {
    /// The socket address the server bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal shutdown and wait for all workers to exit.
    pub fn shutdown(mut self) -> Result<(), Box<dyn Error>> {
        self.shutdown.store(true, Ordering::Release);
        self.join_workers().map_err(into_public_error)
    }

    /// Wait for the workers to exit.
    ///
    /// This blocks until shutdown is triggered or a worker exits with an
    /// unexpected error.
    pub fn wait(mut self) -> Result<(), Box<dyn Error>> {
        let _ = self.completion.recv();
        self.shutdown.store(true, Ordering::Release);
        self.join_workers().map_err(into_public_error)
    }

    fn join_workers(&mut self) -> Result<(), BoxError> {
        let mut first_error: Option<BoxError> = None;

        for worker in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if first_error.is_none() {
                        self.shutdown.store(true, Ordering::Release);
                        first_error = Some(err);
                    }
                }
                Err(panic) => {
                    if first_error.is_none() {
                        self.shutdown.store(true, Ordering::Release);
                        first_error = Some(join_panic_error(panic));
                    }
                }
            }
        }

        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Start the application using Harrow's thread-per-core monoio bootstrap.
pub fn run(app: App, addr: SocketAddr) -> Result<(), Box<dyn Error>> {
    run_with_config(app, addr, ServerConfig::default())
}

/// Start the application using Harrow's thread-per-core monoio bootstrap and
/// block until shutdown.
pub fn run_with_config(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn Error>> {
    start_with_config(app, addr, config)?.wait()
}

/// Start the application using Harrow's thread-per-core monoio bootstrap and
/// return a handle for shutdown / test control.
pub fn start(app: App, addr: SocketAddr) -> Result<ServerHandle, Box<dyn Error>> {
    start_with_config(app, addr, ServerConfig::default())
}

/// Start the application using Harrow's thread-per-core monoio bootstrap and
/// return a handle for shutdown / test control.
pub fn start_with_config(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<ServerHandle, Box<dyn Error>> {
    // Fail fast on unsupported kernels.
    if let Err(err) = kernel_check::check_kernel_version() {
        return Err(Box::new(err));
    }

    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let worker_count = resolved_worker_count(config.workers)?;
    let worker_config = per_worker_config(config, worker_count);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(worker_count);

    let (completion_tx, completion_rx) = mpsc::channel();
    let first_worker = spawn_worker(
        Arc::clone(&shared),
        addr,
        worker_config,
        Arc::clone(&shutdown),
        completion_tx.clone(),
    );
    let bound_addr = match first_worker.startup.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(bound_addr)) => bound_addr,
        Ok(Err(err)) => {
            shutdown.store(true, Ordering::Release);
            let mut handle = ServerHandle {
                addr,
                shutdown,
                completion: completion_rx,
                workers: vec![first_worker.handle],
            };
            let _ = handle.join_workers();
            return Err(into_public_error(err));
        }
        Err(err) => {
            shutdown.store(true, Ordering::Release);
            let mut handle = ServerHandle {
                addr,
                shutdown,
                completion: completion_rx,
                workers: vec![first_worker.handle],
            };
            let _ = handle.join_workers();
            return Err(Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("worker startup failed before reporting a bound address: {err}"),
            )));
        }
    };
    workers.push(first_worker.handle);

    for _ in 1..worker_count {
        let worker = spawn_worker(
            Arc::clone(&shared),
            bound_addr,
            worker_config,
            Arc::clone(&shutdown),
            completion_tx.clone(),
        );
        match worker.startup.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(_)) => workers.push(worker.handle),
            Ok(Err(err)) => {
                shutdown.store(true, Ordering::Release);
                workers.push(worker.handle);
                let mut handle = ServerHandle {
                    addr: bound_addr,
                    shutdown,
                    completion: completion_rx,
                    workers,
                };
                let _ = handle.join_workers();
                return Err(into_public_error(err));
            }
            Err(err) => {
                shutdown.store(true, Ordering::Release);
                workers.push(worker.handle);
                let mut handle = ServerHandle {
                    addr: bound_addr,
                    shutdown,
                    completion: completion_rx,
                    workers,
                };
                let _ = handle.join_workers();
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("worker startup timed out: {err}"),
                )));
            }
        }
    }

    o11y::record_server_start(bound_addr, &config);

    Ok(ServerHandle {
        addr: bound_addr,
        shutdown,
        completion: completion_rx,
        workers,
    })
}

/// Serve the application on the given address using HTTP/1.1.
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
///
/// # Requirements
/// This function requires Linux kernel 6.1+ for full io_uring support.
/// It will fail fast with a clear error on older kernels.
///
/// # HTTP/2 Support
/// Set `config.enable_http2 = true` to accept HTTP/2 connections with
/// prior knowledge (direct H2 without upgrade/ALPN).
pub async fn serve_with_config(
    app: App,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()>,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if config.workers.is_some_and(|workers| workers > 1) {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ServerConfig::workers > 1 requires harrow_server_monoio::run/start; async serve_with_config runs on a single monoio runtime",
        )));
    }

    // Fail fast on unsupported kernels.
    if let Err(e) = kernel_check::check_kernel_version() {
        return Err(Box::new(e));
    }

    let shared = app.into_shared_state();

    shared.route_table.print_routes();

    let listener = TcpListener::bind_with_config(addr, &listener_options())?;
    o11y::record_server_start(addr, &config);

    serve_listener(shared, listener, shutdown, config)
        .await
        .map_err(into_public_error)
}

async fn serve_listener(
    shared: Arc<SharedState>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()>,
    config: ServerConfig,
) -> Result<(), BoxError> {
    let active_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let protocol = if config.enable_http2 {
        ProtocolVersion::Http2PriorKnowledge
    } else {
        ProtocolVersion::Http11
    };

    let mut shutdown = std::pin::pin!(shutdown);

    // Accept loop with graceful shutdown.
    loop {
        monoio::select! {
            result = listener.accept() => {
                let (stream, remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        o11y::record_accept_error(e);
                        continue;
                    }
                };

                // Disable Nagle's algorithm for lower latency.
                if let Err(e) = stream.set_nodelay(true) {
                    o11y::record_tcp_nodelay_error(e);
                }

                if active_count.get() >= config.max_connections {
                    drop(stream);
                    o11y::record_connection_limit_rejected(config.max_connections);
                    continue;
                }

                let shared = Arc::clone(&shared);
                let header_read_timeout = config.header_read_timeout;
                let body_read_timeout = config.body_read_timeout;
                let connection_timeout = config.connection_timeout;
                let max_h2_streams = config.max_h2_streams;
                let counter = Rc::clone(&active_count);

                monoio::spawn(connection::handle_connection(
                    stream,
                    connection::ConnConfig {
                        shared,
                        remote_addr: Some(remote),
                        header_read_timeout,
                        body_read_timeout,
                        connection_timeout,
                        max_h2_streams,
                        active_count: counter,
                        protocol,
                    },
                ));
            }
            () = &mut shutdown => {
                o11y::record_server_shutdown();
                break;
            }
        }
    }

    // Graceful drain: wait for in-flight connections to complete.
    let drain_start = std::time::Instant::now();
    while active_count.get() > 0 {
        if drain_start.elapsed() >= config.drain_timeout {
            o11y::record_drain_timeout(config.drain_timeout.as_secs(), active_count.get());
            break;
        }
        monoio::time::sleep(Duration::from_millis(10)).await;
    }

    o11y::record_drain_complete(active_count.get());

    Ok(())
}

async fn wait_for_shutdown(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        monoio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn listener_options() -> ListenerOpts {
    ListenerOpts::new().reuse_port(true).reuse_addr(true)
}

fn resolved_worker_count(workers: Option<usize>) -> Result<usize, Box<dyn Error>> {
    match workers {
        Some(0) => Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ServerConfig::workers must be greater than 0",
        ))),
        Some(workers) => Ok(workers),
        None => Ok(thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)),
    }
}

fn per_worker_config(config: ServerConfig, workers: usize) -> ServerConfig {
    let per_worker_max = config.max_connections.div_ceil(workers.max(1));
    ServerConfig {
        max_connections: per_worker_max.max(1),
        workers: Some(1),
        ..config
    }
}

fn into_public_error(err: BoxError) -> Box<dyn Error> {
    err
}

fn join_panic_error(panic: Box<dyn std::any::Any + Send + 'static>) -> BoxError {
    let message = if let Some(message) = panic.downcast_ref::<&str>() {
        format!("worker thread panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("worker thread panicked: {message}")
    } else {
        "worker thread panicked".to_string()
    };

    Box::new(io::Error::other(message))
}

struct WorkerThread {
    handle: thread::JoinHandle<Result<(), BoxError>>,
    startup: mpsc::Receiver<Result<SocketAddr, BoxError>>,
}

fn spawn_worker(
    shared: Arc<SharedState>,
    addr: SocketAddr,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    completion: mpsc::Sender<Result<(), String>>,
) -> WorkerThread {
    let (startup_tx, startup_rx) = mpsc::channel::<Result<SocketAddr, BoxError>>();
    let handle = thread::spawn(move || {
        let mut runtime = match monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
            .enable_timer()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                let err: BoxError = Box::new(err);
                let _ = startup_tx.send(Err(Box::new(io::Error::other(err.to_string()))));
                return Err(err);
            }
        };

        let result = runtime.block_on(async move {
            let listener = match TcpListener::bind_with_config(addr, &listener_options()) {
                Ok(listener) => listener,
                Err(err) => {
                    let err: BoxError = Box::new(err);
                    let _ = startup_tx.send(Err(Box::new(io::Error::other(err.to_string()))));
                    return Err(err);
                }
            };

            let local_addr = match listener.local_addr() {
                Ok(local_addr) => local_addr,
                Err(err) => {
                    let err: BoxError = Box::new(err);
                    let _ = startup_tx.send(Err(Box::new(io::Error::other(err.to_string()))));
                    return Err(err);
                }
            };

            let _ = startup_tx.send(Ok(local_addr));
            serve_listener(shared, listener, wait_for_shutdown(shutdown), config).await
        });

        let _ = completion.send(result.as_ref().map(|_| ()).map_err(|err| err.to_string()));
        result
    });

    WorkerThread {
        handle,
        startup: startup_rx,
    }
}
