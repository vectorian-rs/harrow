//! Tokio-based HTTP server for Harrow.
//!
//! Uses harrow-codec-h1 for HTTP parsing (no hyper), tokio `current_thread`
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

use harrow_codec_h1::{
    CONTINUE_100, CodecError, MAX_HEADER_BUF, PayloadDecoder, PayloadItem, try_parse_request,
    write_response_head,
};
use harrow_core::dispatch::{self, SharedState};
use harrow_core::route::App;
use harrow_io::BufPool;

pub use harrow_server::ServerConfig;

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
/// independent accept loop. No work-stealing, no cross-thread wakeups.
pub fn serve_multi_worker(
    app: App,
    addr: SocketAddr,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let shared = app.into_shared_state();
    shared.route_table.print_routes();

    let workers = config.worker_count();
    let shutdown = harrow_server::ShutdownSignal::new();

    tracing::info!("harrow listening on {addr} [{workers} workers, SO_REUSEPORT, harrow-codec-h1]");

    let handles = harrow_server::spawn_workers(workers, "harrow-w", {
        let shared = Arc::clone(&shared);
        let shutdown = shutdown.clone();
        let config = config.clone();
        move |worker_id| {
            let shared = Arc::clone(&shared);
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

                worker_loop(shared, listener, &config, shutdown, worker_id).await;
            });
        }
    });

    harrow_server::join_workers(handles).map_err(|e| -> Box<dyn std::error::Error> { e.into() })
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
    let shutdown_signal2 = shutdown_signal.clone();

    tokio::pin!(shutdown);

    tokio::select! {
        () = worker_loop(shared, listener, &config, shutdown_signal, 0) => {}
        () = &mut shutdown => {
            tracing::info!("harrow shutting down");
            shutdown_signal2.shutdown();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Worker internals
// ---------------------------------------------------------------------------

async fn worker_loop(
    shared: Arc<SharedState>,
    listener: TcpListener,
    config: &ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
    worker_id: usize,
) {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let per_worker_max = config.per_worker_max_connections();

    loop {
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

        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(stream, shared, &config2).await {
                tracing::debug!("connection error: {e}");
            }
            active2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
}

async fn handle_tcp_connection(
    stream: TcpStream,
    shared: Arc<SharedState>,
    config: &ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    handle_connection(stream, shared, config).await
}

#[doc(hidden)]
pub async fn handle_connection<S>(
    mut stream: S,
    shared: Arc<SharedState>,
    config: &ServerConfig,
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
                    harrow_codec_h1::encode_chunk_into(&data, &mut head);
                    head.extend_from_slice(harrow_codec_h1::CHUNK_TERMINATOR);
                } else {
                    head.extend_from_slice(harrow_codec_h1::CHUNK_TERMINATOR);
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
