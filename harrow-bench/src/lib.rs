//! Shared utilities for Harrow benchmarks.
//!
//! Provides a minimal keep-alive HTTP/1.1 client and server helpers
//! so benchmarks measure framework overhead, not client library cost.

pub mod harness;
pub mod perf_summary;

/// Parse `--bind ADDR` and `--port PORT` CLI args common to all perf server binaries.
pub fn parse_bind_port() -> (String, u16) {
    let args: Vec<String> = std::env::args().collect();
    let bin_name = args
        .first()
        .and_then(|s| s.rsplit('/').next())
        .unwrap_or("perf-server");
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 3090;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args.get(i + 1).expect("--bind requires an address").clone();
                i += 2;
            }
            "--port" => {
                port = args
                    .get(i + 1)
                    .expect("--port requires a number")
                    .parse()
                    .expect("invalid port number");
                i += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: {bin_name} [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    (bind, port)
}

/// Set up the global allocator and define `ALLOCATOR_NAME`.
///
/// Usage: put `harrow_bench::setup_allocator!();` at the top of each perf server binary.
#[macro_export]
macro_rules! setup_allocator {
    () => {
        #[cfg(all(feature = "mimalloc", feature = "jemalloc"))]
        compile_error!("features `mimalloc` and `jemalloc` are mutually exclusive");

        #[cfg(feature = "mimalloc")]
        #[global_allocator]
        static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

        #[cfg(all(feature = "jemalloc", not(feature = "mimalloc")))]
        #[global_allocator]
        static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

        const ALLOCATOR_NAME: &str = if cfg!(feature = "mimalloc") {
            "mimalloc"
        } else if cfg!(feature = "jemalloc") {
            "jemalloc"
        } else {
            "system"
        };
    };
}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow::{App, Next, Request, Response, SessionStore};
use serde::{Deserialize, Serialize};

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
        harrow::runtime::tokio::serve_with_shutdown(app, addr, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
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
    let counter = req.require_state::<Arc<HitCounter>>().unwrap();
    counter.0.fetch_add(1, Ordering::Relaxed);
    Response::json(&serde_json::json!({"id": _id, "status": "ok"}))
}

pub struct HitCounter(pub AtomicUsize);

/// ~1KB JSON: object with array of 10 user objects.
pub static JSON_1KB: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "users": (0..10).map(|i| serde_json::json!({
            "id": i,
            "name": format!("User {i}"),
            "email": format!("user{i}@example.com"),
            "active": i % 2 == 0,
            "score": i * 17 + 42,
            "tags": ["bench", "test", "user"]
        })).collect::<Vec<_>>()
    })
});

/// ~10KB JSON: object with array of 100 user objects.
pub static JSON_10KB: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "users": (0..100).map(|i| serde_json::json!({
            "id": i,
            "name": format!("User {i}"),
            "email": format!("user{i}@example.com"),
            "active": i % 2 == 0,
            "score": i * 17 + 42,
            "tags": ["bench", "test", "user"]
        })).collect::<Vec<_>>()
    })
});

/// JSON 1KB handler — returns a ~1KB JSON payload.
pub async fn json_1kb_handler(_req: Request) -> Response {
    Response::json(&*JSON_1KB)
}

/// JSON 10KB handler — returns a ~10KB JSON payload.
pub async fn json_10kb_handler(_req: Request) -> Response {
    Response::json(&*JSON_10KB)
}

/// Simulates a handler that does async I/O (e.g. DB query).
/// Returns JSON 1KB after a 100µs sleep.
pub async fn simulated_io_handler(_req: Request) -> Response {
    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
    Response::json(&*JSON_1KB)
}

// ---------------------------------------------------------------------------
// Typed payloads for serde benchmarks (JSON + MessagePack)
// ---------------------------------------------------------------------------

/// Typed user struct — identical data for JSON and MessagePack serialisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: u32,
    pub name: String,
    pub email: String,
    pub active: bool,
    pub score: u32,
    pub tags: Vec<String>,
}

/// Small payload: ~100B JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmallPayload {
    pub status: String,
    pub code: u32,
}

pub static SMALL_PAYLOAD: LazyLock<SmallPayload> = LazyLock::new(|| SmallPayload {
    status: "ok".into(),
    code: 200,
});

