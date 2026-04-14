//! Tokio-based HTTP server for Harrow.
//!
//! Uses harrow-codec for HTTP parsing (no hyper), tokio `current_thread`
//! per worker (no work-stealing), and thread-local buffer pooling
//! (no per-request allocation).

#[cfg(feature = "ws")]
pub mod ws;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Buf;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use harrow_codec::{
    BufPool, CONTINUE_100, CodecError, MAX_HEADER_BUF, PayloadDecoder, PayloadItem,
    try_parse_request, write_response_head,
};
use harrow_core::dispatch::{self, SharedState};
use harrow_core::route::App;

/// Configuration for server connection handling.
pub struct ServerConfig {
    /// Maximum number of concurrent connections per worker. Default: 8192.
    pub max_connections: usize,
    /// Timeout for reading HTTP headers. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Maximum connection lifetime. Default: Some(5 min).
    pub connection_timeout: Option<Duration>,
    /// Timeout for reading request body. Default: Some(30s).
    pub body_read_timeout: Option<Duration>,
    /// Drain timeout during shutdown. Default: 30s.
    pub drain_timeout: Duration,
    /// Maximum request body size. Default: 2 MiB.
    pub max_body_size: usize,
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
/// independent accept loop with `LocalSet`. No work-stealing, no
/// cross-thread wakeups.
pub fn serve_multi_worker(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::AtomicBool;

    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let shutdown = Arc::new(AtomicBool::new(false));

    tracing::info!("harrow listening on {addr} [{workers} workers, SO_REUSEPORT, harrow-codec]");

    let per_worker_max = config.max_connections / workers.max(1);
    let mut handles = Vec::with_capacity(workers);

    for worker_id in 0..workers {
        let shared = Arc::clone(&shared);
        let shutdown = Arc::clone(&shutdown);
        let worker_config = WorkerConfig {
            max_connections: per_worker_max,
            header_read_timeout: config.header_read_timeout,
            connection_timeout: config.connection_timeout,
            body_read_timeout: config.body_read_timeout,
            max_body_size: config.max_body_size,
        };

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

                    worker_loop(shared, listener, worker_config, shutdown, worker_id).await;
                });
            })?;

        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("worker thread panicked");
    }

    Ok(())
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
    tracing::info!("harrow listening on {addr} [harrow-codec]");

    let worker_config = WorkerConfig {
        max_connections: config.max_connections,
        header_read_timeout: config.header_read_timeout,
        connection_timeout: config.connection_timeout,
        body_read_timeout: config.body_read_timeout,
        max_body_size: config.max_body_size,
    };

    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag2 = Arc::clone(&shutdown_flag);

    tokio::pin!(shutdown);

    tokio::select! {
        () = worker_loop(shared, listener, worker_config, shutdown_flag, 0) => {}
        () = &mut shutdown => {
            tracing::info!("harrow shutting down");
            shutdown_flag2.store(true, std::sync::atomic::Ordering::Release);
        }
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

// ---------------------------------------------------------------------------
// Worker internals
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[doc(hidden)]
pub struct WorkerConfig {
    pub max_connections: usize,
    pub header_read_timeout: Option<Duration>,
    pub connection_timeout: Option<Duration>,
    pub body_read_timeout: Option<Duration>,
    pub max_body_size: usize,
}

async fn worker_loop(
    shared: Arc<SharedState>,
    listener: TcpListener,
    config: WorkerConfig,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    worker_id: usize,
) {
    use std::sync::atomic::Ordering;

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let result = tokio::select! {
            r = listener.accept() => r,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                if shutdown.load(Ordering::Acquire) { break; }
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

        if active.load(Ordering::Relaxed) >= config.max_connections {
            drop(stream);
            continue;
        }

        active.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::clone(&shared);
        let config2 = config.clone();
        let active2 = Arc::clone(&active);

        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(stream, shared, &config2).await {
                tracing::debug!("connection error: {e}");
            }
            active2.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn handle_tcp_connection(
    stream: TcpStream,
    shared: Arc<SharedState>,
    config: &WorkerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    handle_connection(stream, shared, config).await
}

#[doc(hidden)]
pub async fn handle_connection<S>(
    mut stream: S,
    shared: Arc<SharedState>,
    config: &WorkerConfig,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let accepted_at = Instant::now();
    let mut buf = BufPool::acquire_read();

    'connection: loop {
        // Check connection lifetime.
        if let Some(max_life) = config.connection_timeout
            && accepted_at.elapsed() >= max_life
        {
            break;
        }

        let request_started = Instant::now();

        // --- Read headers ---
        // Try buffered data first (handles pipelined keep-alive requests).
        let parsed = loop {
            match try_parse_request(&buf) {
                Ok(parsed) => break parsed,
                Err(CodecError::Incomplete) => {
                    if buf.len() >= MAX_HEADER_BUF {
                        write_error(&mut stream, 400, "request headers too large").await;
                        break 'connection;
                    }
                }
                Err(CodecError::Invalid(_)) => {
                    write_error(&mut stream, 400, "bad request").await;
                    break 'connection;
                }
                Err(CodecError::BodyTooLarge) => {
                    write_error(&mut stream, 413, "payload too large").await;
                    break 'connection;
                }
            }

            // Need more data — read from socket.
            if let Some(timeout) = config.header_read_timeout {
                let remaining = timeout.saturating_sub(request_started.elapsed());
                if remaining.is_zero() {
                    break 'connection;
                }
                match tokio::time::timeout(remaining, stream.read_buf(&mut buf)).await {
                    Ok(Ok(0)) => break 'connection,
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => break 'connection,
                    Err(_) => break 'connection,
                }
            } else {
                match stream.read_buf(&mut buf).await {
                    Ok(0) => break 'connection,
                    Ok(_) => {}
                    Err(_) => break 'connection,
                }
            }
        };

        let header_len = parsed.header_len;
        let keep_alive = parsed.keep_alive;
        let expect_continue = parsed.expect_continue;
        let content_length = parsed.content_length;

        // Early reject: Content-Length exceeds limit.
        if config.max_body_size > 0
            && let Some(cl) = content_length
            && cl as usize > config.max_body_size
        {
            write_error(&mut stream, 413, "payload too large").await;
            break;
        }

        // Consume header bytes; remainder is body data.
        buf.advance(header_len);

        // --- Read body ---
        let body_bytes = if let Some(mut decoder) = PayloadDecoder::from_parsed(&parsed) {
            // Send 100-continue if the client expects it.
            if expect_continue {
                let _ = stream.write_all(CONTINUE_100).await;
            }

            let mut body = Vec::new();
            let body_deadline = config.body_read_timeout.map(|t| Instant::now() + t);

            loop {
                // Feed buffered data to the decoder.
                match decoder.decode(&mut buf, Some(config.max_body_size)) {
                    Err(CodecError::BodyTooLarge) => {
                        write_error(&mut stream, 413, "payload too large").await;
                        break 'connection;
                    }
                    Err(CodecError::Invalid(_)) => {
                        write_error(&mut stream, 400, "bad request").await;
                        break 'connection;
                    }
                    Err(CodecError::Incomplete) => {
                        // Should not happen from PayloadDecoder, but handle gracefully.
                        break 'connection;
                    }
                    Ok(Some(PayloadItem::Chunk(chunk))) => {
                        body.extend_from_slice(&chunk);
                        continue;
                    }
                    Ok(Some(PayloadItem::Eof)) => break,
                    Ok(None) => {}
                }

                // Need more data — read from socket.
                if let Some(deadline) = body_deadline {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        write_error(&mut stream, 408, "request timeout").await;
                        break 'connection;
                    }
                    match tokio::time::timeout(remaining, stream.read_buf(&mut buf)).await {
                        Ok(Ok(0)) => break 'connection,
                        Ok(Ok(_)) => {}
                        Ok(Err(_)) => break 'connection,
                        Err(_) => {
                            write_error(&mut stream, 408, "request timeout").await;
                            break 'connection;
                        }
                    }
                } else {
                    match stream.read_buf(&mut buf).await {
                        Ok(0) => break 'connection,
                        Ok(_) => {}
                        Err(_) => break 'connection,
                    }
                }
            }

            bytes::Bytes::from(body)
        } else {
            bytes::Bytes::new()
        };

        // --- Build harrow request ---
        let mut builder = http::Request::builder()
            .method(parsed.method)
            .uri(parsed.uri)
            .version(parsed.version);

        for (name, value) in parsed.headers.iter() {
            builder = builder.header(name, value);
        }

        let harrow_body: harrow_core::request::Body = {
            http_body_util::Full::new(body_bytes)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
                .boxed()
        };

        let request = builder.body(harrow_body)?;

        // --- Dispatch through harrow pipeline ---
        let response = dispatch::dispatch(Arc::clone(&shared), request).await;

        // --- Serialize and write response ---
        let (mut parts, response_body) = response.into_parts();
        let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);

        if !keep_alive && !parts.headers.contains_key(http::header::CONNECTION) {
            parts
                .headers
                .insert(http::header::CONNECTION, "close".parse().unwrap());
        }

        let chunked = !has_content_length;
        let mut head = write_response_head(parts.status, &parts.headers, chunked);

        // Collect body.
        let body_result = response_body.collect().await;
        match body_result {
            Ok(collected) => {
                let data = collected.to_bytes();
                if has_content_length {
                    head.extend_from_slice(&data);
                } else if !data.is_empty() {
                    harrow_codec::encode_chunk_into(&data, &mut head);
                    head.extend_from_slice(harrow_codec::CHUNK_TERMINATOR);
                } else {
                    head.extend_from_slice(harrow_codec::CHUNK_TERMINATOR);
                }
            }
            Err(_) => {
                head.clear();
                head.extend_from_slice(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 21\r\nconnection: close\r\n\r\ninternal server error",
                );
            }
        }

        // Write response.
        if stream.write_all(&head).await.is_err() {
            break;
        }

        if !keep_alive {
            break;
        }

        // Keep-alive: loop for next request.
        // buf may still have pipelined data from the read phase.
    }

    BufPool::release_read(buf);
    Ok(())
}

async fn write_error<S: tokio::io::AsyncWrite + Unpin>(stream: &mut S, status: u16, body: &str) {
    let resp = match (status, body) {
        (400, "bad request") => &b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad request"[..],
        (400, "request headers too large") => &b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 26\r\nconnection: close\r\n\r\nrequest headers too large"[..],
        (408, "request timeout") => &b"HTTP/1.1 408 Request Timeout\r\ncontent-type: text/plain\r\ncontent-length: 15\r\nconnection: close\r\n\r\nrequest timeout"[..],
        (413, "payload too large") => &b"HTTP/1.1 413 Payload Too Large\r\ncontent-type: text/plain\r\ncontent-length: 17\r\nconnection: close\r\n\r\npayload too large"[..],
        _ => {
            // Fallback for unknown status/body combinations.
            let formatted = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}",
                reason = http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Error"),
                len = body.len(),
            );
            let _ = stream.write_all(formatted.as_bytes()).await;
            return;
        }
    };
    let _ = stream.write_all(resp).await;
}
