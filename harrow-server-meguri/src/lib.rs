//! Meguri-based HTTP server for Harrow.
//!
//! Completion-driven event loop using io_uring directly.
//! No async runtime — the main loop submits SQEs and dispatches CQEs.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │              Server (N worker threads)                │
//! │  Each thread: own io_uring ring + SO_REUSEPORT       │
//! │  listener. Kernel distributes connections.           │
//! └──────────────────────┬───────────────────────────────┘
//!                        │ per thread
//!           ┌────────────┴────────────┐
//!           ▼                         ▼
//! ┌─────────────────┐     ┌─────────────────┐
//! │  Worker Thread  │     │  Worker Thread  │
//! │  submit_and_wait│     │  submit_and_wait│
//! │  poll CQEs      │     │  poll CQEs      │
//! │  dispatch CQE   │     │  dispatch CQE   │
//! │  -> Conn FSM    │     │  -> Conn FSM    │
//! └─────────────────┘     └─────────────────┘
//! ```
//!
//! # Platform
//!
//! **Linux only.** io_uring is a Linux kernel feature.

// Linux only — compile_error when io-uring feature is enabled on non-Linux.
#[cfg(all(feature = "io-uring", not(target_os = "linux")))]
compile_error!("harrow-server-meguri requires Linux. io_uring is not available on this platform.");

#[allow(unused_imports)]
use harrow_codec_h1 as codec;
// Connection FSM is platform-independent (bytes + http types only).
// The io_uring event loop below is Linux-only.
pub(crate) mod connection;

#[cfg(target_os = "linux")]
use std::error::Error;
#[cfg(target_os = "linux")]
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::{Arc, mpsc};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
type BoxError = Box<dyn Error + Send + Sync>;
#[cfg(target_os = "linux")]
use harrow_core::dispatch::{self, SharedState};
#[cfg(target_os = "linux")]
use harrow_core::route::App;
#[cfg(target_os = "linux")]
use harrow_server::h1::{EarlyResponseMode, early_response_control};

#[cfg(target_os = "linux")]
#[derive(Clone)]
/// Server configuration.
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    /// Divided equally across workers.
    pub max_connections: usize,
    /// Ring size (number of SQ/CQ entries). Default: 4096.
    pub ring_entries: u32,
    /// Timeout for reading HTTP headers. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Timeout for reading HTTP request bodies. Default: Some(30s).
    pub body_read_timeout: Option<Duration>,
    /// Maximum connection lifetime. Default: Some(5 min).
    pub connection_timeout: Option<Duration>,
    /// Drain timeout during shutdown. Default: 30s.
    pub drain_timeout: Duration,
    /// Maximum request body size. Default: 2 MiB.
    pub max_body_size: usize,
    /// Number of worker threads. Default: number of CPU cores.
    pub workers: Option<usize>,
}

#[cfg(target_os = "linux")]
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            ring_entries: 4096,
            header_read_timeout: Some(Duration::from_secs(5)),
            body_read_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(300)),
            drain_timeout: Duration::from_secs(30),
            max_body_size: 2 * 1024 * 1024,
            workers: None,
        }
    }
}

#[cfg(target_os = "linux")]
/// Special user_data value for the accept SQE.
const ACCEPT_USER_DATA: u64 = u64::MAX;

#[cfg(target_os = "linux")]
/// Special user_data value for the periodic timeout SQE.
const TIMEOUT_USER_DATA: u64 = u64::MAX - 1;

#[cfg(target_os = "linux")]
/// Special user_data value for the short dispatch wake tick.
const DISPATCH_TICK_USER_DATA: u64 = u64::MAX - 2;

#[cfg(target_os = "linux")]
/// Special user_data value for timeout cancel SQEs.
const TIMEOUT_CANCEL_USER_DATA: u64 = u64::MAX - 3;

#[cfg(target_os = "linux")]
/// Wake interval while a local dispatch task is still in flight.
const DISPATCH_TICK_MILLIS: i64 = 10;

// ---------------------------------------------------------------------------
// Public API: run / start / serve
// ---------------------------------------------------------------------------

/// Start the application using Harrow's thread-per-core meguri bootstrap.
///
/// Blocks until shutdown.
#[cfg(target_os = "linux")]
pub fn run<F>(make_app: F, addr: SocketAddr) -> Result<(), BoxError>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    run_with_config(make_app, addr, ServerConfig::default())
}

