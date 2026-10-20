//! Shared server primitives for Harrow.
//!
//! Runtime-agnostic building blocks used by all server backends
//! (tokio, monoio, meguri): listener creation, worker spawning,
//! shutdown coordination, and configuration.

pub mod h1;
pub mod h1_lifecycle;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Server configuration shared across all backends.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    /// Divided equally across workers in multi-worker mode.
    pub max_connections: usize,
    /// Timeout for reading HTTP request headers. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Maximum connection lifetime. Default: Some(5 min).
    pub connection_timeout: Option<Duration>,
    /// Timeout for reading request body. Default: Some(30s).
    pub body_read_timeout: Option<Duration>,
    /// Drain timeout during shutdown. Default: 30s.
    pub drain_timeout: Duration,
    /// Maximum request body size in bytes. Default: 2 MiB.
    pub max_body_size: usize,
    /// Number of worker threads. None = auto-detect from CPU count.
    pub workers: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            header_read_timeout: Some(Duration::from_secs(5)),
            connection_timeout: Some(Duration::from_secs(300)),
            body_read_timeout: Some(Duration::from_secs(30)),
            drain_timeout: Duration::from_secs(30),
            max_body_size: 2 * 1024 * 1024,
            workers: None,
        }
    }
}

impl ServerConfig {
    /// Resolve the number of worker threads.
    /// `None` or `Some(0)` auto-detects from CPU count.
    pub fn worker_count(&self) -> usize {
        match self.workers {
            Some(0) | None => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            Some(n) => n,
        }
    }

    /// Compute per-worker max connections.
    pub fn per_worker_max_connections(&self) -> usize {
        let workers = self.worker_count();
        self.max_connections.div_ceil(workers.max(1)).max(1)
    }
}

/// Shared shutdown flag used across worker threads.
///
/// Check it with [`is_shutdown`](Self::is_shutdown), or await
/// [`wait`](Self::wait) to be woken when it is set. Runtime-agnostic: waiting
/// futures park their `Waker`, and [`shutdown`](Self::shutdown) wakes them
/// from any thread.
#[derive(Clone)]
pub struct ShutdownSignal {
    inner: Arc<ShutdownInner>,
}

struct ShutdownInner {
    flag: AtomicBool,
    waiters: std::sync::Mutex<Waiters>,
}

#[derive(Default)]
struct Waiters {
    next_key: u64,
    wakers: std::collections::HashMap<u64, std::task::Waker>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ShutdownInner {
                flag: AtomicBool::new(false),
                waiters: std::sync::Mutex::new(Waiters::default()),
            }),
        }
    }

    /// Signal shutdown and wake every pending [`wait`](Self::wait).
    pub fn shutdown(&self) {
        self.inner.flag.store(true, Ordering::Release);
        let wakers = std::mem::take(&mut self.lock_waiters().wakers);
        for waker in wakers.into_values() {
            waker.wake();
        }
    }

    /// Check if shutdown has been signaled.
    pub fn is_shutdown(&self) -> bool {
        self.inner.flag.load(Ordering::Acquire)
    }

    /// Resolve once shutdown has been signaled (immediately if it already
    /// has). Dropping the future before then unregisters it.
    pub fn wait(&self) -> ShutdownWait<'_> {
        ShutdownWait {
            signal: self,
            key: None,
        }
    }

    fn lock_waiters(&self) -> std::sync::MutexGuard<'_, Waiters> {
        // A panic while holding this lock cannot leave the map inconsistent.
        self.inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Future returned by [`ShutdownSignal::wait`].
pub struct ShutdownWait<'a> {
    signal: &'a ShutdownSignal,
    key: Option<u64>,
}

impl std::future::Future for ShutdownWait<'_> {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.signal.is_shutdown() {
            return std::task::Poll::Ready(());
        }
        let mut waiters = self.signal.lock_waiters();
        // Checked again under the lock: `shutdown` sets the flag before it
        // takes the lock, so either it sees this waker or this sees the flag.
        if self.signal.is_shutdown() {
            return std::task::Poll::Ready(());
        }
        let key = match self.key {
            Some(key) => key,
            None => {
                let key = waiters.next_key;
                waiters.next_key += 1;
                key
            }
        };
        waiters.wakers.insert(key, cx.waker().clone());
        drop(waiters);
        self.key = Some(key);
        std::task::Poll::Pending
    }
}

impl Drop for ShutdownWait<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            self.signal.lock_waiters().wakers.remove(&key);
        }
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a `std::net::TcpListener` with `SO_REUSEPORT` and `SO_REUSEADDR`
/// set before `bind`. Returns a non-blocking listener ready for use with
/// any async runtime.
pub fn reuseport_listener(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(65535)?;

    Ok(socket.into())
}

