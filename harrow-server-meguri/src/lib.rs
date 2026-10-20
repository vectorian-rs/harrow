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
//! # Ownership and lifetime rules
//!
//! The event loop talks to the kernel through raw pointers and raw fds, so
//! these rules carry the memory safety of the backend:
//!
//! - **Listener fd**: owned by the worker's `TcpListener`, which outlives
//!   the ring. The ACCEPT SQE passes no address buffer, so it references no
//!   user memory.
//! - **Accepted fds**: the fd in a successful ACCEPT CQE is owned by the
//!   `Conn` created in `handle_accept`, or closed right away
//!   (`close_accepted_fd`) when no connection takes it (limit reached,
//!   drain, teardown). It is created close-on-exec.
//! - **Connection fds**: `Conn::fd` is closed in exactly one place,
//!   `close_conn`, which sets it to `-1`. With a RECV/SEND in flight it
//!   first calls shutdown(2): io_uring holds its own file reference, so
//!   close(2) alone would neither send FIN nor complete the SQE.
//! - **RECV buffer**: a RECV SQE points into the spare capacity of
//!   `Conn::buf`. While `recv_pending` is set, `buf` is not reallocated
//!   (`Conn::prepare_recv`/`commit_recv` record and verify the target) and
//!   the `Conn` stays in the slab.
//! - **SEND buffer**: a SEND SQE points into `Conn::response_buf`. While
//!   `write_pending` is set, `response_buf` is not modified or replaced
//!   (debug-asserted) and the `Conn` stays in the slab.
//! - **Slab slots**: a closed connection with an SQE in flight stays in its
//!   slot in the `Closed` state; its last CQE frees the slot
//!   (`release_if_idle`). At worker exit, `teardown` waits for those CQEs
//!   and leaks (never frees) anything the kernel still holds.
//! - **Stale CQEs**: every per-connection user_data carries the slab index
//!   and a 28-bit generation (`ConnToken`). A CQE whose generation does not
//!   match the slot's current connection is ignored.
//! - **Timespecs**: TIMEOUT SQEs point at `static`s or at a worker-loop
//!   local declared before the ring; the kernel copies them at submission.
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
use std::cmp::Reverse;
#[cfg(target_os = "linux")]
use std::collections::BinaryHeap;
#[cfg(target_os = "linux")]
use std::error::Error;
#[cfg(target_os = "linux")]
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
type BoxError = Box<dyn Error + Send + Sync>;
#[cfg(target_os = "linux")]
use harrow_core::dispatch::{self, SharedState};
#[cfg(target_os = "linux")]
use harrow_core::route::App;
#[cfg(target_os = "linux")]
use harrow_server::h1::{EarlyResponseMode, ErrorResponse, early_response_control};
#[cfg(target_os = "linux")]
use harrow_server::h1_lifecycle::Event;

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
/// Special user_data value for the cancel of the accept SQE at teardown.
const ACCEPT_CANCEL_USER_DATA: u64 = u64::MAX - 4;

#[cfg(target_os = "linux")]
/// Special user_data value for the timer that bounds teardown.
const TEARDOWN_TIMER_USER_DATA: u64 = u64::MAX - 5;

// Per-connection user_data layout:
//
//   bit 63      always 0, so it never collides with the `u64::MAX - n` specials
//   bits 60..63 operation tag (exactly one of the flags below)
//   bits 32..60 connection generation (28 bits)
//   bits  0..32 slab index
//
// The generation makes a CQE that outlives its connection (a late RECV CQE,
// or the CQE of a cancel SQE) unable to act on a newer connection that
// reuses the same slab slot.

#[cfg(target_os = "linux")]
/// Tag used for RECV-cancel SQEs when a body-phase timeout must become a 408.
const RECV_CANCEL_USER_DATA_FLAG: u64 = 1 << 62;

#[cfg(target_os = "linux")]
/// Tag for normal RECV completions.
const RECV_USER_DATA_FLAG: u64 = 1 << 61;

#[cfg(target_os = "linux")]
/// Tag for normal WRITE completions.
const WRITE_USER_DATA_FLAG: u64 = 1 << 60;

#[cfg(target_os = "linux")]
const USER_DATA_TAG_MASK: u64 = 0b111 << 60;

#[cfg(target_os = "linux")]
const CONN_GENERATION_MASK: u32 = (1 << 28) - 1;

#[cfg(target_os = "linux")]
/// Identifies one connection: its slab slot plus the generation of the
/// connection that occupied the slot when the SQE was submitted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ConnToken {
    idx: usize,
    generation: u32,
}

#[cfg(target_os = "linux")]
impl ConnToken {
    fn of(idx: usize, conn: &connection::Conn) -> Self {
        Self {
            idx,
            generation: conn.generation,
        }
    }

    fn user_data(self, tag: u64) -> u64 {
        debug_assert!(
            u32::try_from(self.idx).is_ok(),
            "slab index exceeds 32 bits"
        );
        debug_assert_eq!(self.generation & !CONN_GENERATION_MASK, 0);
        tag | (u64::from(self.generation & CONN_GENERATION_MASK) << 32)
            | (self.idx as u64 & 0xFFFF_FFFF)
    }

    fn decode(user_data: u64, tag: u64) -> Option<Self> {
        (user_data >> 63 == 0 && user_data & USER_DATA_TAG_MASK == tag).then_some(Self {
            idx: (user_data & 0xFFFF_FFFF) as usize,
            generation: ((user_data >> 32) as u32) & CONN_GENERATION_MASK,
        })
    }

    /// The connection this token names, if it is still in its slot.
    fn get(self, conns: &mut slab::Slab<connection::Conn>) -> Option<&mut connection::Conn> {
        conns
            .get_mut(self.idx)
            .filter(|conn| conn.generation == self.generation)
    }
}

#[cfg(target_os = "linux")]
/// Wake interval while a local dispatch task is still in flight.
const DISPATCH_TICK_MILLIS: i64 = 10;

#[cfg(target_os = "linux")]
/// The dispatch tick's duration. A `static`, so the pointer in its SQE is
/// valid for the life of the process.
static DISPATCH_TICK_TS: libc::timespec = libc::timespec {
    tv_sec: 0,
    tv_nsec: DISPATCH_TICK_MILLIS * 1_000_000,
};

#[cfg(target_os = "linux")]
/// The teardown timer's duration (`TEARDOWN_TIMEOUT`), static for the same
/// reason.
static TEARDOWN_TS: libc::timespec = libc::timespec {
    tv_sec: TEARDOWN_TIMEOUT.as_secs() as libc::time_t,
    tv_nsec: 0,
};

#[cfg(target_os = "linux")]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DeadlineEntry {
    deadline: std::time::Instant,
    conn_idx: usize,
    generation: u64,
}

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
    let inner = harrow_server::start_workers(
        addr,
        worker_count,
        wake_workers as fn(SocketAddr, usize),
        move |ctx| run_worker(&make_app, &per_worker_config, ctx),
    )?;

    tracing::info!(
        "harrow (meguri) listening on {} with {worker_count} workers (io_uring)",
        inner.local_addr()
    );

    Ok(ServerHandle { inner })
}