/// Start with custom config and block until shutdown.
#[cfg(target_os = "linux")]
pub fn run_with_config<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), BoxError>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    start_with_config(make_app, addr, config)?.wait()
}

/// Start the server and return a handle for graceful shutdown control.
#[cfg(target_os = "linux")]
pub fn start<F>(make_app: F, addr: SocketAddr) -> Result<ServerHandle, BoxError>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    start_with_config(make_app, addr, ServerConfig::default())
}

/// Start with custom config and return a handle.
#[cfg(target_os = "linux")]
pub fn start_with_config<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<ServerHandle, BoxError>
where
    F: Fn() -> App + Send + Clone + 'static,
{
    let worker_count = resolve_worker_count(config.workers)?;
    let per_worker_config = per_worker_config(config, worker_count);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(worker_count);

    let (completion_tx, completion_rx) = mpsc::channel();

    // Spawn first worker, wait for it to report the bound address.
    let first_worker = spawn_worker(
        make_app.clone(),
        addr,
        per_worker_config.clone(),
        Arc::clone(&shutdown),
        completion_tx.clone(),
        true,
    );
    let bound_addr = match first_worker.startup.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            shutdown.store(true, Ordering::Release);
            let mut handle = ServerHandle {
                addr,
                shutdown,
                completion: completion_rx,
                workers: vec![first_worker.handle],
            };
            let _ = handle.join_workers();
            return Err(e);
        }
        Err(e) => {
            shutdown.store(true, Ordering::Release);
            let mut handle = ServerHandle {
                addr,
                shutdown,
                completion: completion_rx,
                workers: vec![first_worker.handle],
            };
            let _ = handle.join_workers();
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("worker startup timed out: {e}"),
            )));
        }
    };
    workers.push(first_worker.handle);

    // Spawn remaining workers, all binding to the same address (SO_REUSEPORT).
    for _ in 1..worker_count {
        let worker = spawn_worker(
            make_app.clone(),
            bound_addr,
            per_worker_config.clone(),
            Arc::clone(&shutdown),
            completion_tx.clone(),
            false,
        );
        // Wait for each worker to start before spawning the next.
        match worker.startup.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(_)) => workers.push(worker.handle),
            Ok(Err(e)) => {
                shutdown.store(true, Ordering::Release);
                workers.push(worker.handle);
                let mut handle = ServerHandle {
                    addr: bound_addr,
                    shutdown,
                    completion: completion_rx,
                    workers,
                };
                let _ = handle.join_workers();
                return Err(e);
            }
            Err(e) => {
                shutdown.store(true, Ordering::Release);
                workers.push(worker.handle);
                let mut handle = ServerHandle {
                    addr: bound_addr,
                    shutdown,
                    completion: completion_rx,
                    workers,
                };
                let _ = handle.join_workers();
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("worker startup timed out: {e}"),
                )));
            }
        }
    }

    tracing::info!(
        "harrow (meguri) listening on {bound_addr} with {worker_count} workers (io_uring)"
    );

    Ok(ServerHandle {
        addr: bound_addr,
        shutdown,
        completion: completion_rx,
        workers,
    })
}

/// Convenience: single-threaded blocking serve (no multi-worker).
#[cfg(target_os = "linux")]
pub fn serve(app: App, addr: SocketAddr) -> Result<(), BoxError> {
    serve_with_config(app, addr, ServerConfig::default())
}

