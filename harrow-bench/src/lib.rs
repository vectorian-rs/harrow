//! Shared utilities for Harrow benchmarks.
//!
//! Provides a minimal keep-alive HTTP/1.1 client and server helpers
//! so benchmarks measure framework overhead, not client library cost.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use harrow::{App, Next, Request, Response};

// ---------------------------------------------------------------------------
// Server helpers
// ---------------------------------------------------------------------------

/// Start a Harrow server in the background and return its address.
/// Uses port 0 for OS-assigned ephemeral port.
pub async fn start_server(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        harrow::serve_with_shutdown(app, addr, async { let _ = rx.await; }).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    std::mem::forget(tx);
    addr
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Trivial text handler — minimal work.
pub async fn text_handler(_req: Request) -> Response {
    Response::text("ok")
}

/// JSON echo: reads no body, returns a small JSON payload.
pub async fn json_handler(_req: Request) -> Response {
    Response::json(&serde_json::json!({"status": "ok", "code": 200}))
}

/// Handler that reads a path param and state.
pub async fn param_state_handler(req: Request) -> Response {
    let _id = req.param("id");
    let counter = req.state::<Arc<HitCounter>>();
    counter.0.fetch_add(1, Ordering::Relaxed);
    Response::json(&serde_json::json!({"id": _id, "status": "ok"}))
}

pub struct HitCounter(pub AtomicUsize);

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// No-op passthrough — measures pure middleware chain overhead.
pub async fn noop_middleware(req: Request, next: Next) -> Response {
    next.run(req).await
}

/// Adds a response header — minimal work middleware.
pub async fn header_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-bench", "1")
}

/// Simulates a timing middleware.
pub async fn timing_middleware(req: Request, next: Next) -> Response {
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let _elapsed = start.elapsed();
    resp
}

/// Adds x-group response header — used for group middleware benchmarks.
pub async fn group_tag_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-group", "1")
}

// ---------------------------------------------------------------------------
// Keep-alive HTTP/1.1 client
// ---------------------------------------------------------------------------

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A minimal keep-alive HTTP/1.1 client for benchmarks.
/// Reuses a single TCP connection across requests.
pub struct BenchClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl BenchClient {
    /// Connect to the given address.
    pub async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        stream.set_nodelay(true).unwrap();
        Self {
            stream,
            buf: Vec::with_capacity(4096),
        }
    }

    /// Send a GET request and return the status code and body length.
    /// Uses keep-alive — the connection is reused across calls.
    pub async fn get(&mut self, path: &str) -> (u16, usize) {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n"
        );
        self.stream.write_all(req.as_bytes()).await.unwrap();

        // Read the response headers.
        self.buf.clear();
        let header_end = self.read_until_header_end().await;
        let headers = std::str::from_utf8(&self.buf[..header_end]).unwrap();

        let status = parse_status(headers);
        let body_len = parse_body_length(headers, &self.buf[header_end..]).await;

        // If we haven't read the full body yet, read the rest.
        if let BodyLength::ContentLength(cl) = body_len {
            let already_read = self.buf.len() - header_end;
            if already_read < cl {
                let remaining = cl - already_read;
                let old_len = self.buf.len();
                self.buf.resize(old_len + remaining, 0);
                self.stream
                    .read_exact(&mut self.buf[old_len..])
                    .await
                    .unwrap();
            }
            (status, cl)
        } else if let BodyLength::Chunked = body_len {
            // Read chunked body — collect until we see "0\r\n\r\n".
            let body_start = header_end;
            loop {
                let tail = std::str::from_utf8(&self.buf[body_start..]).unwrap_or("");
                if tail.contains("0\r\n\r\n") || tail.ends_with("0\r\n\r\n") {
                    break;
                }
                let old_len = self.buf.len();
                self.buf.resize(old_len + 1024, 0);
                let n = self.stream.read(&mut self.buf[old_len..]).await.unwrap();
                self.buf.truncate(old_len + n);
                if n == 0 {
                    break;
                }
            }
            let body_bytes = decode_chunked_len(&self.buf[body_start..]);
            (status, body_bytes)
        } else {
            (status, 0)
        }
    }

    /// Read from the stream until we find `\r\n\r\n`, returning the byte
    /// offset of the end of headers (pointing to the first body byte).
    async fn read_until_header_end(&mut self) -> usize {
        loop {
            let old_len = self.buf.len();
            self.buf.resize(old_len + 1024, 0);
            let n = self.stream.read(&mut self.buf[old_len..]).await.unwrap();
            self.buf.truncate(old_len + n);

            if let Some(pos) = find_header_end(&self.buf) {
                return pos;
            }
            if n == 0 {
                return self.buf.len();
            }
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn parse_status(headers: &str) -> u16 {
    headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

enum BodyLength {
    ContentLength(usize),
    Chunked,
    Unknown,
}

async fn parse_body_length(headers: &str, _extra: &[u8]) -> BodyLength {
    for line in headers.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            if let Some(val) = lower.strip_prefix("content-length:") {
                if let Ok(n) = val.trim().parse() {
                    return BodyLength::ContentLength(n);
                }
            }
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            return BodyLength::Chunked;
        }
    }
    BodyLength::Unknown
}

fn decode_chunked_len(raw: &[u8]) -> usize {
    let s = std::str::from_utf8(raw).unwrap_or("");
    let mut total = 0;
    let mut remaining = s;
    loop {
        let (size_str, rest) = match remaining.split_once("\r\n") {
            Some(pair) => pair,
            None => break,
        };
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        total += size;
        if rest.len() < size + 2 {
            break;
        }
        remaining = &rest[size + 2..]; // skip data + \r\n
    }
    total
}

// ---------------------------------------------------------------------------
// Route table builders
// ---------------------------------------------------------------------------

/// Build a route table with `n` routes. The last route matches `/target/:id`.
/// All others are decoys like `/decoy-0`, `/decoy-1`, etc.
pub fn build_app_with_routes(n: usize) -> App {
    let mut app = App::new();
    for i in 0..n.saturating_sub(1) {
        let pattern = format!("/decoy-{i}");
        // We need to leak the string to get a &'static str for the route pattern.
        // This is fine for benchmarks.
        let pattern: &'static str = Box::leak(pattern.into_boxed_str());
        app = app.get(pattern, text_handler);
    }
    app = app.get("/target/:id", text_handler);
    app
}