#[cfg(target_os = "linux")]
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
    let shutdown = harrow_server::ShutdownSignal::new();
    let per_worker = ServerConfig {
        workers: Some(1),
        ..config
    };

    worker_loop(shared, listener, &per_worker, shutdown, None)?;

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
    inner: harrow_server::ThreadedServerHandle<BoxError, fn(SocketAddr, usize)>,
}

#[cfg(target_os = "linux")]
impl ServerHandle {
    /// The socket address the server bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    /// Signal shutdown and wait for all workers to exit.
    pub fn shutdown(self) -> Result<(), BoxError> {
        self.inner.shutdown()
    }

    /// Block until shutdown is triggered or a worker exits with an error.
    pub fn wait(self) -> Result<(), BoxError> {
        self.inner.wait()
    }
}

#[cfg(target_os = "linux")]
fn wake_workers(addr: SocketAddr, worker_count: usize) {
    for _ in 0..worker_count.max(1) {
        let _ = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
// ---------------------------------------------------------------------------
// Worker spawning
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
struct StartupSignal {
    ready: harrow_server::WorkerReady,
    addr: SocketAddr,
}

/// One worker thread: its own `SO_REUSEPORT` listener and io_uring loop.
#[cfg(target_os = "linux")]
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

    let listener = create_listener(ctx.addr, true)?;
    let addr = listener.local_addr()?;
    worker_loop(
        shared,
        listener,
        config,
        ctx.shutdown,
        Some(StartupSignal {
            ready: ctx.ready,
            addr,
        }),
    )
}

// ---------------------------------------------------------------------------
// Worker event loop (the core)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn worker_loop(
    shared: Arc<SharedState>,
    listener: std::net::TcpListener,
    config: &ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
    mut startup: Option<StartupSignal>,
) -> Result<(), BoxError> {
    // Read by the kernel when a TIMEOUT SQE pointing at it is submitted.
    // Declared before `ring` so it is dropped after it: an SQE still queued
    // in the ring can never point at a dead stack slot.
    let mut timeout_ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let mut ring = match meguri::Ring::new(config.ring_entries) {
        Ok(ring) => ring,
        Err(err) => return Err(Box::new(err)),
    };
    tracing::debug!(
        "meguri worker ring created with {} entries",
        config.ring_entries
    );

    // Every `Conn` in the slab owns its socket fd and the buffers its SQEs
    // point into. A slot is freed only when no SQE for it is in flight; see
    // `close_conn` and `teardown`.
    let mut conns: slab::Slab<connection::Conn> = slab::Slab::new();
    let mut next_generation: u32 = 0;

    // Embed a tokio current-thread runtime for async dispatch.
    let tokio_rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => return Err(Box::new(err)),
    };
    let local = tokio::task::LocalSet::new();

    let listener_fd = listener.as_raw_fd();

    let mut dispatch_tick_armed = false;
    let mut timeout_armed = false;
    let mut timeout_deadline = None;
    let mut deadline_heap = BinaryHeap::new();

    let mut accept_armed = submit_accept(&mut ring, listener_fd);

    if let Some(signal) = startup.take() {
        signal.ready.listening(signal.addr);
    }

    // An error from io_uring_enter ends the loops, but never skips
    // `teardown`: freeing `conns` with SQEs in flight would hand their
    // buffers back to the allocator while the kernel may still write them.
    let mut failure: Option<std::io::Error> = None;

    loop {
        // Check shutdown flag.
        if shutdown.is_shutdown() {
            tracing::debug!("worker shutting down");
            break;
        }

        // Re-arm accept if the previous attempt found the SQ full.
        if !accept_armed {
            accept_armed = submit_accept(&mut ring, listener_fd);
        }

        schedule_next_timeout(
            &mut ring,
            &mut deadline_heap,
            &conns,
            &mut timeout_armed,
            &mut timeout_deadline,
            &mut timeout_ts,
        );

        if dispatch_tick_needed(&conns) && !dispatch_tick_armed {
            dispatch_tick_armed = submit_dispatch_tick(&mut ring);
        }

        // Submit pending SQEs and wait for at least one completion.
        if let Err(err) = wait_for_completions(&mut ring) {
            failure = Some(err);
            break;
        }

        // Drain all available completions.
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;

            if user_data == ACCEPT_USER_DATA {
                accept_armed = false;
                if let Some(idx) =
                    handle_accept(res, &mut ring, &mut conns, config, &mut next_generation)
                {
                    refresh_timeout_deadline(&mut deadline_heap, &mut conns, idx, config);
                }
            } else if user_data == TIMEOUT_USER_DATA {
                timeout_armed = false;
                timeout_deadline = None;
                sweep_timed_out_connections(&mut ring, &mut conns, config, &mut deadline_heap);
            } else if user_data == TIMEOUT_CANCEL_USER_DATA {
                // Old timeout canceled so a new nearest-deadline timeout can replace it.
            } else if user_data == DISPATCH_TICK_USER_DATA {
                dispatch_tick_armed = false;
            } else if ConnToken::decode(user_data, RECV_CANCEL_USER_DATA_FLAG).is_some() {
                // Nothing to do: the cancelled RECV's own CQE (always posted,
                // cancelled or not) turns the armed timeout into a 408.
            } else if let Some(token) = ConnToken::decode(user_data, RECV_USER_DATA_FLAG) {
                handle_recv_completion(token, res, &mut ring, &mut conns, &shared, &local, config);
                refresh_timeout_deadline(&mut deadline_heap, &mut conns, token.idx, config);
            } else if let Some(token) = ConnToken::decode(user_data, WRITE_USER_DATA_FLAG) {
                handle_write_completion(token, res, &mut ring, &mut conns, &shared, &local, config);
                refresh_timeout_deadline(&mut deadline_heap, &mut conns, token.idx, config);
            } else {
                tracing::debug!("ignoring unexpected CQE user_data={user_data} res={res}");
            }

            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();

        drive_dispatch_runtime(&tokio_rt, &local);
        advance_dispatching_connections(
            &mut ring,
            &mut conns,
            &shared,
            &local,
            config,
            &mut deadline_heap,
        );
        advance_response_streaming_connections(
            &mut ring,
            &mut conns,
            &shared,
            &local,
            config,
            &mut deadline_heap,
        );
    }

    if shutdown.is_shutdown() {
        begin_drain(&mut conns);
    }

    // Drain phase: wait for in-flight connections.
    let drain_start = std::time::Instant::now();
    while failure.is_none() && !conns.is_empty() && drain_start.elapsed() < config.drain_timeout {
        reap_closed_drain_connections(&mut conns);
        if conns.is_empty() {
            break;
        }

        schedule_next_timeout(
            &mut ring,
            &mut deadline_heap,
            &conns,
            &mut timeout_armed,
            &mut timeout_deadline,
            &mut timeout_ts,
        );
        if dispatch_tick_needed(&conns) && !dispatch_tick_armed {
            dispatch_tick_armed = submit_dispatch_tick(&mut ring);
        }
        if let Err(err) = wait_for_completions(&mut ring) {
            failure = Some(err);
            break;
        }
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;
            if user_data == ACCEPT_USER_DATA {
                accept_armed = false;
                if res >= 0 {
                    close_accepted_fd(res);
                }
            } else if user_data == DISPATCH_TICK_USER_DATA {
                dispatch_tick_armed = false;
            } else if user_data == TIMEOUT_USER_DATA {
                timeout_armed = false;
                timeout_deadline = None;
                sweep_timed_out_connections(&mut ring, &mut conns, config, &mut deadline_heap);
            } else if user_data == TIMEOUT_CANCEL_USER_DATA
                || ConnToken::decode(user_data, RECV_CANCEL_USER_DATA_FLAG).is_some()
            {
            } else if let Some(token) = ConnToken::decode(user_data, RECV_USER_DATA_FLAG) {
                handle_recv_completion(token, res, &mut ring, &mut conns, &shared, &local, config);
                refresh_timeout_deadline(&mut deadline_heap, &mut conns, token.idx, config);
            } else if let Some(token) = ConnToken::decode(user_data, WRITE_USER_DATA_FLAG) {
                handle_write_completion(token, res, &mut ring, &mut conns, &shared, &local, config);
                refresh_timeout_deadline(&mut deadline_heap, &mut conns, token.idx, config);
            }
            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();
        reap_closed_drain_connections(&mut conns);
        drive_dispatch_runtime(&tokio_rt, &local);
        advance_dispatching_connections(
            &mut ring,
            &mut conns,
            &shared,
            &local,
            config,
            &mut deadline_heap,
        );
        advance_response_streaming_connections(
            &mut ring,
            &mut conns,
            &shared,
            &local,
            config,
            &mut deadline_heap,
        );
    }

    if !conns.is_empty() {
        tracing::warn!("drain timeout, {} connections aborted", conns.len());
    }
    teardown(&mut ring, &mut conns, &mut accept_armed);

    match failure {
        Some(err) => Err(Box::new(err)),
        None => Ok(()),
    }
}