/// Single-threaded serve with custom config.
#[cfg(target_os = "linux")]
pub fn serve_with_config(app: App, addr: SocketAddr, config: ServerConfig) -> Result<(), BoxError> {
    let shared = app.into_shared_state();
    shared.route_table.print_routes();
    tracing::info!("harrow (meguri) listening on {addr} (single worker)");

    let listener = create_listener(addr, true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let per_worker = ServerConfig {
        workers: Some(1),
        ..config
    };

    worker_loop(shared, listener, &per_worker, shutdown)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// ServerHandle
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
/// Handle returned by `start` / `start_with_config`.
///
/// Dropping the handle signals shutdown and waits for all workers to exit.
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    completion: mpsc::Receiver<Result<(), String>>,
    workers: Vec<thread::JoinHandle<Result<(), BoxError>>>,
}

#[cfg(target_os = "linux")]
impl ServerHandle {
    /// The socket address the server bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal shutdown and wait for all workers to exit.
    pub fn shutdown(mut self) -> Result<(), BoxError> {
        self.shutdown.store(true, Ordering::Release);
        self.join_workers()
    }

    /// Block until shutdown is triggered or a worker exits with an error.
    pub fn wait(mut self) -> Result<(), BoxError> {
        let _ = self.completion.recv();
        self.shutdown.store(true, Ordering::Release);
        self.join_workers()
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

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn join_panic_error(panic: Box<dyn std::any::Any + Send + 'static>) -> BoxError {
    let message = if let Some(message) = panic.downcast_ref::<&str>() {
        format!("worker thread panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("worker thread panicked: {message}")
    } else {
        "worker thread panicked".to_string()
    };
    Box::new(std::io::Error::other(message))
}

// ---------------------------------------------------------------------------
// Worker spawning
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
struct WorkerThread {
    handle: thread::JoinHandle<Result<(), BoxError>>,
    startup: mpsc::Receiver<Result<SocketAddr, BoxError>>,
}

#[cfg(target_os = "linux")]
fn spawn_worker<F>(
    make_app: F,
    addr: SocketAddr,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    completion: mpsc::Sender<Result<(), String>>,
    print_routes: bool,
) -> WorkerThread
where
    F: Fn() -> App + Send + 'static,
{
    let (startup_tx, startup_rx) = mpsc::channel::<Result<SocketAddr, BoxError>>();

    let handle = thread::spawn(move || {
        let app = make_app();
        let shared = app.into_shared_state();
        if print_routes {
            shared.route_table.print_routes();
        }

        let listener = match create_listener(addr, true) {
            Ok(l) => l,
            Err(e) => {
                let _ = startup_tx.send(Err(Box::new(e) as BoxError));
                return Err(
                    Box::new(std::io::Error::other("failed to create listener")) as BoxError
                );
            }
        };

        let local_addr = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => {
                let _ = startup_tx.send(Err(Box::new(e) as BoxError));
                return Err(Box::new(std::io::Error::other("failed to get local addr")) as BoxError);
            }
        };

        let _ = startup_tx.send(Ok(local_addr));

        let result = worker_loop(shared, listener, &config, Arc::clone(&shutdown));

        let _ = completion.send(result.as_ref().map(|_| ()).map_err(|e| e.to_string()));
        result
    });

    WorkerThread {
        handle,
        startup: startup_rx,
    }
}

// ---------------------------------------------------------------------------
// Worker event loop (the core)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn worker_loop(
    shared: Arc<SharedState>,
    listener: std::net::TcpListener,
    config: &ServerConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<(), BoxError> {
    let mut ring = meguri::Ring::new(config.ring_entries)?;
    tracing::debug!(
        "meguri worker ring created with {} entries",
        config.ring_entries
    );

    let mut accept_addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut accept_addrlen: libc::socklen_t = std::mem::size_of::<libc::sockaddr_storage>() as _;

    let mut conns: slab::Slab<connection::Conn> = slab::Slab::new();

    // Embed a tokio current-thread runtime for async dispatch.
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();

    let listener_fd = listener.as_raw_fd();

    // Timeout specs must live on the stack across submit_and_wait since the
    // kernel reads them asynchronously.
    let mut timeout_ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let dispatch_tick_ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: DISPATCH_TICK_MILLIS * 1_000_000,
    };
    let mut dispatch_tick_armed = false;
    let mut timeout_armed = false;
    let mut timeout_deadline = None;

    // Submit initial accept SQE.
    submit_accept(
        &mut ring,
        listener_fd,
        &mut accept_addr,
        &mut accept_addrlen,
    );

    loop {
        // Check shutdown flag.
        if shutdown.load(Ordering::Acquire) {
            tracing::debug!("worker shutting down");
            break;
        }

        schedule_next_timeout(
            &mut ring,
            &conns,
            config,
            &mut timeout_armed,
            &mut timeout_deadline,
            &mut timeout_ts,
        );

        if dispatch_tick_needed(&conns) && !dispatch_tick_armed {
            submit_dispatch_tick(&mut ring, &dispatch_tick_ts);
            dispatch_tick_armed = true;
        }

        // Submit pending SQEs and wait for at least one completion.
        ring.submit_and_wait(1)?;

        // Drain all available completions.
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;

            if user_data == ACCEPT_USER_DATA {
                handle_accept(res, &mut ring, &mut conns, config);
                // Re-arm accept only if not shutting down.
                if !shutdown.load(Ordering::Acquire) {
                    submit_accept(
                        &mut ring,
                        listener_fd,
                        &mut accept_addr,
                        &mut accept_addrlen,
                    );
                }
            } else if user_data == TIMEOUT_USER_DATA {
                timeout_armed = false;
                timeout_deadline = None;
                sweep_timed_out_connections(&mut conns, config);
            } else if user_data == TIMEOUT_CANCEL_USER_DATA {
                // Old timeout canceled so a new nearest-deadline timeout can replace it.
            } else if user_data == DISPATCH_TICK_USER_DATA {
                dispatch_tick_armed = false;
            } else {
                handle_conn_completion(
                    user_data, res, &mut ring, &mut conns, &shared, &local, config,
                );
            }

            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();

        drive_dispatch_runtime(&tokio_rt, &local);
        advance_dispatching_connections(&mut ring, &mut conns, &shared, &local, config);
        advance_response_streaming_connections(&mut ring, &mut conns, &shared, &local, config);
    }

    // Drain phase: wait for in-flight connections.
    let drain_start = std::time::Instant::now();
    while !conns.is_empty() && drain_start.elapsed() < config.drain_timeout {
        schedule_next_timeout(
            &mut ring,
            &conns,
            config,
            &mut timeout_armed,
            &mut timeout_deadline,
            &mut timeout_ts,
        );
        if dispatch_tick_needed(&conns) && !dispatch_tick_armed {
            submit_dispatch_tick(&mut ring, &dispatch_tick_ts);
            dispatch_tick_armed = true;
        }
        ring.submit_and_wait(1)?;
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;
            if user_data == DISPATCH_TICK_USER_DATA {
                dispatch_tick_armed = false;
            } else if user_data == TIMEOUT_USER_DATA {
                timeout_armed = false;
                timeout_deadline = None;
                sweep_timed_out_connections(&mut conns, config);
            } else if user_data == TIMEOUT_CANCEL_USER_DATA {
            } else if user_data != ACCEPT_USER_DATA && user_data != TIMEOUT_USER_DATA {
                handle_conn_completion(
                    user_data, res, &mut ring, &mut conns, &shared, &local, config,
                );
            }
            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();
        drive_dispatch_runtime(&tokio_rt, &local);
        advance_dispatching_connections(&mut ring, &mut conns, &shared, &local, config);
        advance_response_streaming_connections(&mut ring, &mut conns, &shared, &local, config);
    }

    if !conns.is_empty() {
        tracing::warn!("drain timeout, {} connections aborted", conns.len());
        // Close remaining connections.
        for conn in conns.drain() {
            unsafe {
                libc::close(conn.fd);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn resolve_worker_count(workers: Option<usize>) -> Result<usize, BoxError> {
    match workers {
        Some(0) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ServerConfig::workers must be greater than 0",
        ))),
        Some(n) => Ok(n),
        None => Ok(thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)),
    }
}

#[cfg(target_os = "linux")]
fn per_worker_config(config: ServerConfig, workers: usize) -> ServerConfig {
    let per_worker_max = config.max_connections.div_ceil(workers.max(1));
    ServerConfig {
        max_connections: per_worker_max.max(1),
        workers: Some(1),
        ..config
    }
}

#[cfg(target_os = "linux")]
fn create_listener(addr: SocketAddr, reuse_port: bool) -> std::io::Result<std::net::TcpListener> {
    use std::os::fd::FromRawFd;

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };

    // Create socket manually so we can set options BEFORE bind.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    unsafe {
        let optval = 1i32;
        let optlen = std::mem::size_of_val(&optval) as libc::socklen_t;

        // SO_REUSEADDR — allow quick restarts.
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const _,
            optlen,
        );

        // SO_REUSEPORT — must be set before bind for kernel connection
        // distribution across multiple workers.
        if reuse_port {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const _,
                optlen,
            );
        }

        // Bind. Keep sockaddr alive across the bind call.
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let sa_len: libc::socklen_t = match addr {
            SocketAddr::V4(ref a) => {
                let sin =
                    &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = a.port().to_be();
                sin.sin_addr = libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                };
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            SocketAddr::V6(ref a) => {
                let sin6 =
                    &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6);
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_flowinfo = a.flowinfo();
                sin6.sin6_addr = libc::in6_addr {
                    s6_addr: a.ip().octets(),
                };
                sin6.sin6_scope_id = a.scope_id();
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
            }
        };

        if libc::bind(
            fd,
            &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
            sa_len,
        ) < 0
        {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        // Listen with a reasonable backlog.
        if libc::listen(fd, 1024) < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        // Set non-blocking after listen.
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
    }

    // Convert to std TcpListener for local_addr() etc.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    Ok(listener)
}

