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

#[cfg(target_os = "linux")]
mod codec;
#[cfg(target_os = "linux")]
mod connection;

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
/// Interval for the periodic timeout sweep (1 second).
const TIMEOUT_SWEEP_SECS: i64 = 1;

// ---------------------------------------------------------------------------
// Public API: run / start / serve
// ---------------------------------------------------------------------------

/// Start the application using Harrow's thread-per-core meguri bootstrap.
///
/// Blocks until shutdown.
#[cfg(target_os = "linux")]
pub fn run(app: App, addr: SocketAddr) -> Result<(), BoxError> {
    run_with_config(app, addr, ServerConfig::default())
}

/// Start with custom config and block until shutdown.
#[cfg(target_os = "linux")]
pub fn run_with_config(app: App, addr: SocketAddr, config: ServerConfig) -> Result<(), BoxError> {
    start_with_config(app, addr, config)?.wait()
}

/// Start the server and return a handle for graceful shutdown control.
#[cfg(target_os = "linux")]
pub fn start(app: App, addr: SocketAddr) -> Result<ServerHandle, BoxError> {
    start_with_config(app, addr, ServerConfig::default())
}

/// Start with custom config and return a handle.
#[cfg(target_os = "linux")]
pub fn start_with_config(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<ServerHandle, BoxError> {
    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let worker_count = resolve_worker_count(config.workers)?;
    let per_worker_config = per_worker_config(config, worker_count);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(worker_count);

    let (completion_tx, completion_rx) = mpsc::channel();

    // Spawn first worker, wait for it to report the bound address.
    let first_worker = spawn_worker(
        Arc::clone(&shared),
        addr,
        per_worker_config.clone(),
        Arc::clone(&shutdown),
        completion_tx.clone(),
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
            Arc::clone(&shared),
            bound_addr,
            per_worker_config.clone(),
            Arc::clone(&shutdown),
            completion_tx.clone(),
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
fn spawn_worker(
    shared: Arc<SharedState>,
    addr: SocketAddr,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    completion: mpsc::Sender<Result<(), String>>,
) -> WorkerThread {
    let (startup_tx, startup_rx) = mpsc::channel::<Result<SocketAddr, BoxError>>();

    let handle = thread::spawn(move || {
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

    let listener_fd = listener.as_raw_fd();

    // Timeout spec for periodic sweeps. Must live on the stack across
    // submit_and_wait since the kernel reads it asynchronously.
    let sweep_ts = libc::timespec {
        tv_sec: TIMEOUT_SWEEP_SECS,
        tv_nsec: 0,
    };

    // Submit initial accept and periodic timeout SQEs.
    submit_accept(
        &mut ring,
        listener_fd,
        &mut accept_addr,
        &mut accept_addrlen,
    );
    submit_timeout(&mut ring, &sweep_ts);

    loop {
        // Check shutdown flag.
        if shutdown.load(Ordering::Acquire) {
            tracing::debug!("worker shutting down");
            break;
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
                // Periodic timeout fired — sweep timed-out connections.
                sweep_timed_out_connections(&mut conns, config);
                // Re-arm.
                submit_timeout(&mut ring, &sweep_ts);
            } else {
                handle_conn_completion(
                    user_data, res, &mut ring, &mut conns, &shared, &tokio_rt, config,
                );
            }

            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();
    }

    // Drain phase: wait for in-flight connections.
    let drain_start = std::time::Instant::now();
    while !conns.is_empty() && drain_start.elapsed() < config.drain_timeout {
        ring.submit_and_wait(1)?;
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;
            if user_data != ACCEPT_USER_DATA && user_data != TIMEOUT_USER_DATA {
                handle_conn_completion(
                    user_data, res, &mut ring, &mut conns, &shared, &tokio_rt, config,
                );
            }
            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();
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
    tokio_rt: &tokio::runtime::Runtime,
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

        let result = conn.on_recv(nbytes, config.max_body_size);

        match result {
            connection::ProcessResult::NeedRecv => {
                if !submit_recv(ring, conn_idx, &mut conns[conn_idx]) {
                    close_conn(conn_idx, conns);
                }
            }
            connection::ProcessResult::Dispatch => {
                // Send 100-continue if the client expects it.
                if conns[conn_idx]
                    .parsed
                    .as_ref()
                    .is_some_and(|p| p.expect_continue)
                {
                    submit_continue(ring, conn_idx, &mut conns[conn_idx]);
                }

                let conn = &mut conns[conn_idx];
                if let Some(req) = conn.build_harrow_request() {
                    let shared = Arc::clone(shared);
                    let (parts, body_data) = tokio_rt.block_on(async {
                        use http_body_util::BodyExt;
                        let resp = dispatch::dispatch(shared, req).await;
                        let (parts, body) = resp.into_parts();
                        let collected = body.collect().await;
                        let body_data = collected
                            .map(|c| c.to_bytes())
                            .map_err(|e| e as Box<dyn std::error::Error + Send + Sync>);
                        (parts, body_data)
                    });
                    conn.set_response(parts, body_data);
                    submit_write_or_close(ring, conn_idx, conns);
                } else {
                    tracing::error!("failed to build request for fd {}", conn.fd);
                    close_conn(conn_idx, conns);
                }
            }
            connection::ProcessResult::WriteError(resp) => {
                let conn = &mut conns[conn_idx];
                conn.response_buf = resp;
                conn.response_written = 0;
                conn.keep_alive = false;
                conn.buf.clear();
                conn.state = connection::ConnState::Writing;
                if !submit_write(ring, conn_idx, &mut conns[conn_idx]) {
                    close_conn(conn_idx, conns);
                }
            }
            connection::ProcessResult::Close => {
                close_conn(conn_idx, conns);
            }
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

        match result {
            connection::WriteResult::WriteMore => {
                if !submit_write(ring, conn_idx, &mut conns[conn_idx]) {
                    close_conn(conn_idx, conns);
                }
            }
            connection::WriteResult::RecvNext => {
                if !submit_recv(ring, conn_idx, &mut conns[conn_idx]) {
                    close_conn(conn_idx, conns);
                }
            }
            connection::WriteResult::Close => {
                close_conn(conn_idx, conns);
            }
        }
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
fn submit_timeout(ring: &mut meguri::Ring, ts: &libc::timespec) {
    ring.sq()
        .push_timeout(TIMEOUT_USER_DATA, ts as *const _, 0, 0);
}

/// Sweep connections that have exceeded their timeouts.
#[cfg(target_os = "linux")]
fn sweep_timed_out_connections(conns: &mut slab::Slab<connection::Conn>, config: &ServerConfig) {
    let expired: Vec<usize> = conns
        .iter()
        .filter(|(_, c)| {
            !matches!(c.state, connection::ConnState::Closed)
                && (c.is_expired(config.connection_timeout)
                    || c.read_timed_out(config.header_read_timeout))
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
    submit_recv_or_close(ring, conn_idx, conns);
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