/// Spawn N worker threads, each calling `worker_fn` with a worker ID.
/// Returns the join handles, or an error if any thread fails to spawn.
pub fn spawn_workers<F>(
    count: usize,
    name_prefix: &str,
    worker_fn: F,
) -> std::io::Result<Vec<std::thread::JoinHandle<()>>>
where
    F: Fn(usize) + Send + Clone + 'static,
{
    let mut handles = Vec::with_capacity(count);
    for worker_id in 0..count {
        let f = worker_fn.clone();
        let name = format!("{name_prefix}{worker_id}");
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || f(worker_id))?;
        handles.push(handle);
    }
    Ok(handles)
}

/// Join all worker threads. Returns the first panic error if any.
pub fn join_workers(handles: Vec<std::thread::JoinHandle<()>>) -> Result<(), String> {
    let mut first_error = None;
    for handle in handles {
        if let Err(panic) = handle.join()
            && first_error.is_none()
        {
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                format!("worker panicked: {s}")
            } else if let Some(s) = panic.downcast_ref::<String>() {
                format!("worker panicked: {s}")
            } else {
                "worker panicked".to_string()
            };
            first_error = Some(msg);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Resolve a configured worker count.
///
/// `None` defaults to the available parallelism. `Some(0)` is rejected.
pub fn resolve_worker_count(workers: Option<usize>) -> std::io::Result<usize> {
    match workers {
        Some(0) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ServerConfig::workers must be greater than 0",
        )),
        Some(workers) => Ok(workers),
        None => Ok(thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)),
    }
}

/// Split a connection limit across workers, rounding up.
pub fn per_worker_max_connections(max_connections: usize, workers: usize) -> usize {
    max_connections.div_ceil(workers.max(1)).max(1)
}

/// No-op worker wake helper for runtimes that do not need an external wakeup.
pub fn noop_wake_workers(_addr: SocketAddr, _worker_count: usize) {}