// ---------------------------------------------------------------------------
// SQE submission helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn submit_accept(
    ring: &mut meguri::Ring,
    listener_fd: std::os::fd::RawFd,
    addr: &mut libc::sockaddr_storage,
    addrlen: &mut libc::socklen_t,
) {
    ring.sq().push_accept(
        ACCEPT_USER_DATA,
        listener_fd,
        addr as *mut _ as *mut libc::sockaddr,
        addrlen as *mut _,
        0,
    );
}

#[cfg(target_os = "linux")]
fn handle_accept(
    res: i32,
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    config: &ServerConfig,
) {
    if res < 0 {
        let err = std::io::Error::from_raw_os_error(-res);
        tracing::error!("accept error: {err}");
        return;
    }

    let fd = res;

    if conns.len() >= config.max_connections {
        tracing::warn!("connection limit reached, dropping new connection");
        unsafe {
            libc::close(fd);
        }
        return;
    }

    // Set non-blocking.
    unsafe {
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
    }

    // Disable Nagle's algorithm.
    let nodelay = 1i32;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &nodelay as *const _ as *const _,
            std::mem::size_of_val(&nodelay) as _,
        );
    }

    let conn = connection::Conn::new(fd);
    let conn_idx = conns.insert(conn);
    submit_recv_or_close(ring, conn_idx, conns);
}