fn make_users(n: u32) -> Vec<User> {
    (0..n)
        .map(|i| User {
            id: i,
            name: format!("User {i}"),
            email: format!("user{i}@example.com"),
            active: i % 2 == 0,
            score: i * 17 + 42,
            tags: vec!["bench".into(), "test".into(), "user".into()],
        })
        .collect()
}

/// 10 user objects — ~1KB when JSON-encoded.
pub static USERS_10: LazyLock<Vec<User>> = LazyLock::new(|| make_users(10));

/// 100 user objects — ~10KB when JSON-encoded.
pub static USERS_100: LazyLock<Vec<User>> = LazyLock::new(|| make_users(100));

// --- Harrow handlers for serde benchmarks ---

pub async fn json_small_handler(_req: Request) -> Response {
    Response::json(&*SMALL_PAYLOAD)
}

pub async fn json_1kb_typed_handler(_req: Request) -> Response {
    Response::json(&*USERS_10)
}

pub async fn json_10kb_typed_handler(_req: Request) -> Response {
    Response::json(&*USERS_100)
}

pub async fn msgpack_small_handler(_req: Request) -> Response {
    Response::msgpack(&*SMALL_PAYLOAD)
}

pub async fn msgpack_1kb_handler(_req: Request) -> Response {
    Response::msgpack(&*USERS_10)
}

pub async fn msgpack_10kb_handler(_req: Request) -> Response {
    Response::msgpack(&*USERS_100)
}

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
// Session benchmark helpers
// ---------------------------------------------------------------------------

pub const BENCH_SESSION_SECRET: [u8; 32] = *b"harrow-bench-session-secret-key!";
pub const BENCH_SESSION_ID: &str = "00000000000000001111111111111111";
pub const BODY_1KB_TEXT: &str = include_str!("../benches/body_1kb.txt");

pub fn bench_session_cookie() -> String {
    let mac = blake3::keyed_hash(&BENCH_SESSION_SECRET, BENCH_SESSION_ID.as_bytes());
    format!("sid={BENCH_SESSION_ID}.{}", mac.to_hex())
}

pub fn bench_session_config() -> harrow::SessionConfig {
    harrow::SessionConfig::new(BENCH_SESSION_SECRET).secure(false)
}

pub async fn seed_bench_session<S: SessionStore>(store: &S) {
    let mut data = HashMap::new();
    data.insert("user".to_string(), "bench".to_string());
    store
        .save(BENCH_SESSION_ID, &data, Duration::from_secs(86400))
        .await;
}

pub async fn large_text_handler(_req: Request) -> Response {
    Response::text(BODY_1KB_TEXT)
}

static TEXT_128KB: LazyLock<String> = LazyLock::new(|| "A".repeat(128 * 1024));
static TEXT_256KB: LazyLock<String> = LazyLock::new(|| "B".repeat(256 * 1024));
static TEXT_512KB: LazyLock<String> = LazyLock::new(|| "C".repeat(512 * 1024));
static TEXT_1MB: LazyLock<String> = LazyLock::new(|| "D".repeat(1024 * 1024));

pub async fn text_128kb_handler(_req: Request) -> Response {
    Response::text(TEXT_128KB.as_str())
}

pub async fn text_256kb_handler(_req: Request) -> Response {
    Response::text(TEXT_256KB.as_str())
}

pub async fn text_512kb_handler(_req: Request) -> Response {
    Response::text(TEXT_512KB.as_str())
}

pub async fn text_1mb_handler(_req: Request) -> Response {
    Response::text(TEXT_1MB.as_str())
}

/// Echo POST body — reads full body and returns it.
pub async fn echo_body_handler(req: Request) -> Response {
    match req.body_bytes().await {
        Ok(bytes) => Response::new(http::StatusCode::OK, bytes),
        Err(e) => Response::text(e.to_string()).status(400),
    }
}

/// Handler that runs behind session middleware but does NOT access the session.
/// Measures pure session middleware overhead (cookie parse + store lookup).
pub async fn session_noop_handler(_req: Request) -> Response {
    Response::text("ok")
}

pub async fn session_set_handler(req: Request) -> Response {
    let session = req.ext::<harrow::Session>().unwrap();
    session.set("user", "bench");
    Response::text("ok")
}

pub async fn session_get_handler(req: Request) -> Response {
    let session = req.ext::<harrow::Session>().unwrap();
    let _ = session.get("user");
    Response::text("ok")
}