/// Shutdown started. In-flight requests are finished without keep-alive,
/// and idle connections (waiting for a request of which no byte has
/// arrived) are closed now: otherwise each would hold the drain until its
/// header read timeout, or until the drain timeout without one. A request
/// that is partly received is still read and answered.
#[cfg(target_os = "linux")]
fn begin_drain(conns: &mut slab::Slab<connection::Conn>) {
    let mut idle = Vec::new();
    for (idx, conn) in conns.iter_mut() {
        conn.trace.record(Event::Shutdown);
        if matches!(conn.state, connection::ConnState::Closed) {
            continue;
        }
        conn.draining = true;
        conn.keep_alive = false;
        if matches!(conn.state, connection::ConnState::Headers) && conn.buf.is_empty() {
            idle.push(idx);
        }
    }
    for idx in idle {
        // The server stops waiting for the next request.
        close_conn(idx, conns, CloseReason::Timeout);
    }
}

/// Submit queued SQEs and block until at least one CQE is available.
///
/// `EINTR` (any signal delivered to the worker thread, e.g. a profiler's
/// SIGPROF) is retried rather than treated as fatal. `EBUSY`/`EAGAIN` mean
/// the kernel cannot take more work until completions are reaped (CQ
/// overflow backlog) or is short of memory; the caller drains the CQ and
/// tries again on the next iteration.
#[cfg(target_os = "linux")]
fn wait_for_completions(ring: &mut meguri::Ring) -> std::io::Result<()> {
    loop {
        match ring.submit_and_wait(1) {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if matches!(err.raw_os_error(), Some(libc::EBUSY | libc::EAGAIN)) => {
                tracing::debug!("io_uring_enter: {err}; draining completions first");
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }
}

/// How long `teardown` waits for in-flight SQEs after shutting sockets down.
#[cfg(target_os = "linux")]
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Close every remaining connection, then wait until no SQE references
/// memory owned by this worker, so the slab can be freed.
///
/// `close_conn` shuts sockets with in-flight SQEs down, which makes their
/// RECV/SEND complete right away. The accept SQE is cancelled so an accepted
/// socket cannot appear after the worker has stopped reading CQEs (it would
/// leak). If the kernel still has not finished after `TEARDOWN_TIMEOUT`, the
/// remaining connections are leaked rather than freed.
#[cfg(target_os = "linux")]
fn teardown(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    accept_armed: &mut bool,
) {
    let all: Vec<usize> = conns.iter().map(|(idx, _)| idx).collect();
    for idx in all {
        // The drain ended: the server stops waiting for these connections.
        close_conn(idx, conns, CloseReason::Timeout);
    }
    if conns.is_empty() && !*accept_armed {
        return;
    }

    // Flush first so the SQ has room for the cancel and the timer.
    let _ = ring.submit();
    if *accept_armed {
        let _ = ring
            .sq()
            .push_cancel(ACCEPT_CANCEL_USER_DATA, ACCEPT_USER_DATA);
    }
    // SAFETY: `TEARDOWN_TS` is a `static`, valid for as long as the SQE can
    // be submitted.
    let timer_armed = unsafe {
        ring.sq()
            .push_timeout(TEARDOWN_TIMER_USER_DATA, &TEARDOWN_TS, 0, 0)
    };

    let deadline = std::time::Instant::now() + TEARDOWN_TIMEOUT;
    while timer_armed
        && (!conns.is_empty() || *accept_armed)
        && std::time::Instant::now() < deadline
    {
        if wait_for_completions(ring).is_err() {
            break;
        }
        while let Some(cqe) = ring.cq().peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;
            if user_data == ACCEPT_USER_DATA {
                *accept_armed = false;
                if res >= 0 {
                    close_accepted_fd(res);
                }
            } else if let Some(token) = ConnToken::decode(user_data, RECV_USER_DATA_FLAG) {
                if let Some(conn) = token.get(conns) {
                    conn.recv_pending = false;
                    conn.discard_recv();
                }
                release_if_idle(token.idx, conns);
            } else if let Some(token) = ConnToken::decode(user_data, WRITE_USER_DATA_FLAG) {
                if let Some(conn) = token.get(conns) {
                    conn.write_pending = false;
                }
                release_if_idle(token.idx, conns);
            }
            ring.cq_mut().advance();
        }
        ring.cq_mut().flush_head();
    }

    if !conns.is_empty() {
        tracing::warn!(
            "{} connections still had I/O in flight at worker exit; leaking their buffers",
            conns.len()
        );
        for conn in conns.drain() {
            std::mem::forget(conn);
        }
    }
}

/// Close an fd an ACCEPT CQE handed to us that no connection will own.
#[cfg(target_os = "linux")]
fn close_accepted_fd(fd: i32) {
    // SAFETY: a successful ACCEPT CQE returns a new fd that nothing else
    // owns; it is closed exactly once here. close(2) errors are ignored:
    // Linux releases the fd even when close reports an error.
    unsafe {
        libc::close(fd);
    }
}

/// Free a closed connection's slot once its last in-flight SQE completed.
#[cfg(target_os = "linux")]
fn release_if_idle(idx: usize, conns: &mut slab::Slab<connection::Conn>) {
    if conns.get(idx).is_some_and(|conn| {
        matches!(conn.state, connection::ConnState::Closed) && !conn.has_io_in_flight()
    }) {
        remove_conn(idx, conns);
    }
}

/// Remove a connection from its slot. Every removal goes through here, so a
/// connection closed by a timeout records the model's `ClosedCqe` (its
/// abandoned read or write is over) before its trace is recorded on drop.
#[cfg(target_os = "linux")]
fn remove_conn(idx: usize, conns: &mut slab::Slab<connection::Conn>) {
    if let Some(mut conn) = conns.try_remove(idx)
        && conn.awaiting_closed_cqe
    {
        conn.trace.record(Event::ClosedCqe);
    }
}

#[cfg(target_os = "linux")]
fn reap_closed_drain_connections(conns: &mut slab::Slab<connection::Conn>) {
    // Only slots whose SQEs have all completed; the others are released by
    // their last CQE (`release_if_idle`).
    let closed: Vec<usize> = conns
        .iter()
        .filter(|(_, conn)| {
            matches!(conn.state, connection::ConnState::Closed) && !conn.has_io_in_flight()
        })
        .map(|(idx, _)| idx)
        .collect();

    for idx in closed {
        remove_conn(idx, conns);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn resolve_worker_count(workers: Option<usize>) -> Result<usize, BoxError> {
    Ok(harrow_server::resolve_worker_count(workers).map_err(Box::new)?)
}

#[cfg(target_os = "linux")]
fn per_worker_config(config: ServerConfig, workers: usize) -> ServerConfig {
    let per_worker_max = harrow_server::per_worker_max_connections(config.max_connections, workers);
    ServerConfig {
        max_connections: per_worker_max.max(1),
        workers: Some(1),
        ..config
    }
}

#[cfg(target_os = "linux")]
fn create_listener(addr: SocketAddr, reuse_port: bool) -> std::io::Result<std::net::TcpListener> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };

    // Create the socket by hand so options can be set BEFORE bind.
    // Non-blocking from the start (as before, when it was set after listen)
    // and close-on-exec.
    // SAFETY: socket(2) takes no pointers; the result is checked below.
    let raw = unsafe {
        libc::socket(
            domain,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Owned from here on: every early return below closes it.
    // SAFETY: socket(2) just returned this fd and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    set_socket_flag(&fd, libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
    // SO_REUSEPORT must be set before bind for the kernel to spread
    // connections across the workers' listeners; without it the second
    // worker's bind fails with EADDRINUSE, so a failure here is fatal too.
    if reuse_port {
        set_socket_flag(&fd, libc::SOL_SOCKET, libc::SO_REUSEPORT)?;
    }

    // SAFETY: all-zero is a valid `sockaddr_storage`.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let sa_len: libc::socklen_t = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: `sockaddr_storage` is large and aligned enough for
            // every `sockaddr_*` type.
            unsafe { std::ptr::write((&raw mut storage).cast(), sin) };
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: as above.
            unsafe { std::ptr::write((&raw mut storage).cast(), sin6) };
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    };

    // SAFETY: `storage` is a local that outlives the call and holds a
    // sockaddr of family `domain` whose length is `sa_len`.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const storage).cast::<libc::sockaddr>(),
            sa_len,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: listen(2) takes no pointers; `fd` is a bound socket.
    if unsafe { libc::listen(fd.as_raw_fd(), 1024) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Convert to std TcpListener for local_addr() etc.; it now owns the fd.
    Ok(std::net::TcpListener::from(fd))
}

/// Enable a boolean socket option, reporting failure.
#[cfg(target_os = "linux")]
fn set_socket_flag(
    fd: &impl AsRawFd,
    level: libc::c_int,
    name: libc::c_int,
) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: `on` is a live `c_int` for the duration of the call and the
    // length passed matches its size; the kernel copies it synchronously.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const on).cast(),
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SQE submission helpers
// ---------------------------------------------------------------------------

/// Queue the ACCEPT SQE. Returns `false` if the SQ is full (the caller
/// retries on the next loop iteration).
///
/// The peer address is not needed, so `addr`/`addrlen` are null: the SQE
/// then references no user memory at all, and nothing has to stay alive
/// until its CQE.
///
/// The accepted socket is created close-on-exec (so it does not leak into
/// child processes) and non-blocking (matching the listener), atomically,
/// instead of with a follow-up fcntl(2) whose result went unchecked.
#[cfg(target_os = "linux")]
fn submit_accept(ring: &mut meguri::Ring, listener_fd: std::os::fd::RawFd) -> bool {
    // SAFETY: null `addr`/`addrlen` is allowed and points at no memory. The
    // accepted fd in the CQE is taken over by `handle_accept` (or closed by
    // the drain/teardown loops); `listener_fd` outlives the ring.
    unsafe {
        ring.sq().push_accept(
            ACCEPT_USER_DATA,
            listener_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32,
        )
    }
}

#[cfg(target_os = "linux")]
fn handle_accept(
    res: i32,
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    config: &ServerConfig,
    next_generation: &mut u32,
) -> Option<usize> {
    if res < 0 {
        let err = std::io::Error::from_raw_os_error(-res);
        tracing::error!("accept error: {err}");
        return None;
    }

    let fd = res;

    if conns.len() >= config.max_connections {
        tracing::warn!("connection limit reached, dropping new connection");
        close_accepted_fd(fd);
        return None;
    }

    // Disable Nagle's algorithm. A failure only costs latency, so the
    // connection is still served.
    if let Err(err) = set_socket_flag(&fd, libc::IPPROTO_TCP, libc::TCP_NODELAY) {
        tracing::debug!("TCP_NODELAY on fd {fd}: {err}");
    }

    // From here the `Conn` owns `fd`; `close_conn` closes it.
    let mut conn = connection::Conn::new(fd);
    conn.trace.record(Event::Accept);
    conn.generation = *next_generation;
    *next_generation = next_generation.wrapping_add(1) & CONN_GENERATION_MASK;
    let conn_idx = conns.insert(conn);
    submit_recv_or_close(ring, conn_idx, conns);
    Some(conn_idx)
}

/// Queue a RECV into the spare capacity of `conn.buf`.
///
/// Buffer lifetime: the SQE holds a raw pointer into `conn.buf`. It stays
/// valid because (1) `conn.buf` is not reallocated while `recv_pending` is
/// set (`Conn::prepare_recv` documents this and `commit_recv` checks it), and
/// (2) the `Conn` is not removed from the slab while `recv_pending` is set
/// (`close_conn` keeps the slot until the CQE arrives).
#[cfg(target_os = "linux")]
fn submit_recv(ring: &mut meguri::Ring, conn_idx: usize, conn: &mut connection::Conn) -> bool {
    if conn.recv_pending || conn.fd < 0 {
        debug_assert!(false, "RECV submitted twice or on a closed connection");
        return false;
    }
    let (buf_ptr, len) = conn.prepare_recv(codec::DEFAULT_BUFFER_SIZE);
    let user_data = ConnToken::of(conn_idx, conn).user_data(RECV_USER_DATA_FLAG);

    // SAFETY: `buf_ptr..buf_ptr + len` is `conn.buf`'s spare capacity (heap,
    // valid for writes). It stays allocated and unmoved until the CQE: the
    // slot is kept while `recv_pending` (`close_conn`, `teardown`), and
    // `conn.buf` is not reallocated while a RECV is pending (checked by
    // `commit_recv`). `conn.fd` is the connection's open socket; if it is
    // closed early, the kernel keeps its own reference.
    let pushed = unsafe { ring.sq().push_recv(user_data, conn.fd, buf_ptr, len, 0) };
    if !pushed {
        conn.discard_recv();
        tracing::error!("SQ full, dropping connection fd {}", conn.fd);
        return false;
    }

    conn.recv_pending = true;
    true
}

#[cfg(target_os = "linux")]
fn handle_recv_completion(
    token: ConnToken,
    res: i32,
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    let conn_idx = token.idx;
    // A CQE for an older connection in this slot: that connection is gone,
    // and this CQE must not touch the current one.
    let Some(conn) = token.get(conns) else {
        return;
    };
    if !conn.recv_pending {
        return;
    }
    conn.recv_pending = false;

    if matches!(conn.state, connection::ConnState::Closed) {
        conn.discard_recv();
        release_if_idle(conn_idx, conns);
        return;
    }

    if let Some(error) = conn.take_timeout_error() {
        conn.discard_recv();
        if conn.write_pending || !matches!(conn.state, connection::ConnState::Dispatching) {
            // A response is already on its way (it replaced the request
            // phase that timed out) and ends with connection: close; its
            // buffer must not be replaced under the in-flight SEND.
            conn.keep_alive = false;
            return;
        }
        conn.set_error_response(error);
        submit_write_or_close(ring, conn_idx, conns);
        return;
    }

    if res < 0 {
        tracing::debug!(
            "recv error on fd {}: {}",
            conn.fd,
            std::io::Error::from_raw_os_error(-res)
        );
        conn.discard_recv();
        close_conn(conn_idx, conns, CloseReason::IoError);
        return;
    }

    let nbytes = match conn.commit_recv(res as usize) {
        Ok(nbytes) => nbytes,
        Err(connection::RecvBufferMoved) => {
            debug_assert!(false, "receive buffer moved while a RECV was in flight");
            tracing::error!("receive buffer moved under a RECV on fd {}", conn.fd);
            close_conn(conn_idx, conns, CloseReason::IoError);
            return;
        }
    };

    if matches!(conns[conn_idx].state, connection::ConnState::Headers) {
        let result = conns[conn_idx].on_recv(nbytes, config.max_body_size);
        handle_process_result(ring, conns, conn_idx, result, shared, local, config);
    } else if matches!(conns[conn_idx].state, connection::ConnState::Dispatching) {
        if nbytes == 0 {
            // Peer EOF before the request body ended: the body can never
            // complete. (Pumping again would only queue another RECV that
            // returns 0 at once, spinning until the body read timeout.)
            close_conn(conn_idx, conns, CloseReason::IoError);
            return;
        }
        advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
    }
}

#[cfg(target_os = "linux")]
fn handle_write_completion(
    token: ConnToken,
    res: i32,
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    shared: &Arc<SharedState>,
    local: &tokio::task::LocalSet,
    config: &ServerConfig,
) {
    let conn_idx = token.idx;
    let Some(conn) = token.get(conns) else {
        return;
    };
    if !conn.write_pending {
        return;
    }

    conn.write_pending = false;

    if matches!(conn.state, connection::ConnState::Closed) {
        release_if_idle(conn_idx, conns);
        return;
    }

    if res < 0 {
        tracing::debug!(
            "write error on fd {}: {}",
            conn.fd,
            std::io::Error::from_raw_os_error(-res)
        );
        close_conn(conn_idx, conns, CloseReason::IoError);
        return;
    }

    let nbytes = res as usize;
    let result = conn.on_write(nbytes);
    handle_write_result(ring, conns, conn_idx, result, shared, local, config);
}

/// Queue a SEND of the unwritten part of `conn.response_buf`.
///
/// Buffer lifetime: as for `submit_recv`. `response_buf` is not modified or
/// replaced while `write_pending` is set (debug-asserted in `Conn`), and the
/// slot is kept until the CQE arrives.
#[cfg(target_os = "linux")]
fn submit_write(ring: &mut meguri::Ring, conn_idx: usize, conn: &mut connection::Conn) -> bool {
    if conn.write_pending || conn.fd < 0 {
        debug_assert!(false, "SEND submitted twice or on a closed connection");
        return false;
    }
    let Some((ptr, len)) = conn.prepare_write() else {
        return true;
    };
    let user_data = ConnToken::of(conn_idx, conn).user_data(WRITE_USER_DATA_FLAG);

    // SAFETY: `ptr..ptr + len` is the unwritten tail of `conn.response_buf`
    // (heap, valid for reads). It stays allocated, unmoved and unmodified
    // until the CQE: the slot is kept while `write_pending`, and nothing
    // replaces or refills `response_buf` while a SEND is pending.
    let pushed = unsafe { ring.sq().push_send(user_data, conn.fd, ptr, len, 0) };
    if !pushed {
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

/// Ask the kernel to cancel the connection's in-flight RECV. The target
/// carries the generation, so it can only ever hit this connection's RECV.
#[cfg(target_os = "linux")]
fn submit_recv_cancel(ring: &mut meguri::Ring, token: ConnToken) -> bool {
    ring.sq().push_cancel(
        token.user_data(RECV_CANCEL_USER_DATA_FLAG),
        token.user_data(RECV_USER_DATA_FLAG),
    )
}

#[cfg(target_os = "linux")]
/// Queue the short dispatch wake tick. Returns `false` if the SQ is full, so
/// the caller does not believe a tick is armed (it would otherwise never arm
/// another, and a pending dispatch could stall until an unrelated CQE).
fn submit_dispatch_tick(ring: &mut meguri::Ring) -> bool {
    // SAFETY: `DISPATCH_TICK_TS` is a `static`.
    unsafe {
        ring.sq()
            .push_timeout(DISPATCH_TICK_USER_DATA, &DISPATCH_TICK_TS, 0, 0)
    }
}

#[cfg(target_os = "linux")]
fn schedule_next_timeout(
    ring: &mut meguri::Ring,
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
    conns: &slab::Slab<connection::Conn>,
    timeout_armed: &mut bool,
    timeout_deadline: &mut Option<std::time::Instant>,
    timeout_ts: &mut libc::timespec,
) {
    let next_deadline = next_timeout_deadline(deadline_heap, conns);
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
    // SAFETY: `timeout_ts` is the worker loop's local, declared before the
    // ring, so it outlives every submit of this SQE. It is next rewritten
    // only on a later call, after the loop has submitted this SQE (each
    // iteration submits before scheduling again) and the kernel has copied
    // the value.
    if unsafe {
        ring.sq()
            .push_timeout(TIMEOUT_USER_DATA, timeout_ts as *const _, 0, 0)
    } {
        *timeout_armed = true;
    }
}

#[cfg(target_os = "linux")]
fn next_timeout_deadline(
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
    conns: &slab::Slab<connection::Conn>,
) -> Option<std::time::Instant> {
    while let Some(Reverse(entry)) = deadline_heap.peek().copied() {
        let Some(conn) = conns.get(entry.conn_idx) else {
            let _ = deadline_heap.pop();
            continue;
        };

        if matches!(conn.state, connection::ConnState::Closed)
            || conn.deadline_generation != entry.generation
            || conn.tracked_deadline != Some(entry.deadline)
        {
            let _ = deadline_heap.pop();
            continue;
        }

        return Some(entry.deadline);
    }

    None
}

#[cfg(target_os = "linux")]
fn refresh_timeout_deadline(
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
    conns: &mut slab::Slab<connection::Conn>,
    conn_idx: usize,
    config: &ServerConfig,
) {
    let Some(conn) = conns.get_mut(conn_idx) else {
        return;
    };

    conn.deadline_generation = conn.deadline_generation.wrapping_add(1);
    conn.tracked_deadline = if matches!(conn.state, connection::ConnState::Closed) {
        None
    } else {
        connection_timeout_deadline(conn, config)
    };

    if let Some(deadline) = conn.tracked_deadline {
        deadline_heap.push(Reverse(DeadlineEntry {
            deadline,
            conn_idx,
            generation: conn.deadline_generation,
        }));
    }
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
fn sweep_timed_out_connections(
    ring: &mut meguri::Ring,
    conns: &mut slab::Slab<connection::Conn>,
    config: &ServerConfig,
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
) {
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
        let body_timeout = conns[idx].body_read_timed_out(config.body_read_timeout);

        if body_timeout {
            let recv_pending = conns[idx].recv_pending;
            let write_pending = conns[idx].write_pending;
            let fd = conns[idx].fd;
            tracing::debug!("body read timeout fd={fd}");

            if recv_pending {
                // The RECV's CQE (ECANCELED, or data if it won the race)
                // turns the armed error into a 408; see handle_recv_completion.
                conns[idx].arm_timeout_error(ErrorResponse::RequestTimeout);
                let token = ConnToken::of(idx, &conns[idx]);
                if !submit_recv_cancel(ring, token) {
                    close_conn(idx, conns, CloseReason::Timeout);
                }
            } else if write_pending {
                close_conn(idx, conns, CloseReason::Timeout);
            } else {
                conns[idx].set_error_response(ErrorResponse::RequestTimeout);
                submit_write_or_close(ring, idx, conns);
            }
        } else {
            tracing::debug!("closing timed-out connection fd={}", conns[idx].fd);
            close_conn(idx, conns, CloseReason::Timeout);
        }

        refresh_timeout_deadline(deadline_heap, conns, idx, config);
    }
}

/// Send HTTP/1.1 100 Continue interim response.
///
/// This is a synchronous (non-SQE) send because the client is blocking on it
/// before sending the body.  The buffer is tiny (25 bytes) so a single
/// `send(2)` with `MSG_DONTWAIT` will never block on a connected socket.
#[cfg(target_os = "linux")]
fn submit_continue(_ring: &mut meguri::Ring, _conn_idx: usize, conn: &mut connection::Conn) {
    // SAFETY: `CONTINUE_100` is a `'static` byte string and send(2) reads
    // it synchronously; `conn.fd` is the connection's open socket (this is
    // only called for a live connection that just parsed headers).
    //
    // The result is ignored on purpose: this is best effort. If the 25
    // bytes do not fit right now (EAGAIN) or the peer is gone, a client
    // that sent `Expect: 100-continue` sends the body anyway after its own
    // timeout, and a dead peer surfaces on the next RECV/SEND CQE.
    unsafe {
        libc::send(
            conn.fd,
            codec::CONTINUE_100.as_ptr() as *const _,
            codec::CONTINUE_100.len(),
            libc::MSG_DONTWAIT,
        );
    }
}

/// Close a connection's socket and release its slab slot.
///
/// fd ownership: a `Conn` owns `conn.fd` from accept until this function,
/// the only place that closes it; `fd` is set to `-1` right after, so a
/// second call cannot close a reused fd number.
///
/// If a RECV or SEND for the connection is still in flight, the slot (and
/// with it `buf` / `response_buf`, which that SQE points into) is kept in
/// the `Closed` state and released by the last CQE (`release_if_idle`).
/// The socket is shut down first: io_uring holds its own reference to the
/// file, so close(2) alone would neither send FIN nor complete the RECV.
#[cfg(target_os = "linux")]
fn close_conn(conn_idx: usize, conns: &mut slab::Slab<connection::Conn>, reason: CloseReason) {
    let Some(conn) = conns.get_mut(conn_idx) else {
        return;
    };
    let in_flight = conn.has_io_in_flight();
    if !matches!(conn.state, connection::ConnState::Closed) {
        match reason {
            CloseReason::Finished => {}
            CloseReason::IoError => conn.trace.record(Event::IoError),
            CloseReason::Timeout => {
                conn.trace.record(Event::Timeout);
                conn.awaiting_closed_cqe = conn.lifecycle_io_pending();
            }
        }
    }
    if conn.fd >= 0 {
        // SAFETY: `conn.fd` is the connection's own socket, still open
        // (`fd >= 0`), and closed exactly once here. shutdown(2) can only
        // fail with ENOTCONN when the peer already reset the connection,
        // which is fine: the in-flight SQE then fails by itself. close(2)
        // errors are ignored because Linux releases the fd regardless.
        unsafe {
            if in_flight {
                libc::shutdown(conn.fd, libc::SHUT_RDWR);
            }
            libc::close(conn.fd);
        }
        conn.fd = -1;
    }
    if in_flight {
        conn.mark_closed();
    } else {
        remove_conn(conn_idx, conns);
    }
}

/// Why `close_conn` closes a connection, for its lifecycle trace.
#[cfg(target_os = "linux")]
#[derive(Copy, Clone, Debug)]
enum CloseReason {
    /// The trace already ends the connection (`WriteDone` with
    /// `keep_alive: false`, or a failed response recorded as `IoError`).
    Finished,
    /// Peer EOF, a failed RECV/SEND, or no room to queue one.
    IoError,
    /// The server stops waiting: header/body/lifetime timeout, or the drain
    /// ended. A read or write the model still has pending ends with
    /// `ClosedCqe` when the slot is freed.
    Timeout,
}

/// Submit a RECV or close the connection on SQ full.
#[cfg(target_os = "linux")]
fn submit_recv_or_close(
    ring: &mut meguri::Ring,
    conn_idx: usize,
    conns: &mut slab::Slab<connection::Conn>,
) {
    if !submit_recv(ring, conn_idx, &mut conns[conn_idx]) {
        close_conn(conn_idx, conns, CloseReason::IoError);
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
        close_conn(conn_idx, conns, CloseReason::IoError);
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
    let (version, is_head_request) = conns[conn_idx]
        .parsed
        .as_ref()
        .map_or((http::Version::HTTP_11, false), |parsed| {
            (parsed.version, parsed.method == http::Method::HEAD)
        });
    // A streaming response to HTTP/1.0 ends by closing the connection.
    if harrow_server::h1::response_closes_connection(version, &response, is_head_request) {
        conns[conn_idx].keep_alive = false;
    }
    let keep_alive = conns[conn_idx].keep_alive;
    let (tx, rx) = connection::response_channel();
    conns[conn_idx].set_response_receiver(rx);

    local.spawn_local(async move {
        connection::stream_response(tx, response, version, keep_alive, is_head_request).await;
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
                close_conn(conn_idx, conns, CloseReason::IoError);
                return;
            }

            advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
        }
        connection::ProcessResult::WriteError(resp) => {
            conns[conn_idx].set_serialized_response(resp, false);
            conns[conn_idx].buf.clear();
            submit_write_or_close(ring, conn_idx, conns);
        }
        // Peer EOF while waiting for a request head.
        connection::ProcessResult::Close => {
            close_conn(conn_idx, conns, CloseReason::IoError);
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
            close_conn(conn_idx, conns, CloseReason::Finished);
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
        // The response body failed after its head went out.
        connection::ResponseProgress::StreamError => {
            close_conn(conn_idx, conns, CloseReason::IoError);
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
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
) {
    let dispatching: Vec<usize> = conns
        .iter()
        .filter(|(_, conn)| matches!(conn.state, connection::ConnState::Dispatching))
        .map(|(idx, _)| idx)
        .collect();

    for conn_idx in dispatching {
        if conns.contains(conn_idx) {
            advance_dispatching_connection(ring, conns, conn_idx, shared, local, config);
            refresh_timeout_deadline(deadline_heap, conns, conn_idx, config);
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
    deadline_heap: &mut BinaryHeap<Reverse<DeadlineEntry>>,
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
            refresh_timeout_deadline(deadline_heap, conns, conn_idx, config);
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
        let conn = &mut conns[conn_idx];
        conn.trace.record(if conn.request_body_in_progress() {
            // The handler answered before the request body ended.
            Event::EarlyResponse
        } else {
            Event::DispatchDone
        });
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
            let conn = &mut conns[conn_idx];
            // The model's first body read is armed by `HeadersParsed`.
            if std::mem::replace(&mut conn.body_recv_armed, true) {
                conn.trace.record(Event::BodyNeedMore);
            }
            submit_recv_or_close(ring, conn_idx, conns);
        }
        connection::BodyPumpResult::Blocked => {}
        connection::BodyPumpResult::Eof => conns[conn_idx].trace.record(Event::BodyDone),
        connection::BodyPumpResult::ReceiverClosed => {
            // The handler dropped the request body: no more body to read.
            let control = early_response_control(EarlyResponseMode::DropRequestBody);
            let conn = &mut conns[conn_idx];
            conn.trace.record(Event::BodyDone);
            conn.keep_alive = control.keep_alive;
            conn.abort_request_body();
        }
        connection::BodyPumpResult::ResponseError(error) => {
            conns[conn_idx].set_error_response(error);
            submit_write_or_close(ring, conn_idx, conns);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn test_config() -> ServerConfig {
        ServerConfig {
            max_connections: 16,
            ring_entries: 64,
            header_read_timeout: Some(Duration::from_secs(5)),
            body_read_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(60)),
            drain_timeout: Duration::from_secs(5),
            max_body_size: 1024 * 1024,
            workers: Some(1),
        }
    }

    #[test]
    fn deadline_heap_skips_stale_entries_after_refresh() {
        let config = test_config();
        let mut heap = BinaryHeap::new();
        let mut conns = slab::Slab::new();
        let idx = conns.insert(connection::Conn::new(0));

        refresh_timeout_deadline(&mut heap, &mut conns, idx, &config);
        let first = next_timeout_deadline(&mut heap, &conns).unwrap();

        conns[idx].state = connection::ConnState::Writing;
        refresh_timeout_deadline(&mut heap, &mut conns, idx, &config);
        let next = next_timeout_deadline(&mut heap, &conns).unwrap();

        assert!(next >= first);
        assert_eq!(conns[idx].tracked_deadline, Some(next));
    }

    fn conn_with_generation(generation: u32) -> connection::Conn {
        // fd -1: these tests never touch a real socket.
        let mut conn = connection::Conn::new(-1);
        conn.generation = generation;
        conn
    }

    #[test]
    fn user_data_round_trips_index_generation_and_tag() {
        for tag in [
            RECV_USER_DATA_FLAG,
            WRITE_USER_DATA_FLAG,
            RECV_CANCEL_USER_DATA_FLAG,
        ] {
            let token = ConnToken {
                idx: 0xFFFF_FFFF,
                generation: CONN_GENERATION_MASK,
            };
            let user_data = token.user_data(tag);
            assert_eq!(ConnToken::decode(user_data, tag), Some(token));
            for other in [
                RECV_USER_DATA_FLAG,
                WRITE_USER_DATA_FLAG,
                RECV_CANCEL_USER_DATA_FLAG,
            ] {
                if other != tag {
                    assert_eq!(ConnToken::decode(user_data, other), None);
                }
            }
        }
    }

    #[test]
    fn special_user_data_never_decodes_as_a_connection() {
        for special in [
            ACCEPT_USER_DATA,
            TIMEOUT_USER_DATA,
            DISPATCH_TICK_USER_DATA,
            TIMEOUT_CANCEL_USER_DATA,
            ACCEPT_CANCEL_USER_DATA,
            TEARDOWN_TIMER_USER_DATA,
        ] {
            for tag in [
                RECV_USER_DATA_FLAG,
                WRITE_USER_DATA_FLAG,
                RECV_CANCEL_USER_DATA_FLAG,
            ] {
                assert_eq!(ConnToken::decode(special, tag), None, "{special:#x}");
            }
        }
    }

    /// A late CQE for a connection that has left its slot must not be
    /// applied to the next connection that reuses the slot.
    #[test]
    fn stale_cqe_does_not_reach_a_connection_reusing_the_slot() {
        let mut conns = slab::Slab::new();
        let old_idx = conns.insert(conn_with_generation(7));
        let stale = ConnToken::of(old_idx, &conns[old_idx]).user_data(RECV_USER_DATA_FLAG);
        conns.remove(old_idx);

        let new_idx = conns.insert(conn_with_generation(8));
        assert_eq!(new_idx, old_idx, "slab reuses the freed slot");
        conns[new_idx].recv_pending = true;

        let token = ConnToken::decode(stale, RECV_USER_DATA_FLAG).unwrap();
        assert_eq!(token.idx, new_idx);
        assert!(token.get(&mut conns).is_none(), "generation mismatch");

        let current = ConnToken::of(new_idx, &conns[new_idx]);
        assert!(current.get(&mut conns).is_some());
    }

    #[test]
    fn generation_wraps_within_its_bits() {
        let mut next = CONN_GENERATION_MASK;
        let first = next;
        next = next.wrapping_add(1) & CONN_GENERATION_MASK;
        assert_eq!(next, 0);
        let token = ConnToken {
            idx: 3,
            generation: first,
        };
        assert_eq!(
            ConnToken::decode(token.user_data(WRITE_USER_DATA_FLAG), WRITE_USER_DATA_FLAG),
            Some(token)
        );
    }

    /// `close_conn` must not free a connection (and its buffers) while the
    /// kernel may still write into them; the last CQE frees the slot.
    #[test]
    fn closing_with_io_in_flight_keeps_the_slot_until_the_last_cqe() {
        let mut conns = slab::Slab::new();
        let idx = conns.insert(conn_with_generation(1));
        conns[idx].recv_pending = true;
        conns[idx].write_pending = true;

        close_conn(idx, &mut conns, CloseReason::IoError);
        assert!(conns.contains(idx), "buffers still referenced by SQEs");
        assert!(matches!(conns[idx].state, connection::ConnState::Closed));
        assert_eq!(conns[idx].fd, -1);

        conns[idx].recv_pending = false;
        release_if_idle(idx, &mut conns);
        assert!(conns.contains(idx), "SEND still in flight");

        conns[idx].write_pending = false;
        release_if_idle(idx, &mut conns);
        assert!(!conns.contains(idx));
    }

    #[test]
    fn closing_an_idle_connection_frees_the_slot_at_once() {
        let mut conns = slab::Slab::new();
        let idx = conns.insert(conn_with_generation(1));
        close_conn(idx, &mut conns, CloseReason::IoError);
        assert!(!conns.contains(idx));
        // Closing again (e.g. from a stale path) is a no-op.
        close_conn(idx, &mut conns, CloseReason::IoError);
    }

    #[test]
    fn drain_reaping_skips_closed_connections_with_io_in_flight() {
        let mut conns = slab::Slab::new();
        let busy = conns.insert(conn_with_generation(1));
        conns[busy].recv_pending = true;
        conns[busy].mark_closed();
        let idle = conns.insert(conn_with_generation(2));
        conns[idle].mark_closed();

        reap_closed_drain_connections(&mut conns);
        assert!(conns.contains(busy));
        assert!(!conns.contains(idle));
    }

    extern "C" fn ignore_signal(_: libc::c_int) {}

    /// A signal delivered while the worker blocks in io_uring_enter makes it
    /// return EINTR. That used to propagate out of the event loop with `?`
    /// and kill the worker.
    #[test]
    fn wait_for_completions_retries_after_a_signal() {
        // SAFETY: installs a no-op handler (no SA_RESTART) for SIGUSR1, which
        // nothing else in this test binary uses.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                0
            );
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut ring = meguri::Ring::new(8).expect("ring");
            let ts = duration_to_timespec(Duration::from_millis(300));
            // SAFETY: `ts` outlives the submit right below.
            assert!(unsafe { ring.sq().push_timeout(1, &ts, 0, 0) });
            // Submit now: with nothing left to submit, an interrupted wait
            // reports EINTR (with SQEs submitted it reports their count).
            assert_eq!(ring.submit().expect("submit"), 1);
            // SAFETY: pthread_self has no preconditions.
            tx.send(unsafe { libc::pthread_self() }).unwrap();
            let started = std::time::Instant::now();
            let result = wait_for_completions(&mut ring);
            (
                result.map_err(|e| e.to_string()),
                started.elapsed(),
                ring.cq().peek().is_some(),
            )
        });

        let thread = rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: `thread` is still running (it waits up to 300ms).
        assert_eq!(unsafe { libc::pthread_kill(thread, libc::SIGUSR1) }, 0);

        let (result, elapsed, has_cqe) = worker.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(has_cqe, "kept waiting until the timer fired");
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    }

    /// Accepted sockets come out close-on-exec and non-blocking from the
    /// ACCEPT itself; no unchecked fcntl(2) afterwards.
    #[test]
    fn accepted_sockets_are_cloexec_and_nonblocking() {
        let listener = create_listener("127.0.0.1:0".parse().unwrap(), false).expect("listener");
        let mut ring = meguri::Ring::new(8).expect("ring");
        assert!(submit_accept(&mut ring, listener.as_raw_fd()));
        let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();

        wait_for_completions(&mut ring).expect("enter");
        let cqe = *ring.cq().peek().expect("accept CQE");
        ring.cq_mut().advance();
        ring.cq_mut().flush_head();
        assert_eq!(cqe.user_data, ACCEPT_USER_DATA);
        assert!(cqe.res >= 0, "accept failed: {}", cqe.res);

        // SAFETY: fcntl F_GETFD/F_GETFL on the fd the ACCEPT just returned.
        let (fd_flags, fl_flags) = unsafe {
            (
                libc::fcntl(cqe.res, libc::F_GETFD),
                libc::fcntl(cqe.res, libc::F_GETFL),
            )
        };
        close_accepted_fd(cqe.res);
        assert_ne!(
            fd_flags & libc::FD_CLOEXEC,
            0,
            "accepted fd must be close-on-exec"
        );
        assert_ne!(
            fl_flags & libc::O_NONBLOCK,
            0,
            "accepted fd must be non-blocking"
        );

        // SAFETY: as above, on the listener.
        let listener_fl = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(listener_fl & libc::O_NONBLOCK, 0);
    }

    fn open_fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    /// Every failure path of `create_listener` closes the socket it created.
    #[test]
    fn listener_errors_do_not_leak_the_socket() {
        let taken = create_listener("127.0.0.1:0".parse().unwrap(), false).expect("listener");
        let addr = taken.local_addr().unwrap();
        // Other tests open and close fds concurrently, so compare with slack:
        // a leak would add one fd per iteration.
        const ATTEMPTS: usize = 256;
        let before = open_fd_count();
        for _ in 0..ATTEMPTS {
            // Without SO_REUSEPORT on either side, bind fails: EADDRINUSE.
            let err = create_listener(addr, false).expect_err("address in use");
            assert_eq!(err.raw_os_error(), Some(libc::EADDRINUSE));
        }
        let after = open_fd_count();
        assert!(after < before + ATTEMPTS / 4, "{before} -> {after} fds");
    }

    #[test]
    fn deadline_heap_drops_closed_connections() {
        let config = test_config();
        let mut heap = BinaryHeap::new();
        let mut conns = slab::Slab::new();
        let idx = conns.insert(connection::Conn::new(0));

        refresh_timeout_deadline(&mut heap, &mut conns, idx, &config);
        assert!(next_timeout_deadline(&mut heap, &conns).is_some());

        conns[idx].state = connection::ConnState::Closed;
        refresh_timeout_deadline(&mut heap, &mut conns, idx, &config);

        assert!(next_timeout_deadline(&mut heap, &conns).is_none());
    }
}