#[cfg(target_os = "linux")]
fn submit_recv(ring: &mut meguri::Ring, conn_idx: usize, conn: &mut connection::Conn) -> bool {
    // Ensure spare capacity for the kernel to write into.
    let spare = conn.buf.capacity() - conn.buf.len();
    if spare < codec::DEFAULT_BUFFER_SIZE {
        conn.buf.reserve(codec::DEFAULT_BUFFER_SIZE - spare);
    }

    let start = conn.buf.len();
    let avail = conn.buf.capacity() - start;
    let buf_ptr = conn.buf.as_mut_ptr();

    if !ring.sq().push_recv(
        conn_idx as u64,
        conn.fd,
        unsafe { buf_ptr.add(start) },
        avail as u32,
        0,
    ) {
        tracing::error!("SQ full, dropping connection fd {}", conn.fd);
        return false;
    }

    conn.recv_pending = true;
    true
}

#[cfg(target_os = "linux")]
fn handle_conn_completion(
    user_data: u64,
    res: i32,
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    let conn_idx = user_data as usize;

    if !conns.contains(conn_idx) {
        return; // Connection already closed.
    }

    let conn = &mut conns[conn_idx];

    if conn.recv_pending {
        conn.recv_pending = false;

        if res < 0 {
            tracing::debug!(
                "recv error on fd {}: {}",
                conn.fd,
                std::io::Error::from_raw_os_error(-res)
            );
            close_conn(conn_idx, conns);
            return;
        }

        // Advance the buffer length to include the bytes the kernel wrote.
        // Clamp to spare capacity as a safety invariant — the kernel should
        // never return more than we requested, but trusting that blindly in
        // an unsafe set_len would risk reading uninitialised memory.
        let nbytes = (res as usize).min(conn.buf.capacity() - conn.buf.len());
        unsafe {
            conn.buf.set_len(conn.buf.len() + nbytes);
        }

        if matches!(conns[conn_idx].state, connection::ConnState::Headers) {
            let result = conns[conn_idx].on_recv(nbytes, config.max_body_size);
            handle_process_result(ring, conns, conn_idx, result, shared, local, config);
        } else if matches!(conns[conn_idx].state, connection::ConnState::Dispatching) {
            advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
        }
    } else if conn.write_pending {
        conn.write_pending = false;

        if res < 0 {
            tracing::debug!(
                "write error on fd {}: {}",
                conn.fd,
                std::io::Error::from_raw_os_error(-res)
            );
            close_conn(conn_idx, conns);
            return;
        }

        let nbytes = res as usize;
        let result = conn.on_write(nbytes);
        handle_write_result(ring, conns, conn_idx, result, shared, local, config);
    }
}

#[cfg(target_os = "linux")]
fn submit_write(ring: &mut meguri::Ring, conn_idx: usize, conn: &mut connection::Conn) -> bool {
    let remaining = &conn.response_buf[conn.response_written..];
    if remaining.is_empty() {
        return true;
    }

    if !ring.sq().push_send(
        conn_idx as u64,
        conn.fd,
        remaining.as_ptr(),
        remaining.len() as u32,
        0,
    ) {
        tracing::error!("SQ full, dropping connection fd {}", conn.fd);
        return false;
    }

    conn.write_pending = true;
    true
}