fn join_panic_error(panic: Box<dyn std::any::Any + Send + 'static>) -> std::io::Error {
    let message = if let Some(message) = panic.downcast_ref::<&str>() {
        format!("worker thread panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("worker thread panicked: {message}")
    } else {
        "worker thread panicked".to_string()
    };

    std::io::Error::other(message)
}

/// How long [`start_workers`] waits for each worker to report its address.
const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// What a worker thread receives from [`start_workers`].
pub struct WorkerContext {
    /// Address to bind: the requested address for worker 0, and the address
    /// worker 0 actually bound for the others (so port 0 works).
    pub addr: SocketAddr,
    /// Worker index, `0..worker_count`.
    pub index: usize,
    /// Set when the server should stop accepting and drain.
    pub shutdown: ShutdownSignal,
    /// Report the bound address once the worker is accepting connections.
    pub ready: WorkerReady,
}

/// Startup handshake for one worker; see [`WorkerContext::ready`].
pub struct WorkerReady(mpsc::Sender<Result<SocketAddr, String>>);

impl WorkerReady {
    /// Report that the worker is listening on `addr`.
    pub fn listening(self, addr: SocketAddr) {
        let _ = self.0.send(Ok(addr));
    }
}

/// Start `worker_count` worker threads that each run `run_worker`, one at a
/// time: each must call `ctx.ready.listening(addr)` within 5 s before the
/// next starts. A worker that returns an error before that fails startup;
/// all started workers are then shut down and joined.
///
/// Backends own what runs inside a worker (runtime, listener, accept loop);
/// this owns thread lifecycle, startup, and shutdown coordination.
pub fn start_workers<E, W, F>(
    addr: SocketAddr,
    worker_count: usize,
    wake_workers: W,
    run_worker: F,
) -> Result<ThreadedServerHandle<E, W>, E>
where
    E: From<std::io::Error> + std::fmt::Display + Send + 'static,
    W: Fn(SocketAddr, usize) + Copy,
    F: Fn(WorkerContext) -> Result<(), E> + Send + Clone + 'static,
{
    let shutdown = ShutdownSignal::new();
    let (completion_tx, completion_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(worker_count);
    let mut bind_addr = addr;

    for index in 0..worker_count {
        let started = spawn_worker(
            index,
            bind_addr,
            &shutdown,
            &completion_tx,
            run_worker.clone(),
        )
        .and_then(|(handle, ready)| {
            workers.push(handle);
            wait_for_worker(index, &ready)
        });

        match started {
            Ok(bound) if index == 0 => bind_addr = bound,
            Ok(_) => {}
            Err(err) => {
                let _ = ThreadedServerHandle::new(
                    bind_addr,
                    shutdown,
                    completion_rx,
                    workers,
                    wake_workers,
                )
                .shutdown();
                return Err(E::from(err));
            }
        }
    }

    Ok(ThreadedServerHandle::new(
        bind_addr,
        shutdown,
        completion_rx,
        workers,
        wake_workers,
    ))
}

type WorkerReadyRx = mpsc::Receiver<Result<SocketAddr, String>>;

fn spawn_worker<E, F>(
    index: usize,
    addr: SocketAddr,
    shutdown: &ShutdownSignal,
    completion: &mpsc::Sender<Result<(), String>>,
    run_worker: F,
) -> std::io::Result<(thread::JoinHandle<Result<(), E>>, WorkerReadyRx)>
where
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(WorkerContext) -> Result<(), E> + Send + 'static,
{
    let (ready_tx, ready_rx) = mpsc::channel();
    let ctx = WorkerContext {
        addr,
        index,
        shutdown: shutdown.clone(),
        ready: WorkerReady(ready_tx.clone()),
    };
    let completion = completion.clone();

    let handle = thread::Builder::new()
        .name(format!("harrow-worker-{index}"))
        .spawn(move || {
            let result = run_worker(ctx);
            if let Err(err) = &result {
                // Ignored once the worker has already reported its address.
                let _ = ready_tx.send(Err(err.to_string()));
            }
            let _ = completion.send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            result
        })?;
    Ok((handle, ready_rx))
}

fn wait_for_worker(index: usize, ready: &WorkerReadyRx) -> std::io::Result<SocketAddr> {
    match ready.recv_timeout(WORKER_STARTUP_TIMEOUT) {
        Ok(Ok(addr)) => Ok(addr),
        Ok(Err(message)) => Err(std::io::Error::other(message)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("worker {index} did not report a bound address within 5s"),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(format!(
            "worker {index} exited before reporting a bound address"
        ))),
    }
}

/// Shared handle for backends that run one blocking worker thread per core.
pub struct ThreadedServerHandle<E, W = fn(SocketAddr, usize)>
where
    E: From<std::io::Error>,
    W: Fn(SocketAddr, usize) + Copy,
{
    addr: SocketAddr,
    shutdown: ShutdownSignal,
    completion: mpsc::Receiver<Result<(), String>>,
    workers: Vec<thread::JoinHandle<Result<(), E>>>,
    wake_workers: W,
}

impl<E, W> ThreadedServerHandle<E, W>
where
    E: From<std::io::Error>,
    W: Fn(SocketAddr, usize) + Copy,
{
    fn new(
        addr: SocketAddr,
        shutdown: ShutdownSignal,
        completion: mpsc::Receiver<Result<(), String>>,
        workers: Vec<thread::JoinHandle<Result<(), E>>>,
        wake_workers: W,
    ) -> Self {
        Self {
            addr,
            shutdown,
            completion,
            workers,
            wake_workers,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(mut self) -> Result<(), E> {
        self.signal_shutdown();
        self.join_workers()
    }

    pub fn wait(mut self) -> Result<(), E> {
        let _ = self.completion.recv();
        self.signal_shutdown();
        self.join_workers()
    }

    fn signal_shutdown(&self) {
        self.shutdown.shutdown();
        (self.wake_workers)(self.addr, self.workers.len());
    }

    fn join_workers(&mut self) -> Result<(), E> {
        let mut first_error: Option<E> = None;
        let addr = self.addr;
        let worker_count = self.workers.len();
        let shutdown = self.shutdown.clone();
        let wake_workers = self.wake_workers;

        for worker in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if first_error.is_none() {
                        shutdown.shutdown();
                        wake_workers(addr, worker_count);
                        first_error = Some(err);
                    }
                }
                Err(panic) => {
                    if first_error.is_none() {
                        shutdown.shutdown();
                        wake_workers(addr, worker_count);
                        first_error = Some(E::from(join_panic_error(panic)));
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

impl<E, W> Drop for ThreadedServerHandle<E, W>
where
    E: From<std::io::Error>,
    W: Fn(SocketAddr, usize) + Copy,
{
    fn drop(&mut self) {
        self.signal_shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.max_connections, 8192);
        assert_eq!(config.max_body_size, 2 * 1024 * 1024);
        assert!(config.worker_count() >= 1);
    }

    #[test]
    fn config_per_worker_max_connections() {
        let config = ServerConfig {
            workers: Some(4),
            ..ServerConfig::default()
        };
        assert_eq!(config.per_worker_max_connections(), 2048);
    }

    #[test]
    fn per_worker_max_connections_rounds_up() {
        let config = ServerConfig {
            max_connections: 8192,
            workers: Some(3),
            ..ServerConfig::default()
        };
        assert_eq!(config.per_worker_max_connections(), 2731);
        assert!(config.per_worker_max_connections() * 3 >= config.max_connections);
    }

    #[test]
    fn shutdown_signal() {
        let signal = ShutdownSignal::new();
        assert!(!signal.is_shutdown());
        signal.shutdown();
        assert!(signal.is_shutdown());
    }

    #[test]
    fn spawn_and_join_workers() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = counter.clone();
        let handles = spawn_workers(4, "test-w", move |_id| {
            c.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        join_workers(handles).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn reuseport_listener_binds() {
        let listener = reuseport_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn resolve_worker_count_rejects_zero() {
        assert!(resolve_worker_count(Some(0)).is_err());
    }

    #[test]
    fn standalone_per_worker_limit_rounds_up() {
        assert_eq!(per_worker_max_connections(10, 3), 4);
    }

    fn wait_for_shutdown(shutdown: &ShutdownSignal) {
        while !shutdown.is_shutdown() {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn start_workers_binds_all_workers_to_the_address_worker_zero_reports() {
        let requested: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bound: SocketAddr = "127.0.0.1:4242".parse().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

        let handle = start_workers::<std::io::Error, _, _>(requested, 3, noop_wake_workers, {
            let seen = Arc::clone(&seen);
            move |ctx: WorkerContext| {
                seen.lock().unwrap().push((ctx.index, ctx.addr));
                ctx.ready
                    .listening(if ctx.index == 0 { bound } else { ctx.addr });
                wait_for_shutdown(&ctx.shutdown);
                Ok(())
            }
        })
        .expect("workers start");

        assert_eq!(handle.local_addr(), bound);
        handle.shutdown().expect("clean shutdown");
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec![(0, requested), (1, bound), (2, bound)]);
    }

    #[test]
    fn start_workers_failure_stops_already_running_workers() {
        let worker0_exited = Arc::new(AtomicBool::new(false));

        let result = start_workers::<std::io::Error, _, _>(
            "127.0.0.1:0".parse().unwrap(),
            2,
            noop_wake_workers,
            {
                let worker0_exited = Arc::clone(&worker0_exited);
                move |ctx: WorkerContext| {
                    if ctx.index == 1 {
                        return Err(std::io::Error::other("bind failed"));
                    }
                    ctx.ready.listening(ctx.addr);
                    wait_for_shutdown(&ctx.shutdown);
                    worker0_exited.store(true, Ordering::Release);
                    Ok(())
                }
            },
        );

        let Err(err) = result else {
            panic!("startup should fail");
        };
        assert_eq!(err.to_string(), "bind failed");
        assert!(
            worker0_exited.load(Ordering::Acquire),
            "worker 0 was not joined"
        );
    }

    #[test]
    fn start_workers_reports_a_worker_that_panics_before_listening() {
        let result = start_workers::<std::io::Error, _, _>(
            "127.0.0.1:0".parse().unwrap(),
            1,
            noop_wake_workers,
            |_ctx: WorkerContext| -> std::io::Result<()> { panic!("boom") },
        );

        let Err(err) = result else {
            panic!("startup should fail");
        };
        assert!(err.to_string().contains("exited before reporting"), "{err}");
    }

    /// Minimal executor: block on `future`, parking the thread between polls.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct ThreadWaker(thread::Thread);
        impl std::task::Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(ThreadWaker(thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            thread::park();
        }
    }

    #[test]
    fn wait_is_woken_by_shutdown_from_another_thread() {
        let signal = ShutdownSignal::new();
        let remote = signal.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            remote.shutdown();
        });
        block_on(signal.wait());
        assert!(signal.is_shutdown());
        trigger.join().unwrap();
    }

    #[test]
    fn wait_after_shutdown_is_ready_immediately() {
        let signal = ShutdownSignal::new();
        signal.shutdown();
        block_on(signal.wait());
    }

    #[test]
    fn dropping_a_pending_wait_unregisters_its_waker() {
        let signal = ShutdownSignal::new();
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        {
            let mut wait = std::pin::pin!(signal.wait());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                signal.lock_waiters().wakers.len(),
                1,
                "re-poll reuses its slot"
            );
        }
        assert!(signal.lock_waiters().wakers.is_empty());
    }
}
