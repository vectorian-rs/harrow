#[cfg(feature = "ws")]
pub mod ws;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Sleep, sleep};

use harrow_core::dispatch::dispatch;
use harrow_core::request::{Body, box_incoming};
use harrow_core::route::App;

// Wraps a hyper `Incoming` body with a per-frame read timeout.
// Each call to `poll_frame` resets a deadline. If no frame arrives
// within the timeout, the body returns an error.
pin_project_lite::pin_project! {
    struct TimeoutBody {
        #[pin]
        inner: Incoming,
        timeout: Duration,
        #[pin]
        deadline: Sleep,
    }
}

impl TimeoutBody {
    fn new(inner: Incoming, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: sleep(timeout),
            timeout,
        }
    }
}

impl HttpBody for TimeoutBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();

        // Check if the body has a frame ready.
        match this.inner.poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // Got a frame — reset deadline for the next one.
                this.deadline
                    .reset(tokio::time::Instant::now() + *this.timeout);
                Poll::Ready(Some(Ok(frame.map_data(Bytes::from))))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(
                Box::new(e) as Box<dyn std::error::Error + Send + Sync>
            ))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                // Body not ready — check if deadline expired.
                match this.deadline.poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Some(Err("body read timeout".into()))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Convert a `hyper::body::Incoming` into a harrow `Body` with a read timeout.
fn box_incoming_with_timeout(incoming: Incoming, timeout: Duration) -> Body {
    use http_body_util::BodyExt;
    TimeoutBody::new(incoming, timeout).boxed()
}

/// Configuration for server connection handling.
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Timeout for reading HTTP headers from a new connection. Default: Some(5s).
    /// Set to `None` to disable (eliminates per-connection timer overhead).
    pub header_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection. Default: Some(5 min).
    /// Set to `None` to disable (eliminates per-connection timer overhead).
    pub connection_timeout: Option<Duration>,
    /// Timeout for reading each body frame from the client. Default: None (disabled).
    /// Set to `Some(duration)` to protect against slow body senders.
    /// When disabled, the raw non-timeout body path is used with zero overhead.
    pub body_read_timeout: Option<Duration>,
    /// Time to wait for in-flight requests to complete during shutdown. Default: 30s.
    pub drain_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8192,
            header_read_timeout: Some(Duration::from_secs(5)),
            connection_timeout: Some(Duration::from_secs(300)),
            body_read_timeout: None,
            drain_timeout: Duration::from_secs(30),
        }
    }
}

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
/// independent accept loop. This eliminates cross-thread task scheduling
/// overhead and keeps connections pinned to the core that accepted them.
///
/// This function blocks the calling thread until shutdown. Call it from
/// `main()` — not from inside an async runtime.
///
/// ```no_run
/// let app = harrow_core::route::App::new();
/// let addr = "0.0.0.0:3090".parse().unwrap();
/// harrow_server_tokio::serve_multi_worker(app, addr, Default::default());
/// ```
pub fn serve_multi_worker(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let shutdown = Arc::new(AtomicBool::new(false));

    tracing::info!("harrow listening on {addr} [{workers} workers, SO_REUSEPORT]");

    let mut handles = Vec::with_capacity(workers);
    for worker_id in 0..workers {
        let shared = Arc::clone(&shared);
        let shutdown = Arc::clone(&shutdown);
        let config_max = config.max_connections / workers;
        let header_read_timeout = config.header_read_timeout;
        let connection_timeout = config.connection_timeout;
        let body_read_timeout = config.body_read_timeout;

        let handle = std::thread::Builder::new()
            .name(format!("harrow-w{worker_id}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        reuseport_listener(addr).expect("failed to bind SO_REUSEPORT listener");

                    let semaphore = Arc::new(Semaphore::new(config_max));
                    let mut connections: JoinSet<()> = JoinSet::new();

                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }

                        while connections.try_join_next().is_some() {}

                        let result = tokio::select! {
                            r = listener.accept() => r,
                            () = tokio::time::sleep(Duration::from_millis(100)) => {
                                if shutdown.load(Ordering::Relaxed) { break; }
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

                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                drop(stream);
                                continue;
                            }
                        };

                        let io = TokioIo::new(stream);
                        let shared = Arc::clone(&shared);

                        connections.spawn(async move {
                            let _permit = permit;

                            let service = service_fn(move |req: http::Request<Incoming>| {
                                let shared = Arc::clone(&shared);
                                async move {
                                    let boxed = if let Some(brt) = body_read_timeout {
                                        req.map(|body| box_incoming_with_timeout(body, brt))
                                    } else {
                                        req.map(box_incoming)
                                    };
                                    Ok::<_, std::convert::Infallible>(dispatch(shared, boxed).await)
                                }
                            });

                            let mut builder = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            );
                            if let Some(hrt) = header_read_timeout {
                                builder
                                    .http1()
                                    .timer(hyper_util::rt::TokioTimer::new())
                                    .header_read_timeout(hrt);
                            }
                            let conn = builder
                                .serve_connection_with_upgrades(io, service)
                                .into_owned();

                            if let Some(ct) = connection_timeout {
                                match tokio::time::timeout(ct, conn).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => {
                                        tracing::error!("connection error: {e}")
                                    }
                                    Err(_) => {
                                        tracing::warn!("connection timed out")
                                    }
                                }
                            } else if let Err(e) = conn.await {
                                tracing::error!("connection error: {e}");
                            }
                        });
                    }

                    // Drain
                    while connections.join_next().await.is_some() {}
                });
            })?;

        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("worker thread panicked");
    }

    Ok(())
}