#[cfg(target_os = "linux")]
fn submit_timeout_cancel(ring: &mut meguri::Ring) {
    let _ = ring
        .sq()
        .push_cancel(TIMEOUT_CANCEL_USER_DATA, TIMEOUT_USER_DATA);
}

#[cfg(target_os = "linux")]
fn submit_dispatch_tick(ring: &mut meguri::Ring, ts: &libc::timespec) {
    ring.sq()
        .push_timeout(DISPATCH_TICK_USER_DATA, ts as *const _, 0, 0);
}

#[cfg(target_os = "linux")]
fn schedule_next_timeout(
    ring: &mut meguri::Ring,
    conns: &slab::Slab<connection::Conn>,
    config: &ServerConfig,
    timeout_armed: &mut bool,
    timeout_deadline: &mut Option<std::time::Instant>,
    timeout_ts: &mut libc::timespec,
) {
    let next_deadline = next_timeout_deadline(conns, config);
    if next_deadline == *timeout_deadline {
        return;
    }

    if *timeout_armed {
        submit_timeout_cancel(ring);
        *timeout_armed = false;
    }

    *timeout_deadline = next_deadline;

    let Some(deadline) = next_deadline else {
        return;
    };

    let delay = deadline.saturating_duration_since(std::time::Instant::now());
    *timeout_ts = duration_to_timespec(delay);
    if ring
        .sq()
        .push_timeout(TIMEOUT_USER_DATA, timeout_ts as *const _, 0, 0)
    {
        *timeout_armed = true;
    }
}

#[cfg(target_os = "linux")]
fn next_timeout_deadline(
    conns: &slab::Slab<connection::Conn>,
    config: &ServerConfig,
) -> Option<std::time::Instant> {
    conns
        .iter()
        .filter(|(_, conn)| !matches!(conn.state, connection::ConnState::Closed))
        .filter_map(|(_, conn)| connection_timeout_deadline(conn, config))
        .min()
}

