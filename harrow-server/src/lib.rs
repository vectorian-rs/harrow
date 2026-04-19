//! Shared server primitives for Harrow.
//!
//! Runtime-agnostic building blocks used by all server backends
//! (tokio, monoio, meguri): listener creation, worker spawning,
//! shutdown coordination, and configuration.

pub mod h1;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
#[derive(Clone)]
pub struct ShutdownSignal {
    flag: Arc<AtomicBool>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal shutdown. All workers will observe this on their next check.
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Check if shutdown has been signaled.
    pub fn is_shutdown(&self) -> bool {
        self.flag.load(Ordering::Acquire)
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
    fn per_worker_max_connections() {
        let mut config = ServerConfig::default();
        config.workers = Some(4);
        assert_eq!(config.per_worker_max_connections(), 2048);
    }

    #[test]
    fn per_worker_max_connections_rounds_up() {
        let mut config = ServerConfig::default();
        config.max_connections = 8192;
        config.workers = Some(3);
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
}