/// Create a `TcpListener` with `SO_REUSEPORT` set before binding.
fn reuseport_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
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

    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
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
    tracing::info!("harrow listening on {addr}");

    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections: JoinSet<()> = JoinSet::new();

    tokio::pin!(shutdown);

    loop {
        // Reap completed tasks before accepting new ones.
        while connections.try_join_next().is_some() {}

        tokio::select! {
            result = listener.accept() => {
                let (stream, _remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                        continue;
                    }
                };

                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        // At connection limit — drop the TCP stream immediately.
                        drop(stream);
                        tracing::warn!("connection limit reached, dropping new connection");
                        continue;
                    }
                };

                let io = TokioIo::new(stream);
                let shared = Arc::clone(&shared);
                let header_read_timeout = config.header_read_timeout;
                let connection_timeout = config.connection_timeout;
                let body_read_timeout = config.body_read_timeout;

                connections.spawn(async move {
                    let _permit = permit; // held until task completes

                    let service = service_fn(move |req: http::Request<Incoming>| {
                        let shared = Arc::clone(&shared);
                        async move {
                            let boxed = if let Some(brt) = body_read_timeout {
                                req.map(|body| box_incoming_with_timeout(body, brt))
                            } else {
                                req.map(box_incoming)
                            };
                            Ok::<_, std::convert::Infallible>(dispatch(shared, boxed).await)
                        }
                    });

                    let mut builder = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Some(hrt) = header_read_timeout {
                        builder.http1()
                            .timer(hyper_util::rt::TokioTimer::new())
                            .header_read_timeout(hrt);
                    }
                    // Always use serve_connection_with_upgrades — it's a
                    // strict superset with zero overhead for non-upgrade
                    // connections. into_owned() is needed because the
                    // UpgradeableConnection borrows the builder.
                    let conn = builder
                        .serve_connection_with_upgrades(io, service)
                        .into_owned();

                    if let Some(ct) = connection_timeout {
                        match tokio::time::timeout(ct, conn).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => tracing::error!("connection error: {e}"),
                            Err(_) => tracing::warn!("connection timed out"),
                        }
                    } else if let Err(e) = conn.await {
                        tracing::error!("connection error: {e}");
                    }
                });
            }
            () = &mut shutdown => {
                tracing::info!("harrow shutting down");
                break;
            }
        }
    }

    // Graceful drain: wait for in-flight connections to complete.
    match tokio::time::timeout(config.drain_timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    {
        Ok(()) => tracing::info!("all connections drained"),
        Err(_) => {
            tracing::warn!(
                "drain timeout ({}s) exceeded, aborting remaining connections",
                config.drain_timeout.as_secs()
            );
            connections.abort_all();
        }
    }

    Ok(())
}