#[cfg(target_os = "linux")]
fn connection_timeout_deadline(
    conn: &connection::Conn,
    config: &ServerConfig,
) -> Option<std::time::Instant> {
    let lifetime_deadline = config
        .connection_timeout
        .map(|timeout| conn.accepted_at + timeout);
    let phase_deadline = match conn.state {
        connection::ConnState::Headers => config
            .header_read_timeout
            .map(|timeout| conn.request_started_at + timeout),
        connection::ConnState::Dispatching if conn.request_body_in_progress() => config
            .body_read_timeout
            .map(|timeout| conn.request_started_at + timeout),
        _ => None,
    };

    match (lifetime_deadline, phase_deadline) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[cfg(target_os = "linux")]
fn duration_to_timespec(duration: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: duration.as_secs() as i64,
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// Sweep connections that have exceeded their timeouts.
#[cfg(target_os = "linux")]
fn sweep_timed_out_connections(conns: &mut slab::Slab<connection::Conn>, config: &ServerConfig) {
    let expired: Vec<usize> = conns
        .iter()
        .filter(|(_, c)| {
            !matches!(c.state, connection::ConnState::Closed)
                && (c.is_expired(config.connection_timeout)
                    || c.read_timed_out_for_phase(
                        config.header_read_timeout,
                        config.body_read_timeout,
                    ))
        })
        .map(|(i, _)| i)
        .collect();

    for idx in expired {
        let conn = &mut conns[idx];
        tracing::debug!("closing timed-out connection fd={}", conn.fd);

        if conn.recv_pending || conn.write_pending {
            // I/O is in flight — close the fd so the kernel cancels the
            // pending operation, but keep the slab entry alive until the
            // CQE arrives.  The CQE handler will see the error and call
            // close_conn to release the slot.
            unsafe {
                libc::close(conn.fd);
            }
            conn.fd = -1;
            conn.state = connection::ConnState::Closed;
        } else {
            close_conn(idx, conns);
        }
    }
}

/// Send HTTP/1.1 100 Continue interim response.
///
/// This is a synchronous (non-SQE) send because the client is blocking on it
/// before sending the body.  The buffer is tiny (25 bytes) so a single
/// `send(2)` with `MSG_DONTWAIT` will never block on a connected socket.
#[cfg(target_os = "linux")]
fn submit_continue(_ring: &mut meguri::Ring, _conn_idx: usize, conn: &mut connection::Conn) {
    unsafe {
        libc::send(
            conn.fd,
            codec::CONTINUE_100.as_ptr() as *const _,
            codec::CONTINUE_100.len(),
            libc::MSG_DONTWAIT,
        );
    }
}

#[cfg(target_os = "linux")]
fn close_conn(conn_idx: usize, conns: &mut slab::Slab<connection::Conn>) {
    let conn = conns.remove(conn_idx);
    if conn.fd >= 0 {
        unsafe {
            libc::close(conn.fd);
        }
    }
}

/// Submit a RECV or close the connection on SQ full.
#[cfg(target_os = "linux")]
fn submit_recv_or_close(
    ring: &mut meguri::Ring,
    conn_idx: usize,
    conns: &mut slab::Slab<connection::Conn>,
) {
    if !submit_recv(ring, conn_idx, &mut conns[conn_idx]) {
        close_conn(conn_idx, conns);
    }
}

/// Submit a WRITE or close the connection on SQ full.
#[cfg(target_os = "linux")]
fn submit_write_or_close(
    ring: &mut meguri::Ring,
    conn_idx: usize,
    conns: &mut slab::Slab<connection::Conn>,
) {
    if !submit_write(ring, conn_idx, &mut conns[conn_idx]) {
        close_conn(conn_idx, conns);
    }
}

#[cfg(target_os = "linux")]
fn dispatch_tick_needed(conns: &slab::Slab<connection::Conn>) -> bool {
    conns.iter().any(|(_, conn)| {
        conn.has_active_dispatch()
            || conn.has_active_response_stream()
            || matches!(conn.state, connection::ConnState::Dispatching)
    })
}

#[cfg(target_os = "linux")]
fn drive_dispatch_runtime(rt: &tokio::runtime::Runtime, local: &tokio::task::LocalSet) {
    rt.block_on(local.run_until(async {
        tokio::task::yield_now().await;
    }));
}

#[cfg(target_os = "linux")]
fn spawn_dispatch_task(
    conn_idx: usize,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
) -> bool {
    let Some(request) = conns[conn_idx].build_harrow_request() else {
        tracing::error!("failed to build request for fd {}", conns[conn_idx].fd);
        return false;
    };

    let shared = Arc::clone(shared);
    let (tx, handle) = connection::dispatch_slot();
    conns[conn_idx].set_dispatch_handle(handle);

    local.spawn_local(async move {
        let response = dispatch::dispatch(shared, request).await;
        tx.send(Ok(response));
    });

    true
}

#[cfg(target_os = "linux")]
fn start_response_stream_task(
    conn_idx: usize,
    conns: &mut slab::Slab<connection::Conn>,
    local: &tokio::task::LocalSet,
    response: http::Response<harrow_core::response::ResponseBody>,
) {
    let keep_alive = conns[conn_idx].keep_alive;
    let is_head_request = conns[conn_idx]
        .parsed
        .as_ref()
        .is_some_and(|parsed| parsed.method == http::Method::HEAD);
    let (tx, rx) = connection::response_channel();
    conns[conn_idx].set_response_receiver(rx);

    local.spawn_local(async move {
        connection::stream_response(tx, response, keep_alive, is_head_request).await;
    });
}

#[cfg(target_os = "linux")]
fn handle_process_result(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    conn_idx: usize,
    process_result: connection::ProcessResult,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    match process_result {
        connection::ProcessResult::NeedRecv => {
            submit_recv_or_close(ring, conn_idx, conns);
        }
        connection::ProcessResult::Dispatch => {
            if conns[conn_idx]
                .parsed
                .as_ref()
                .is_some_and(|p| p.expect_continue)
            {
                submit_continue(ring, conn_idx, &mut conns[conn_idx]);
            }

            if !spawn_dispatch_task(conn_idx, conns, shared, local) {
                close_conn(conn_idx, conns);
                return;
            }

            advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
        }
        connection::ProcessResult::WriteError(resp) => {
            conns[conn_idx].set_serialized_response(resp, false);
            conns[conn_idx].buf.clear();
            if !submit_write(ring, conn_idx, &mut conns[conn_idx]) {
                close_conn(conn_idx, conns);
            }
        }
        connection::ProcessResult::Close => {
            close_conn(conn_idx, conns);
        }
    }
}

#[cfg(target_os = "linux")]
fn handle_write_result(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    conn_idx: usize,
    result: connection::WriteResult,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    match result {
        connection::WriteResult::WriteMore => {
            submit_write_or_close(ring, conn_idx, conns);
        }
        connection::WriteResult::AwaitResponse => {
            advance_response_stream(ring, conns, conn_idx, shared, local, config);
        }
        connection::WriteResult::RecvNext => {
            let process_result = {
                let conn = &mut conns[conn_idx];
                conn.resume_after_keep_alive(config.max_body_size)
            };
            handle_process_result(ring, conns, conn_idx, process_result, shared, local, config);
        }
        connection::WriteResult::Close => {
            close_conn(conn_idx, conns);
        }
    }
}

#[cfg(target_os = "linux")]
fn advance_response_stream(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    conn_idx: usize,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    if !conns.contains(conn_idx) || conns[conn_idx].write_pending {
        return;
    }

    match conns[conn_idx].poll_response_stream() {
        connection::ResponseProgress::Pending => {}
        connection::ResponseProgress::WriteReady => {
            submit_write_or_close(ring, conn_idx, conns);
        }
        connection::ResponseProgress::Complete => {
            let result = conns[conn_idx].on_write(0);
            handle_write_result(ring, conns, conn_idx, result, shared, local, config);
        }
        connection::ResponseProgress::StartError => {
            conns[conn_idx].set_internal_server_error();
            submit_write_or_close(ring, conn_idx, conns);
        }
        connection::ResponseProgress::StreamError => {
            close_conn(conn_idx, conns);
        }
    }
}

#[cfg(target_os = "linux")]
fn advance_dispatching_connections(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    let dispatching: Vec<usize> = conns
        .iter()
        .filter(|(_, conn)| matches!(conn.state, connection::ConnState::Dispatching))
        .map(|(idx, _)| idx)
        .collect();

    for conn_idx in dispatching {
        if conns.contains(conn_idx) {
            advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
        }
    }
}

#[cfg(target_os = "linux")]
fn advance_response_streaming_connections(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    let writing: Vec<usize> = conns
        .iter()
        .filter(|(_, conn)| {
            matches!(conn.state, connection::ConnState::Writing)
                && conn.has_active_response_stream()
                && !conn.write_pending
        })
        .map(|(idx, _)| idx)
        .collect();

    for conn_idx in writing {
        if conns.contains(conn_idx) {
            advance_response_stream(ring, conns, conn_idx, shared, local, config);
        }
    }
}

#[cfg(target_os = "linux")]
fn advance_dispatching_connection(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    conn_idx: usize,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    if !conns.contains(conn_idx)
        || !matches!(conns[conn_idx].state, connection::ConnState::Dispatching)
    {
        return;
    }

    if let Some(dispatch_result) = conns[conn_idx].poll_dispatch_result() {
        match dispatch_result {
            Ok(response) => {
                if conns[conn_idx].request_body_in_progress() {
                    let control = early_response_control(EarlyResponseMode::DropRequestBody);
                    conns[conn_idx].keep_alive = control.keep_alive;
                    conns[conn_idx].abort_request_body();
                }
                start_response_stream_task(conn_idx, conns, local, response);
                advance_response_stream(ring, conns, conn_idx, shared, local, config);
            }
            Err(()) => {
                conns[conn_idx].set_internal_server_error();
                submit_write_or_close(ring, conn_idx, conns);
            }
        }
    }

    advance_response_stream(ring, conns, conn_idx, shared, local, config);

    if !conns.contains(conn_idx)
        || !matches!(conns[conn_idx].state, connection::ConnState::Dispatching)
    {
        return;
    }

    if conns[conn_idx].recv_pending || conns[conn_idx].write_pending {
        return;
    }

    if !conns[conn_idx].request_body_in_progress() {
        return;
    }

    match conns[conn_idx].pump_request_body(config.max_body_size) {
        connection::BodyPumpResult::NeedRecv => {
            submit_recv_or_close(ring, conn_idx, conns);
        }
        connection::BodyPumpResult::Blocked | connection::BodyPumpResult::Eof => {}
        connection::BodyPumpResult::ReceiverClosed => {
            let control = early_response_control(EarlyResponseMode::DropRequestBody);
            let conn = &mut conns[conn_idx];
            conn.keep_alive = control.keep_alive;
            conn.abort_request_body();
        }
        connection::BodyPumpResult::ResponseError(error) => {
            conns[conn_idx].set_error_response(error);
            submit_write_or_close(ring, conn_idx, conns);
        }
    }
}