pub async fn session_write_handler(req: Request) -> Response {
    let session = req.ext::<harrow::Session>().unwrap();
    let _ = session.get("user");
    session.set("counter", "1");
    Response::text("ok")
}

pub async fn session_large_get_handler(req: Request) -> Response {
    let session = req.ext::<harrow::Session>().unwrap();
    let _ = session.get("user");
    Response::text(BODY_1KB_TEXT)
}

pub async fn session_large_write_handler(req: Request) -> Response {
    let session = req.ext::<harrow::Session>().unwrap();
    let _ = session.get("user");
    session.set("counter", "1");
    Response::text(BODY_1KB_TEXT)
}

// ---------------------------------------------------------------------------
// Concurrent connection helper
// ---------------------------------------------------------------------------

/// Run N concurrent keep-alive connections, each sending `reqs_per_conn`
/// requests before closing. Models realistic traffic where clients maintain
/// persistent connections and pipeline multiple requests.
pub async fn run_concurrent(
    addr: SocketAddr,
    path: &str,
    concurrency: usize,
    reqs_per_conn: usize,
) {
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let path = path.to_string();
        let handle = tokio::spawn(async move {
            let mut client = BenchClient::connect(addr).await;
            for _ in 0..reqs_per_conn {
                let (status, _) = client.get(&path).await;
                debug_assert!(status == 200 || status == 404);
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.await.unwrap();
    }
}

/// Run N concurrent keep-alive connections against a mixed set of routes.
/// Each connection cycles through `paths` round-robin, sending `reqs_per_conn`
/// total requests. Models real traffic with varied endpoint weights.
pub async fn run_concurrent_mixed(
    addr: SocketAddr,
    paths: &[&str],
    concurrency: usize,
    reqs_per_conn: usize,
) {
    let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    let mut handles = Vec::with_capacity(concurrency);
    for conn_id in 0..concurrency {
        let paths = paths.clone();
        let handle = tokio::spawn(async move {
            let mut client = BenchClient::connect(addr).await;
            for i in 0..reqs_per_conn {
                let path = &paths[(conn_id + i) % paths.len()];
                let (status, _) = client.get(path).await;
                debug_assert!(status == 200 || status == 404);
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.await.unwrap();
    }
}

/// Run N concurrent keep-alive connections with extra headers on each request.
pub async fn run_concurrent_with_headers(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    concurrency: usize,
    reqs_per_conn: usize,
) {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let path = path.to_string();
        let headers = headers.clone();
        let handle = tokio::spawn(async move {
            let mut client = BenchClient::connect(addr).await;
            let h: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            for _ in 0..reqs_per_conn {
                let (status, _) = client.get_with_headers(&path, &h).await;
                debug_assert!(status == 200 || status == 204 || status == 404);
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.await.unwrap();
    }
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
        // Avoid TIME_WAIT buildup: RST on close instead of FIN handshake.
        // Tokio deprecated this but Duration::ZERO means no blocking.
        #[allow(deprecated)]
        stream.set_linger(Some(std::time::Duration::ZERO)).unwrap();
        Self {
            stream,
            buf: Vec::with_capacity(4096),
        }
    }

    /// Send a GET request with extra headers and return the status code and body length.
    /// Uses keep-alive — the connection is reused across calls.
    pub async fn get_with_headers(&mut self, path: &str, headers: &[(&str, &str)]) -> (u16, usize) {
        let mut req =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n");
        for &(name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        req.push_str("\r\n");
        self.stream.write_all(req.as_bytes()).await.unwrap();

        self.buf.clear();
        let header_end = self.read_until_header_end().await;
        let headers_str = std::str::from_utf8(&self.buf[..header_end]).unwrap();

        let status = parse_status(headers_str);
        let body_len = parse_body_length(headers_str, &self.buf[header_end..]).await;

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

    /// Send a GET request and return the status code and body length.
    /// Uses keep-alive — the connection is reused across calls.
    pub async fn get(&mut self, path: &str) -> (u16, usize) {
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n");
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
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
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
        if let Some(val) = lower.strip_prefix("content-length:")
            && let Ok(n) = val.trim().parse()
        {
            return BodyLength::ContentLength(n);
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
    while let Some((size_str, rest)) = remaining.split_once("\r\n") {
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

// ---------------------------------------------------------------------------
// Test-only in-memory stores (not for production use)
// ---------------------------------------------------------------------------

use std::sync::RwLock;
use std::time::Instant as StdInstant;

struct SessionEntry {
    data: HashMap<String, String>,
    expires_at: StdInstant,
}

/// Simple in-memory session store for testing and benchmarks only.
/// Not suitable for production — no sweeper, no size limit, single-node only.
pub struct InMemorySessionStore {
    sessions: Arc<RwLock<HashMap<String, SessionEntry>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl harrow::SessionStore for InMemorySessionStore {
    async fn load(&self, id: &str) -> Option<HashMap<String, String>> {
        let sessions = self.sessions.read().unwrap();
        let entry = sessions.get(id)?;
        if entry.expires_at <= StdInstant::now() {
            return None;
        }
        Some(entry.data.clone())
    }

    async fn save(&self, id: &str, data: &HashMap<String, String>, ttl: Duration) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.insert(
            id.to_string(),
            SessionEntry {
                data: data.clone(),
                expires_at: StdInstant::now() + ttl,
            },
        );
    }

    async fn remove(&self, id: &str) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.remove(id);
    }
}

// ---------------------------------------------------------------------------
// In-memory GCRA rate-limit backend (not for production use)
// ---------------------------------------------------------------------------

use std::sync::atomic::AtomicU64;

static EPOCH: LazyLock<StdInstant> = LazyLock::new(StdInstant::now);

fn now_ns() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

/// In-memory GCRA rate-limit backend for testing and benchmarks only.
/// Not suitable for production — no sweeper, no size limit, single-node only.
pub struct InMemoryBackend {
    states: Arc<dashmap::DashMap<String, AtomicU64>>,
    t_ns: u64,
    tau_ns: u64,
    limit: u64,
    burst: u64,
}

impl InMemoryBackend {
    pub fn per_second(rate: u64) -> Self {
        let t_ns = 1_000_000_000 / rate;
        Self {
            states: Arc::new(dashmap::DashMap::new()),
            t_ns,
            tau_ns: (rate - 1) * t_ns,
            limit: rate,
            burst: rate,
        }
    }

    pub fn per_minute(rate: u64) -> Self {
        let t_ns = 60_000_000_000 / rate;
        Self {
            states: Arc::new(dashmap::DashMap::new()),
            t_ns,
            tau_ns: (rate - 1) * t_ns,
            limit: rate,
            burst: rate,
        }
    }

    pub fn burst(mut self, burst: u64) -> Self {
        self.burst = burst;
        self.tau_ns = (burst - 1) * self.t_ns;
        self
    }

    fn gcra_check(&self, tat: &AtomicU64, now: u64) -> harrow::RateLimitOutcome {
        loop {
            let old_tat = tat.load(Ordering::Relaxed);
            let tat_val = if old_tat == 0 { now } else { old_tat };
            if now < tat_val.saturating_sub(self.tau_ns) {
                let retry_after_ns = tat_val.saturating_sub(self.tau_ns) - now;
                let reset_after_ns = tat_val.saturating_sub(now);
                return harrow::RateLimitOutcome {
                    allowed: false,
                    limit: self.limit,
                    remaining: 0,
                    reset_after_ns,
                    retry_after_ns,
                };
            }
            let new_tat = tat_val.max(now) + self.t_ns;
            if tat
                .compare_exchange_weak(old_tat, new_tat, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let max_tat = now + self.tau_ns + self.t_ns;
                let remaining = max_tat.saturating_sub(new_tat) / self.t_ns;
                let remaining = remaining.min(self.burst);
                let reset_after_ns = new_tat.saturating_sub(now);
                return harrow::RateLimitOutcome {
                    allowed: true,
                    limit: self.limit,
                    remaining,
                    reset_after_ns,
                    retry_after_ns: 0,
                };
            }
        }
    }
}

impl harrow::RateLimitBackend for InMemoryBackend {
    fn check(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = harrow::RateLimitOutcome> + Send {
        let now = now_ns();
        if let Some(entry) = self.states.get(key) {
            return std::future::ready(self.gcra_check(entry.value(), now));
        }
        let entry = self
            .states
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        std::future::ready(self.gcra_check(entry.value(), now))
    }
}
