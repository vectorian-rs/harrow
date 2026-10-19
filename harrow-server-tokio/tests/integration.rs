use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
extern crate http;
use harrow_middleware::body_limit::body_limit_middleware;
use harrow_middleware::catch_panic::catch_panic_middleware;
use harrow_middleware::o11y::o11y_middleware;
use harrow_middleware::rate_limit::{
    HeaderKeyExtractor, RateLimitBackend, RateLimitOutcome, rate_limit_middleware,
};
use harrow_middleware::session::{Session, SessionConfig, SessionStore, session_middleware};
use harrow_o11y::O11yConfig;
use http::StatusCode;

/// Shared counter used as application state.
struct HitCounter(AtomicUsize);

// ---------------------------------------------------------------------------
// Test-only in-memory stores
// ---------------------------------------------------------------------------

use std::sync::RwLock;
use std::time::Instant as StdInstant;

struct SessionEntry {
    data: std::collections::HashMap<String, String>,
    expires_at: StdInstant,
}

struct InMemorySessionStore {
    sessions: Arc<RwLock<std::collections::HashMap<String, SessionEntry>>>,
}

impl InMemorySessionStore {
    fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }
}

impl SessionStore for InMemorySessionStore {
    async fn load(&self, id: &str) -> Option<std::collections::HashMap<String, String>> {
        let sessions = self.sessions.read().unwrap();
        let entry = sessions.get(id)?;
        if entry.expires_at <= StdInstant::now() {
            return None;
        }
        Some(entry.data.clone())
    }

    async fn save(
        &self,
        id: &str,
        data: &std::collections::HashMap<String, String>,
        ttl: Duration,
    ) {
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

use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;

static EPOCH: LazyLock<StdInstant> = LazyLock::new(StdInstant::now);

fn now_ns() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

struct InMemoryBackend {
    states: Arc<dashmap::DashMap<String, AtomicU64>>,
    t_ns: u64,
    tau_ns: u64,
    limit: u64,
    burst: u64,
}

impl InMemoryBackend {
    fn per_second(rate: u64) -> Self {
        let t_ns = 1_000_000_000 / rate;
        Self {
            states: Arc::new(dashmap::DashMap::new()),
            t_ns,
            tau_ns: (rate - 1) * t_ns,
            limit: rate,
            burst: rate,
        }
    }

    fn burst(mut self, burst: u64) -> Self {
        self.burst = burst;
        self.tau_ns = (burst - 1) * self.t_ns;
        self
    }

    fn gcra_check(&self, tat: &AtomicU64, now: u64) -> RateLimitOutcome {
        loop {
            let old_tat = tat.load(Ordering::Relaxed);
            let tat_val = if old_tat == 0 { now } else { old_tat };
            if now < tat_val.saturating_sub(self.tau_ns) {
                let retry_after_ns = tat_val.saturating_sub(self.tau_ns) - now;
                let reset_after_ns = tat_val.saturating_sub(now);
                return RateLimitOutcome {
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
                return RateLimitOutcome {
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

impl RateLimitBackend for InMemoryBackend {
    fn check(&self, key: &str) -> impl std::future::Future<Output = RateLimitOutcome> + Send {
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

// -- Handlers ----------------------------------------------------------------

async fn hello(_req: Request) -> Response {
    Response::text("hello")
}

async fn greet(req: Request) -> Response {
    let name = req.param("name");
    Response::text(format!("hello, {name}"))
}

async fn state_handler(req: Request) -> Response {
    let counter = req.require_state::<Arc<HitCounter>>().unwrap();
    let count = counter.0.fetch_add(1, Ordering::Relaxed) + 1;
    Response::text(format!("hits: {count}"))
}

// -- Middleware --------------------------------------------------------------

/// Prepends "before|" to the response body and appends "|after".
async fn wrap_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    // We can't easily read the body back, so add a header to prove we ran.
    resp.header("x-wrap", "true")
}

/// A second middleware that adds its own header.
async fn second_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-second", "yes")
}

// -- TCP Helpers (kept for true end-to-end tests) ----------------------------

struct TestServer {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

static TEST_SERVERS: LazyLock<Mutex<Vec<TestServer>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn spawn_server_thread<F>(
    make_app: F,
    addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    config: harrow_server_tokio::ServerConfig,
) -> std::thread::JoinHandle<()>
where
    F: Fn() -> App + Send + 'static,
{
    std::thread::spawn(move || {
        let app = make_app();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();

        rt.block_on(local.run_until(async move {
            harrow_server_tokio::serve_with_config(
                app,
                addr,
                async move {
                    let _ = shutdown.await;
                },
                config,
            )
            .await
            .unwrap();
        }));
    })
}

/// Spin up the server on a random port, return the bound address.
async fn start_server<F>(make_app: F) -> SocketAddr
where
    F: Fn() -> App + Send + 'static,
{
    start_server_with_config(make_app, harrow_server_tokio::ServerConfig::default()).await
}

async fn start_server_with_config<F>(
    make_app: F,
    config: harrow_server_tokio::ServerConfig,
) -> SocketAddr
where
    F: Fn() -> App + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let thread = spawn_server_thread(make_app, addr, rx, config);

    tokio::time::sleep(Duration::from_millis(50)).await;
    TEST_SERVERS.lock().unwrap().push(TestServer {
        shutdown: Some(tx),
        thread: Some(thread),
    });
    addr
}

/// Simple HTTP/1.1 GET via raw TCP, returns (status, headers, body).
async fn http_get(addr: SocketAddr, path: &str) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(2, ": ");
            let key = parts.next()?.to_lowercase();
            let val = parts.next()?.to_string();
            Some((key, val))
        })
        .collect();

    let body = if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        decode_chunked(&body)
    } else {
        body
    };

    (status, headers, body)
}

fn decode_chunked(raw: &str) -> String {
    let mut result = String::new();
    let mut remaining = raw;
    loop {
        let (size_str, rest) = remaining.split_once("\r\n").unwrap_or(("0", ""));
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        result.push_str(&rest[..size]);
        remaining = &rest[size..];
        if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
        }
    }
    result
}

fn parse_raw_http_response(raw: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let head_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..head_end.saturating_sub(4)]);
    let body = raw[head_end..].to_vec();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(2, ": ");
            let key = parts.next()?.to_lowercase();
            let val = parts.next()?.to_string();
            Some((key, val))
        })
        .collect();

    (status, headers, body)
}

async fn raw_http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    parse_raw_http_response(&buf)
}

fn header_val<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Simple HTTP/1.1 GET with extra headers, returns (status, headers, body).
async fn http_get_with_headers(
    addr: SocketAddr,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(2, ": ");
            let key = parts.next()?.to_lowercase();
            let val = parts.next()?.to_string();
            Some((key, val))
        })
        .collect();

    let body = if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        decode_chunked(&body)
    } else {
        body
    };

    (status, headers, body)
}

// ============================================================================
// Client-based tests (no TCP)
// ============================================================================

#[tokio::test]
async fn client_basic_routing() {
    let client = App::new()
        .get("/hello", hello)
        .get("/greet/:name", greet)
        .client();

    let resp = client.get("/hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");

    let resp = client.get("/greet/world").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello, world");
}

#[tokio::test]
async fn client_returns_404_for_unknown_path() {
    let client = App::new().get("/hello", hello).client();
    let resp = client.get("/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn client_returns_405_for_wrong_method() {
    let client = App::new().post("/hello", hello).client();
    let resp = client.get("/hello").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn client_middleware_runs_in_order() {
    let client = App::new()
        .middleware(wrap_middleware)
        .middleware(second_middleware)
        .get("/hello", hello)
        .client();

    let resp = client.get("/hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");
    assert_eq!(resp.header("x-wrap"), Some("true"));
    assert_eq!(resp.header("x-second"), Some("yes"));
}

#[tokio::test]
async fn client_state_injection_works() {
    let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
    let client = App::new()
        .state(counter.clone())
        .get("/count", state_handler)
        .client();

    let resp = client.get("/count").await;
    assert_eq!(resp.text(), "hits: 1");

    let resp = client.get("/count").await;
    assert_eq!(resp.text(), "hits: 2");

    let resp = client.get("/count").await;
    assert_eq!(resp.text(), "hits: 3");

    assert_eq!(counter.0.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn client_middleware_and_state_together() {
    let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
    let client = App::new()
        .state(counter.clone())
        .middleware(wrap_middleware)
        .get("/count", state_handler)
        .client();

    let resp = client.get("/count").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hits: 1");
    assert_eq!(resp.header("x-wrap"), Some("true"));
}

// -- Route Group Tests (Client) ----------------------------------------------

async fn group_tag_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-group", "api")
}

async fn inner_tag_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-inner", "v1")
}

async fn users_handler(_req: Request) -> Response {
    Response::text("users list")
}

async fn user_by_id(req: Request) -> Response {
    let id = req.param("id");
    Response::text(format!("user {id}"))
}

#[tokio::test]
async fn client_group_basic_prefix() {
    let client = App::new()
        .get("/health", hello)
        .group("/api", |g| {
            g.get("/users", users_handler).get("/users/:id", user_by_id)
        })
        .client();

    let resp = client.get("/health").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");

    let resp = client.get("/api/users").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "users list");

    let resp = client.get("/api/users/42").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "user 42");

    let resp = client.get("/users").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn client_group_scoped_middleware() {
    let client = App::new()
        .get("/health", hello)
        .group("/api", |g| {
            g.middleware(group_tag_middleware)
                .get("/users", users_handler)
        })
        .client();

    let resp = client.get("/health").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.header("x-group"), None);

    let resp = client.get("/api/users").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "users list");
    assert_eq!(resp.header("x-group"), Some("api"));
}

#[tokio::test]
async fn client_group_with_global_middleware() {
    let client = App::new()
        .middleware(wrap_middleware)
        .get("/health", hello)
        .group("/api", |g| {
            g.middleware(group_tag_middleware)
                .get("/users", users_handler)
        })
        .client();

    let resp = client.get("/health").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.header("x-wrap"), Some("true"));
    assert_eq!(resp.header("x-group"), None);

    let resp = client.get("/api/users").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "users list");
    assert_eq!(resp.header("x-wrap"), Some("true"));
    assert_eq!(resp.header("x-group"), Some("api"));
}

#[tokio::test]
async fn client_nested_groups() {
    let client = App::new()
        .group("/api", |g| {
            g.middleware(group_tag_middleware)
                .get("/health", hello)
                .group("/v1", |v1| {
                    v1.middleware(inner_tag_middleware)
                        .get("/users", users_handler)
                })
        })
        .client();

    let resp = client.get("/api/health").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.header("x-group"), Some("api"));
    assert_eq!(resp.header("x-inner"), None);

    let resp = client.get("/api/v1/users").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "users list");
    assert_eq!(resp.header("x-group"), Some("api"));
    assert_eq!(resp.header("x-inner"), Some("v1"));
}

#[tokio::test]
async fn client_group_404_and_405() {
    let client = App::new()
        .group("/api", |g| g.post("/submit", hello))
        .client();

    let resp = client.get("/api/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = client.get("/api/submit").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// -- Allow Header on 405 (Client) -------------------------------------------

#[tokio::test]
async fn client_returns_405_with_allow_header() {
    let client = App::new()
        .get("/users", users_handler)
        .post("/users", hello)
        .delete("/users", hello)
        .client();

    let resp = client.put("/users", "").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    let allow = resp.header("allow").expect("expected Allow header on 405");
    let mut methods: Vec<&str> = allow.split(", ").collect();
    methods.sort();
    // HEAD is implicitly added because GET is registered (RFC 9110 §9.3.2).
    assert_eq!(methods, vec!["DELETE", "GET", "HEAD", "POST"]);
}

// -- HEAD Auto-Handling (Client) ---------------------------------------------

#[tokio::test]
async fn client_head_returns_get_headers_without_body() {
    let client = App::new().get("/hello", hello).client();
    let resp = client.head("/hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.text().is_empty(), "HEAD response body should be empty");
    assert!(
        resp.header("content-type").is_some(),
        "HEAD response should preserve Content-Type"
    );
}

#[tokio::test]
async fn client_head_returns_404_for_unknown_path() {
    let client = App::new().get("/hello", hello).client();
    let resp = client.head("/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn client_head_generated_404_and_405_have_empty_bodies() {
    let client = App::new()
        .default_problem_details()
        .post("/submit", hello)
        .client();

    let resp = client.head("/nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.header("content-type"),
        Some("application/problem+json")
    );
    assert!(resp.text().is_empty());

    let resp = client.head("/submit").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.header("allow"), Some("POST"));
    assert_eq!(
        resp.header("content-type"),
        Some("application/problem+json")
    );
    assert!(resp.text().is_empty());
}

// -- Route Pattern (Client) --------------------------------------------------

async fn echo_route_pattern(req: Request) -> Response {
    let pattern = req.route_pattern().unwrap_or("none").to_string();
    Response::text(pattern)
}

#[tokio::test]
async fn client_route_pattern_is_template_not_resolved() {
    let client = App::new().get("/users/:id", echo_route_pattern).client();
    let resp = client.get("/users/42").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "/users/:id");
}

// -- Body Size Limit (Client) ------------------------------------------------

async fn echo_body(req: Request) -> Response {
    match req.body_bytes().await {
        Ok(bytes) => Response::text(format!("got {} bytes", bytes.len())),
        Err(e) => Response::new(http::StatusCode::PAYLOAD_TOO_LARGE, format!("error: {e}")),
    }
}

#[tokio::test]
async fn client_body_size_limit_rejects_oversized_content_length() {
    let client = App::new()
        .max_body_size(100)
        .post("/upload", echo_body)
        .client();

    let body = vec![b'x'; 200];
    let req = http::Request::post("/upload")
        .header("content-length", "200")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn client_body_size_limit_allows_small_body() {
    let client = App::new()
        .max_body_size(1024)
        .post("/upload", echo_body)
        .client();

    let resp = client.post("/upload", "hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "got 5 bytes");
}

#[tokio::test]
async fn client_body_size_zero_means_no_limit() {
    let client = App::new()
        .max_body_size(0)
        .post("/upload", echo_body)
        .client();

    let body = vec![b'x'; 10_000];
    let resp = client.post("/upload", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "got 10000 bytes");
}

// -- Timeout Middleware (Client) ---------------------------------------------

async fn slow_handler(_req: Request) -> Response {
    tokio::time::sleep(Duration::from_millis(200)).await;
    Response::text("slow")
}

// -- Body Limit Middleware (Client) -------------------------------------------

#[tokio::test]
async fn client_body_limit_rejects_oversized_content_length() {
    let client = App::new()
        .middleware(body_limit_middleware(100))
        .post("/upload", echo_body)
        .client();

    let req = http::Request::post("/upload")
        .header("content-length", "200")
        .body(http_body_util::Full::new(bytes::Bytes::from(vec![
            b'x';
            200
        ])))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn client_body_limit_allows_under_limit() {
    let client = App::new()
        .middleware(body_limit_middleware(1024))
        .post("/upload", echo_body)
        .client();

    let resp = client.post("/upload", "hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "got 5 bytes");
}

#[tokio::test]
async fn client_body_limit_allows_exact_limit() {
    let client = App::new()
        .middleware(body_limit_middleware(5))
        .post("/upload", echo_body)
        .client();

    let resp = client.post("/upload", "hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "got 5 bytes");
}

#[tokio::test]
async fn client_body_limit_passes_without_content_length() {
    let client = App::new()
        .middleware(body_limit_middleware(1024))
        .get("/hello", hello)
        .client();

    let resp = client.get("/hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");
}

// ============================================================================
// TCP-based tests (true end-to-end, kept for server-level coverage)
// ============================================================================

#[tokio::test]
async fn tcp_basic_routing() {
    let app = || App::new().get("/hello", hello).get("/greet/:name", greet);
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hello");

    let (status, _, body) = http_get(addr, "/greet/world").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hello, world");
}

#[tokio::test]
async fn tcp_probe_helpers_register_endpoints() {
    let app = || {
        App::new()
            .state(Arc::new(String::from("primary-db")))
            .health("/health")
            .liveness("/live")
            .readiness_handler("/ready", |req| async move {
                let db = req.require_state::<Arc<String>>().unwrap();
                Response::text(format!("ready {}", db.as_str()))
            })
    };
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/health").await;
    assert_eq!(status, 200);
    assert_eq!(body, "ok");

    let (status, _, body) = http_get(addr, "/live").await;
    assert_eq!(status, 200);
    assert_eq!(body, "alive");

    let (status, _, body) = http_get(addr, "/ready").await;
    assert_eq!(status, 200);
    assert_eq!(body, "ready primary-db");
}

async fn panicking_handler(_req: Request) -> Response {
    panic!("handler bug");
}

#[tokio::test]
async fn tcp_panic_in_handler_closes_connection_without_catch_panic() {
    let app = || App::new().get("/boom", panicking_handler);
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/boom").await;
    assert_eq!(status, 0);
    assert!(body.is_empty());
}

#[tokio::test]
async fn tcp_catch_panic_middleware_returns_500() {
    let app = || {
        App::new()
            .middleware(catch_panic_middleware)
            .get("/boom", panicking_handler)
    };
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/boom").await;
    assert_eq!(status, 500);
    assert_eq!(body, "internal server error");
}

#[tokio::test]
async fn tcp_o11y_middleware_adds_request_id_header() {
    let app = || {
        App::new()
            .state(Arc::new(O11yConfig::default()))
            .middleware(o11y_middleware)
            .get("/hello", hello)
    };

    let addr = start_server(app).await;

    let (status, headers, _) = http_get(addr, "/hello").await;
    assert_eq!(status, 200);
    let rid = header_val(&headers, "x-request-id");
    assert!(rid.is_some(), "expected x-request-id header");
    let rid = rid.unwrap();
    assert_eq!(rid.len(), 11, "expected 11-char request ID");
    assert!(rid.is_ascii(), "expected ASCII characters only");
}

#[tokio::test]
async fn tcp_o11y_middleware_echoes_incoming_request_id() {
    let app = || {
        App::new()
            .state(Arc::new(O11yConfig::default()))
            .middleware(o11y_middleware)
            .get("/hello", hello)
    };

    let addr = start_server(app).await;

    let (status, headers, _) =
        http_get_with_headers(addr, "/hello", &[("x-request-id", "client-123")]).await;
    assert_eq!(status, 200);
    assert_eq!(header_val(&headers, "x-request-id"), Some("client-123"));
}

/// Simple HTTP/1.1 request with arbitrary method, returns (status, headers, body).
async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(2, ": ");
            let key = parts.next()?.to_lowercase();
            let val = parts.next()?.to_string();
            Some((key, val))
        })
        .collect();

    let body = if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        decode_chunked(&body)
    } else {
        body
    };

    (status, headers, body)
}

// -- Graceful Drain Test (TCP) -----------------------------------------------

#[tokio::test]
async fn tcp_graceful_drain_completes_inflight_request() {
    let app = || App::new().get("/slow", slow_handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = spawn_server_thread(
        app,
        addr,
        rx,
        harrow_server_tokio::ServerConfig {
            drain_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start a request to the slow handler (takes 200ms).
    let request_handle = tokio::spawn(async move { http_get(addr, "/slow").await });

    // Wait a bit for the request to reach the handler, then signal shutdown.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = tx.send(());

    // The in-flight request should still complete despite shutdown signal.
    let (status, _, body) = request_handle.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "slow");
    server.join().unwrap();
}

#[tokio::test]
async fn tcp_graceful_shutdown_waits_for_drain_before_returning() {
    let app = || App::new().get("/slow", slow_handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let server = spawn_server_thread(
        app,
        addr,
        rx,
        harrow_server_tokio::ServerConfig {
            drain_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let request = tokio::spawn(async move { http_get(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = tx.send(());

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !server.is_finished(),
        "server future returned before in-flight request drained"
    );

    let (status, _, body) = request.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "slow");

    tokio::time::timeout(
        Duration::from_secs(1),
        tokio::task::spawn_blocking(move || server.join().unwrap()),
    )
    .await
    .expect("server should finish after in-flight request drains")
    .unwrap();
}

// -- Body Read Timeout Test -------------------------------------------------

#[tokio::test]
async fn tcp_body_read_timeout_returns_400() {
    let app = || {
        App::new().post("/echo", |req: Request| async move {
            match req.body_bytes().await {
                Ok(bytes) => Response::text(format!("got {} bytes", bytes.len())),
                Err(e) => Response::text(format!("error: {e}")).status(400),
            }
        })
    };

    let addr = start_server_with_config(
        app,
        harrow_server_tokio::ServerConfig {
            body_read_timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        },
    )
    .await;

    // Send headers promising a body, then stall — never send the body.
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\r\n")
        .await
        .unwrap();

    // Don't send any body data. Wait for the server to respond with an error.
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .expect("should get a response within 3s")
        .expect("read should succeed");
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(
        response.contains("400") || response.contains("408") || response.contains("timeout"),
        "expected 400/408 or timeout error, got: {response}"
    );
}

#[tokio::test]
async fn tcp_head_returns_get_headers_without_body() {
    let app = || App::new().get("/hello", hello);
    let addr = start_server(app).await;

    let (status, headers, body) = http_request(addr, "HEAD", "/hello").await;
    assert_eq!(status, 200);
    assert!(body.is_empty(), "HEAD response body should be empty");
    assert!(
        header_val(&headers, "content-type").is_some(),
        "HEAD response should preserve Content-Type"
    );
}

#[tokio::test]
async fn tcp_head_response_is_not_chunked_on_wire() {
    let app = || App::new().get("/hello", hello);
    let addr = start_server(app).await;

    let (status, headers, body) = raw_http_request(addr, "HEAD", "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(header_val(&headers, "transfer-encoding"), None);
    assert!(body.is_empty(), "HEAD response should not write body bytes");
}

#[tokio::test]
async fn tcp_204_and_304_responses_are_bodyless_on_wire() {
    let app = || {
        App::new()
            .get("/no-content", |_req: Request| async move {
                Response::text("hidden").status(StatusCode::NO_CONTENT.as_u16())
            })
            .get("/not-modified", |_req: Request| async move {
                Response::text("hidden").status(StatusCode::NOT_MODIFIED.as_u16())
            })
    };
    let addr = start_server(app).await;

    for (path, expected_status) in [
        ("/no-content", StatusCode::NO_CONTENT),
        ("/not-modified", StatusCode::NOT_MODIFIED),
    ] {
        let (status, headers, body) = raw_http_request(addr, "GET", path).await;
        assert_eq!(status, expected_status.as_u16(), "{path}");
        assert_eq!(
            header_val(&headers, "transfer-encoding"),
            None,
            "{path} should not be chunked"
        );
        assert!(body.is_empty(), "{path} should not write body bytes");
    }
}

#[tokio::test]
async fn tcp_streaming_response_flushes_before_eof() {
    let app = || {
        App::new().get("/stream", |_req: Request| async move {
            let stream = futures_util::stream::unfold(0u8, |state| async move {
                match state {
                    0 => Some((
                        Ok(http_body::Frame::data(bytes::Bytes::from_static(b"first"))),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Some((
                            Ok(http_body::Frame::data(bytes::Bytes::from_static(b"second"))),
                            2,
                        ))
                    }
                    _ => None,
                }
            });
            Response::streaming(StatusCode::OK, stream)
        })
    };
    let addr = start_server(app).await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let initial = tokio::time::timeout(Duration::from_millis(100), async {
        let mut acc = Vec::new();
        loop {
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before first chunk arrived");
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(b"first".len()).any(|window| window == b"first") {
                break acc;
            }
        }
    })
    .await
    .expect("expected response head and first chunk before stream EOF");

    let initial_text = String::from_utf8_lossy(&initial);
    assert!(initial_text.contains("HTTP/1.1 200"));
    assert!(
        initial_text.contains("first"),
        "expected first chunk to flush early: {initial_text}"
    );

    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();

    let mut full = initial;
    full.extend_from_slice(&rest);
    let full_text = String::from_utf8_lossy(&full);
    assert!(full_text.contains("second"));
    assert!(full_text.contains("0\r\n\r\n"));
}

#[tokio::test]
async fn tcp_request_body_streams_before_request_eof() {
    let app = || {
        App::new().post("/first-chunk", |mut req: Request| async move {
            use http_body_util::BodyExt;

            let frame = req
                .inner_mut()
                .body_mut()
                .frame()
                .await
                .expect("body should yield first chunk")
                .expect("chunk should not error");
            let data = frame.into_data().expect("expected data frame");
            Response::text(String::from_utf8_lossy(&data).to_string())
        })
    };
    let addr = start_server(app).await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"POST /first-chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n",
        )
        .await
        .unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_millis(250), stream.read_to_end(&mut buf))
        .await
        .expect("expected response before request EOF")
        .unwrap();

    let raw = String::from_utf8_lossy(&buf);
    assert!(raw.contains("HTTP/1.1 200"), "unexpected response: {raw}");
    assert!(
        raw.contains("hello"),
        "expected first chunk in response: {raw}"
    );
    assert!(raw.to_lowercase().contains("connection: close"));
}

#[tokio::test]
async fn tcp_default_problem_details_formats_404_and_405() {
    let app = || App::new().default_problem_details().get("/users", hello);
    let addr = start_server(app).await;

    let (status, headers, body) = http_get(addr, "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(
        header_val(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert!(body.contains("\"status\":404"));
    assert!(body.contains("\"detail\":\"no route for /missing\""));

    let (status, headers, body) = http_request(addr, "POST", "/users").await;
    assert_eq!(status, 405);
    assert_eq!(
        header_val(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(header_val(&headers, "allow"), Some("GET, HEAD"));
    assert!(body.contains("\"status\":405"));
    assert!(body.contains("\"allow\":\"GET, HEAD\""));
}

#[tokio::test]
async fn tcp_custom_fallback_handlers_can_access_state() {
    let app = || {
        App::new()
            .state(Arc::new(String::from("tcp-state")))
            .get("/users", hello)
            .not_found_handler(|req| async move {
                let state = req.require_state::<Arc<String>>().unwrap();
                Response::text(format!("{} missing {}", state.as_str(), req.path()))
            })
            .method_not_allowed_handler(|req, methods| async move {
                let state = req.require_state::<Arc<String>>().unwrap();
                let allow = methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Response::text(format!(
                    "{} allow {} on {}",
                    state.as_str(),
                    allow,
                    req.path()
                ))
            })
    };
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(body, "tcp-state missing /missing");

    let (status, headers, body) = http_request(addr, "PUT", "/users").await;
    assert_eq!(status, 405);
    assert_eq!(header_val(&headers, "allow"), Some("GET, HEAD"));
    assert_eq!(body, "tcp-state allow GET, HEAD on /users");
}

#[tokio::test]
async fn tcp_head_generated_404_and_405_strip_body() {
    let app = || App::new().default_problem_details().post("/submit", hello);
    let addr = start_server(app).await;

    let (status, headers, body) = http_request(addr, "HEAD", "/missing").await;
    assert_eq!(status, 404);
    assert_eq!(
        header_val(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert!(body.is_empty(), "HEAD 404 should not include a body");

    let (status, headers, body) = http_request(addr, "HEAD", "/submit").await;
    assert_eq!(status, 405);
    assert_eq!(
        header_val(&headers, "content-type"),
        Some("application/problem+json")
    );
    assert_eq!(header_val(&headers, "allow"), Some("POST"));
    assert!(body.is_empty(), "HEAD 405 should not include a body");
}

// -- Body Limit Test (TCP) ---------------------------------------------------

/// Simple HTTP/1.1 POST with body + Content-Length, returns (status, headers, body).
async fn http_post(
    addr: SocketAddr,
    path: &str,
    body: &[u8],
) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let resp_body = parts.next().unwrap_or("").to_string();

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let mut parts = line.splitn(2, ": ");
            let key = parts.next()?.to_lowercase();
            let val = parts.next()?.to_string();
            Some((key, val))
        })
        .collect();

    let resp_body = if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        decode_chunked(&resp_body)
    } else {
        resp_body
    };

    (status, headers, resp_body)
}

#[tokio::test]
async fn tcp_body_limit_rejects_oversized() {
    let app = || {
        App::new()
            .middleware(body_limit_middleware(50))
            .post("/upload", echo_body)
    };
    let addr = start_server(app).await;

    let body = vec![b'x'; 200];
    let (status, _, _) = http_post(addr, "/upload", &body).await;
    assert_eq!(status, 413);
}

#[tokio::test]
async fn tcp_body_limit_allows_small() {
    let app = || {
        App::new()
            .middleware(body_limit_middleware(1024))
            .post("/upload", echo_body)
    };
    let addr = start_server(app).await;

    let (status, _, body) = http_post(addr, "/upload", b"hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, "got 5 bytes");
}

// -- Rate Limit Middleware (Client) ------------------------------------------

#[tokio::test]
async fn client_rate_limit_returns_429_after_exceeding_limit() {
    let backend = InMemoryBackend::per_second(2).burst(2);
    let client = App::new()
        .middleware(rate_limit_middleware(
            backend,
            HeaderKeyExtractor::new("x-api-key"),
        ))
        .get("/hello", hello)
        .client();

    // First two requests should pass (burst=2)
    for _ in 0..2 {
        let req = http::Request::get("/hello")
            .header("x-api-key", "test-key")
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .unwrap();
        let resp = client.request(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Third request should be rate limited
    let req = http::Request::get("/hello")
        .header("x-api-key", "test-key")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.header("retry-after").is_some(),
        "expected retry-after header"
    );
}

#[tokio::test]
async fn client_rate_limit_passes_under_limit_with_headers() {
    let backend = InMemoryBackend::per_second(10).burst(10);
    let client = App::new()
        .middleware(rate_limit_middleware(
            backend,
            HeaderKeyExtractor::new("x-api-key"),
        ))
        .get("/hello", hello)
        .client();

    let req = http::Request::get("/hello")
        .header("x-api-key", "test-key")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");
    assert!(
        resp.header("x-ratelimit-limit").is_some(),
        "expected x-ratelimit-limit header"
    );
    assert!(
        resp.header("x-ratelimit-remaining").is_some(),
        "expected x-ratelimit-remaining header"
    );
    assert!(
        resp.header("x-ratelimit-reset").is_some(),
        "expected x-ratelimit-reset header"
    );
}

#[tokio::test]
async fn client_rate_limit_skips_when_key_header_missing() {
    let backend = InMemoryBackend::per_second(1).burst(1);
    let client = App::new()
        .middleware(rate_limit_middleware(
            backend,
            HeaderKeyExtractor::new("x-api-key"),
        ))
        .get("/hello", hello)
        .client();

    // No x-api-key header → rate limiting skipped, all pass
    for _ in 0..5 {
        let resp = client.get("/hello").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text(), "hello");
        assert!(
            resp.header("x-ratelimit-limit").is_none(),
            "should not have rate limit headers when key missing"
        );
    }
}

// ============================================================================
// Session Middleware (Client)
// ============================================================================

fn test_secret() -> [u8; 32] {
    *b"test-secret-key-for-harrow-sess!"
}

async fn session_set_handler(req: Request) -> Response {
    let session = req.ext::<Session>().unwrap();
    session.set("user", "alice");
    Response::text("set")
}

async fn session_get_handler(req: Request) -> Response {
    let session = req.ext::<Session>().unwrap();
    let user = session.get("user").unwrap_or_default();
    Response::text(user)
}

async fn session_destroy_handler(req: Request) -> Response {
    let session = req.ext::<Session>().unwrap();
    session.destroy();
    Response::text("destroyed")
}

async fn session_noop_handler(req: Request) -> Response {
    // Access session but don't modify it
    let session = req.ext::<Session>().unwrap();
    let _ = session.get("anything");
    Response::text("noop")
}

#[tokio::test]
async fn client_session_round_trip() {
    let config = SessionConfig::new(test_secret()).secure(false);
    let client = App::new()
        .middleware(session_middleware(InMemorySessionStore::new(), config))
        .get("/set", session_set_handler)
        .get("/get", session_get_handler)
        .client();

    // Set session data
    let resp = client.get("/set").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp
        .header("set-cookie")
        .expect("expected set-cookie on set");
    assert!(cookie.contains("sid="));

    // Extract the cookie value (everything before ';')
    let cookie_val = cookie.split(';').next().unwrap().trim();

    // Send cookie back to read session data
    let req = http::Request::get("/get")
        .header("cookie", cookie_val)
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "alice");
}

#[tokio::test]
async fn client_session_destroy_clears() {
    let config = SessionConfig::new(test_secret()).secure(false);
    let client = App::new()
        .middleware(session_middleware(InMemorySessionStore::new(), config))
        .get("/set", session_set_handler)
        .get("/destroy", session_destroy_handler)
        .get("/get", session_get_handler)
        .client();

    // Set data
    let resp = client.get("/set").await;
    let cookie = resp.header("set-cookie").unwrap();
    let cookie_val = cookie.split(';').next().unwrap().trim();

    // Destroy session
    let req = http::Request::get("/destroy")
        .header("cookie", cookie_val)
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.text(), "destroyed");
    let clear_cookie = resp.header("set-cookie").expect("expected clear cookie");
    assert!(clear_cookie.contains("Max-Age=0"));

    // Next request with old cookie should see empty session
    let req = http::Request::get("/get")
        .header("cookie", cookie_val)
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    assert_eq!(resp.text(), ""); // empty — session data gone
}

#[tokio::test]
async fn client_session_no_cookie_on_unmodified() {
    let config = SessionConfig::new(test_secret()).secure(false);
    let client = App::new()
        .middleware(session_middleware(InMemorySessionStore::new(), config))
        .get("/noop", session_noop_handler)
        .client();

    let resp = client.get("/noop").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.header("set-cookie").is_none(),
        "unmodified session should not set cookie"
    );
}

#[tokio::test]
async fn client_session_tampered_cookie_rejected() {
    let config = SessionConfig::new(test_secret()).secure(false);
    let client = App::new()
        .middleware(session_middleware(InMemorySessionStore::new(), config))
        .get("/get", session_get_handler)
        .client();

    // Send a tampered cookie
    let tampered = "sid=abcdef0123456789abcdef0123456789.0000000000000000000000000000000000000000000000000000000000000000";
    let req = http::Request::get("/get")
        .header("cookie", tampered)
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = client.request(req).await;
    // Should get empty session (no user data)
    assert_eq!(resp.text(), "");
    // No set-cookie since session wasn't modified
    assert!(resp.header("set-cookie").is_none());
}

// ============================================================================
// HTTP/2 h2c Tests (cleartext HTTP/2 over TCP)
// Gated behind "h2" feature — HTTP/2 was intentionally removed from the
// default backend in favour of the custom harrow-codec-h1 path.
// These tests will be re-enabled when harrow-codec-h2 is implemented (#57).
// ============================================================================
#[cfg(any())]
mod h2c_tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty};
    use hyper::client::conn::http2 as h2_client;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    /// Helper: perform an HTTP/2 cleartext (h2c) request against a harrow server.
    /// Returns (status, body_string).
    async fn h2c_get(addr: SocketAddr, path: &str) -> (u16, String) {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = h2_client::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();

        // Drive the connection in the background.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("h2c connection error: {e}");
            }
        });

        let req = http::Request::get(path)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();

        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body).to_string();

        (status, body_str)
    }

    /// Helper: perform an HTTP/2 cleartext POST with a body.
    async fn h2c_post(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, String) {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = h2_client::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("h2c connection error: {e}");
            }
        });

        let req = http::Request::post(path)
            .body(http_body_util::Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();

        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    /// Helper: perform an HTTP/2 cleartext GET with extra headers.
    async fn h2c_get_with_headers(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Vec<(String, String)>, String) {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = h2_client::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("h2c connection error: {e}");
            }
        });

        let mut builder = http::Request::get(path);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Empty::<Bytes>::new()).unwrap();
        let resp = sender.send_request(req).await.unwrap();

        let status = resp.status().as_u16();
        let resp_headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            resp_headers,
            String::from_utf8_lossy(&body).to_string(),
        )
    }

    #[tokio::test]
    async fn h2c_basic_get() {
        let app = App::new().get("/hello", hello);
        let addr = start_server(app).await;

        let (status, body) = h2c_get(addr, "/hello").await;
        assert_eq!(status, 200);
        assert_eq!(body, "hello");
    }

    #[tokio::test]
    async fn h2c_path_params() {
        let app = App::new().get("/greet/:name", greet);
        let addr = start_server(app).await;

        let (status, body) = h2c_get(addr, "/greet/world").await;
        assert_eq!(status, 200);
        assert_eq!(body, "hello, world");
    }

    #[tokio::test]
    async fn h2c_404_on_unknown_path() {
        let app = App::new().get("/hello", hello);
        let addr = start_server(app).await;

        let (status, _) = h2c_get(addr, "/nope").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn h2c_405_on_wrong_method() {
        let app = App::new().post("/submit", hello);
        let addr = start_server(app).await;

        let (status, _) = h2c_get(addr, "/submit").await;
        assert_eq!(status, 405);
    }

    #[tokio::test]
    async fn h2c_post_with_body() {
        let app = App::new().post("/upload", echo_body);
        let addr = start_server(app).await;

        let (status, body) = h2c_post(addr, "/upload", b"hello h2").await;
        assert_eq!(status, 200);
        assert_eq!(body, "got 8 bytes");
    }

    #[tokio::test]
    async fn h2c_middleware_runs() {
        let app = App::new()
            .middleware(wrap_middleware)
            .middleware(second_middleware)
            .get("/hello", hello);
        let addr = start_server(app).await;

        let (status, headers, body) = h2c_get_with_headers(addr, "/hello", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(body, "hello");
        assert!(
            headers.iter().any(|(k, v)| k == "x-wrap" && v == "true"),
            "expected x-wrap header over h2c"
        );
        assert!(
            headers.iter().any(|(k, v)| k == "x-second" && v == "yes"),
            "expected x-second header over h2c"
        );
    }

    #[tokio::test]
    async fn h2c_multiplexed_requests() {
        let app = App::new().get("/hello", hello).get("/greet/:name", greet);
        let addr = start_server(app).await;

        // Open a single h2c connection and send multiple concurrent requests.
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = h2_client::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("h2c connection error: {e}");
            }
        });

        // Fire off multiple requests concurrently on the same connection.
        let mut handles = Vec::new();
        for i in 0..10 {
            let req = http::Request::get(format!("/greet/user{i}"))
                .body(Empty::<Bytes>::new())
                .unwrap();
            let fut = sender.send_request(req);
            handles.push(tokio::spawn(async move {
                let resp = fut.await.unwrap();
                let status = resp.status().as_u16();
                let body = resp.into_body().collect().await.unwrap().to_bytes();
                (i, status, String::from_utf8_lossy(&body).to_string())
            }));
        }

        for handle in handles {
            let (i, status, body) = handle.await.unwrap();
            assert_eq!(status, 200, "request {i} failed");
            assert_eq!(body, format!("hello, user{i}"), "request {i} wrong body");
        }
    }

    #[tokio::test]
    async fn h2c_state_works() {
        let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
        let app = App::new()
            .state(counter.clone())
            .get("/count", state_handler);
        let addr = start_server(app).await;

        let (status, body) = h2c_get(addr, "/count").await;
        assert_eq!(status, 200);
        assert_eq!(body, "hits: 1");

        let (status, body) = h2c_get(addr, "/count").await;
        assert_eq!(status, 200);
        assert_eq!(body, "hits: 2");
    }

    #[tokio::test]
    async fn h2c_body_limit_rejects_oversized() {
        let app = App::new()
            .middleware(body_limit_middleware(50))
            .post("/upload", echo_body);
        let addr = start_server(app).await;

        let body = vec![b'x'; 200];
        let (status, _) = h2c_post(addr, "/upload", &body).await;
        assert_eq!(status, 413);
    }

    #[tokio::test]
    async fn h2c_session_round_trip() {
        let config = SessionConfig::new(test_secret()).secure(false);
        let app = App::new()
            .middleware(session_middleware(InMemorySessionStore::new(), config))
            .get("/set", session_set_handler)
            .get("/get", session_get_handler);
        let addr = start_server(app).await;

        // Set session data
        let (status, headers, _) = h2c_get_with_headers(addr, "/set", &[]).await;
        assert_eq!(status, 200);
        let set_cookie = headers
            .iter()
            .find(|(k, _)| k == "set-cookie")
            .map(|(_, v)| v.as_str())
            .expect("expected set-cookie on h2c session set");
        assert!(set_cookie.contains("sid="));

        // Extract cookie value
        let cookie_val = set_cookie.split(';').next().unwrap().trim();

        // Read session data with cookie
        let (status, _, body) = h2c_get_with_headers(addr, "/get", &[("cookie", cookie_val)]).await;
        assert_eq!(status, 200);
        assert_eq!(body, "alice");
    }

    // ---------------------------------------------------------------------------
    // Global middleware runs on 404/405 (dispatch refactor)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn tcp_middleware_runs_on_404() {
        let app = App::new().middleware(wrap_middleware).get("/exists", hello);
        let addr = start_server(app).await;

        let (status, headers, _body) = http_get(addr, "/nope").await;
        assert_eq!(status, 404);
        let x_wrap = headers.iter().find(|(k, _)| k == "x-wrap");
        assert_eq!(
            x_wrap.map(|(_, v)| v.as_str()),
            Some("true"),
            "global middleware should run on 404 over TCP"
        );
    }

    #[tokio::test]
    async fn tcp_middleware_runs_on_405() {
        let app = App::new().middleware(wrap_middleware).get("/users", hello);
        let addr = start_server(app).await;

        let (status, headers, _body) = http_request(addr, "POST", "/users").await;
        assert_eq!(status, 405);
        let x_wrap = headers.iter().find(|(k, _)| k == "x-wrap");
        assert_eq!(
            x_wrap.map(|(_, v)| v.as_str()),
            Some("true"),
            "global middleware should run on 405 over TCP"
        );
        let allow = headers.iter().find(|(k, _)| k == "allow");
        assert!(allow.is_some(), "405 should include Allow header over TCP");
    }

    #[tokio::test]
    async fn tcp_o11y_middleware_runs_on_404() {
        let app = App::new().middleware(o11y_middleware).get("/exists", hello);
        let addr = start_server(app).await;

        let (status, headers, _body) = http_get(addr, "/nope").await;
        assert_eq!(status, 404);
        let rid = headers.iter().find(|(k, _)| k == "x-request-id");
        assert!(
            rid.is_some(),
            "o11y middleware should add request-id header on 404 over TCP"
        );
    }

    #[tokio::test]
    async fn tcp_o11y_middleware_runs_on_405() {
        let app = App::new().middleware(o11y_middleware).get("/users", hello);
        let addr = start_server(app).await;

        let (status, headers, _body) = http_request(addr, "DELETE", "/users").await;
        assert_eq!(status, 405);
        let rid = headers.iter().find(|(k, _)| k == "x-request-id");
        assert!(
            rid.is_some(),
            "o11y middleware should add request-id header on 405 over TCP"
        );
    }
} // mod h2c_tests

// ============================================================================
// WebSocket tests (feature = "ws")
// ============================================================================

#[cfg(feature = "ws")]
mod ws_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use harrow_core::response::IntoResponse;
    use harrow_core::ws::Message;
    use harrow_server_tokio::ws;
    use tokio_tungstenite::tungstenite;

    /// Echo handler: sends back whatever it receives, stops on Close.
    async fn ws_echo(req: Request) -> Response {
        ws::upgrade(req, |mut socket| async move {
            while let Some(Ok(msg)) = socket.recv().await {
                match msg {
                    Message::Close(_) => break,
                    other => {
                        let _ = socket.send(other).await;
                    }
                }
            }
        })
        .unwrap_or_else(|e| e.into_response())
    }

    #[tokio::test]
    async fn ws_raw_handshake_returns_rfc_accept() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let req = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();

        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let deadline = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let n = stream.read(&mut tmp).await.unwrap();
                assert!(n > 0, "server closed connection before responding");
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        });
        deadline.await.expect("server did not respond in time");
        let raw = String::from_utf8_lossy(&buf);

        assert!(raw.contains("101"));
        assert!(
            raw.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "Sec-WebSocket-Accept must match RFC 6455 example"
        );
    }

    #[tokio::test]
    async fn ws_text_echo() {
        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        stream
            .send(tungstenite::Message::Text("hello".into()))
            .await
            .unwrap();

        let msg = stream.next().await.unwrap().unwrap();
        assert_eq!(msg, tungstenite::Message::Text("hello".into()));

        stream.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn ws_binary_echo() {
        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        let payload: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        stream
            .send(tungstenite::Message::Binary(payload.clone().into()))
            .await
            .unwrap();

        let msg = stream.next().await.unwrap().unwrap();
        assert_eq!(msg, tungstenite::Message::Binary(payload.into()));

        stream.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn ws_multiple_messages() {
        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let (mut stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        for i in 0..5 {
            let text = format!("msg-{i}");
            stream
                .send(tungstenite::Message::Text(text.clone().into()))
                .await
                .unwrap();

            let msg = stream.next().await.unwrap().unwrap();
            assert_eq!(msg, tungstenite::Message::Text(text.into()));
        }

        stream.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn ws_non_upgrade_request_returns_400() {
        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let (status, _, _) = http_get(addr, "/ws").await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn ws_upgrade_on_wrong_path_returns_404() {
        let app = App::new().get("/ws", ws_echo);
        let addr = start_server(app).await;

        let url = format!("ws://127.0.0.1:{}/nope", addr.port());
        let result = tokio_tungstenite::connect_async(&url).await;
        assert!(result.is_err());
    }
}

// ============================================================================
// Adversarial HTTP/1 wire tests (#86)
//
// Each test pins what the Tokio backend does on the wire for a hostile or
// malformed exchange. See `docs/operations/verification.md` for the map from
// issue #86 acceptance criteria to tests.
// ============================================================================

mod h1_adversarial {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    type Headers = Vec<(String, String)>;

    /// Read until the server closes the connection (EOF or reset). Panics if
    /// the server keeps the connection open for more than five seconds.
    async fn read_until_close(stream: &mut TcpStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "server kept the connection open; read so far: {:?}",
            String::from_utf8_lossy(&out)
        );
        out
    }

    /// Send raw bytes on a fresh connection and return everything the server
    /// writes before closing it.
    async fn raw_exchange(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        read_until_close(&mut stream).await
    }

    /// Parse one response from the front of `raw`. Returns `None` while the
    /// response is incomplete. `has_body` is false for HEAD/204/304, whose
    /// framing headers do not describe bytes on the wire.
    fn try_parse_response(raw: &[u8], has_body: bool) -> Option<(u16, Headers, Vec<u8>, usize)> {
        let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let (status, headers, _) = parse_raw_http_response(&raw[..head_end]);
        if !has_body {
            return Some((status, headers, Vec::new(), head_end));
        }
        if let Some(len) = header_val(&headers, "content-length") {
            let end = head_end + len.parse::<usize>().unwrap();
            if raw.len() < end {
                return None;
            }
            let body = raw[head_end..end].to_vec();
            return Some((status, headers, body, end));
        }
        assert_eq!(
            header_val(&headers, "transfer-encoding"),
            Some("chunked"),
            "response has a body but no framing: {:?}",
            String::from_utf8_lossy(&raw[..head_end])
        );
        let mut pos = head_end;
        let mut body = Vec::new();
        loop {
            let line_end = pos + raw[pos..].windows(2).position(|w| w == b"\r\n")?;
            let size = usize::from_str_radix(std::str::from_utf8(&raw[pos..line_end]).unwrap(), 16)
                .unwrap();
            pos = line_end + 2;
            if raw.len() < pos + size + 2 {
                return None;
            }
            assert_eq!(&raw[pos + size..pos + size + 2], b"\r\n");
            if size == 0 {
                return Some((status, headers, body, pos + 2));
            }
            body.extend_from_slice(&raw[pos..pos + size]);
            pos += size + 2;
        }
    }

    /// Read exactly one response from a keep-alive connection, leaving any
    /// later bytes in `pending`.
    async fn read_one_response(
        stream: &mut TcpStream,
        pending: &mut Vec<u8>,
        has_body: bool,
    ) -> (u16, Headers, Vec<u8>) {
        loop {
            if let Some((status, headers, body, end)) = try_parse_response(pending, has_body) {
                pending.drain(..end);
                return (status, headers, body);
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("timed out waiting for a response")
                .expect("read failed");
            assert!(
                n > 0,
                "connection closed mid-response: {:?}",
                String::from_utf8_lossy(pending)
            );
            pending.extend_from_slice(&buf[..n]);
        }
    }

    fn count_responses(raw: &[u8]) -> usize {
        raw.windows(b"HTTP/1.1 ".len())
            .filter(|w| *w == b"HTTP/1.1 ")
            .count()
    }

    /// Assert the server answered with exactly one `status` response that
    /// closes the connection.
    fn assert_single_error(raw: &[u8], status: u16, context: &str) {
        let (got, headers, _) = parse_raw_http_response(raw);
        assert_eq!(
            got,
            status,
            "{context}: unexpected response {:?}",
            String::from_utf8_lossy(raw)
        );
        assert_eq!(
            header_val(&headers, "connection"),
            Some("close"),
            "{context}"
        );
        assert_eq!(count_responses(raw), 1, "{context}: extra response on wire");
    }

    fn one_chunk_stream(data: &'static [u8]) -> Response {
        Response::streaming(
            StatusCode::OK,
            futures_util::stream::iter([Ok(http_body::Frame::data(bytes::Bytes::from_static(
                data,
            )))]),
        )
    }

    // -- Request parsing / smuggling ----------------------------------------

    #[tokio::test]
    async fn tcp_oversized_incomplete_headers_get_400_and_close() {
        let addr = start_server(|| App::new().get("/", hello)).await;

        // Exactly MAX_HEADER_BUF bytes with no end-of-headers marker.
        let mut req = b"GET / HTTP/1.1\r\nHost: localhost\r\nx-large: ".to_vec();
        req.resize(64 * 1024, b'a');
        let raw = raw_exchange(addr, &req).await;

        assert_single_error(&raw, 400, "incomplete oversized headers");
        assert!(raw.ends_with(b"request headers too large"));
    }

    #[tokio::test]
    async fn tcp_duplicate_identical_content_length_is_accepted() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .await;

        let (status, _, body) = parse_raw_http_response(&raw);
        assert_eq!(status, 200);
        assert_eq!(body, b"got 5 bytes");
    }

    #[tokio::test]
    async fn tcp_conflicting_content_lengths_are_rejected_without_dispatch() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let addr = start_server(|| {
            App::new().post("/echo", |_req: Request| async {
                HITS.fetch_add(1, Ordering::SeqCst);
                Response::text("dispatched")
            })
        })
        .await;

        // If either length were honoured, the tail would parse as a second
        // (smuggled) request on the same connection.
        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\nhelloGET /smuggled HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;

        assert_single_error(&raw, 400, "conflicting content-length");
        assert_eq!(HITS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tcp_transfer_encoding_with_content_length_is_rejected() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let addr = start_server(|| {
            App::new().post("/echo", |_req: Request| async {
                HITS.fetch_add(1, Ordering::SeqCst);
                Response::text("dispatched")
            })
        })
        .await;

        for (name, req) in [
            (
                "CL then TE",
                &b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
            ),
            (
                "TE then CL",
                b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: localhost\r\n\r\n",
            ),
        ] {
            let raw = raw_exchange(addr, req).await;
            assert_single_error(&raw, 400, name);
        }
        assert_eq!(HITS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tcp_invalid_content_length_is_rejected() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        for value in [
            "abc",
            "+5",
            "-1",
            "5, 5",
            "0x5",
            "",
            "99999999999999999999999",
        ] {
            let req = format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {value}\r\n\r\nhello"
            );
            let raw = raw_exchange(addr, req.as_bytes()).await;
            assert_single_error(&raw, 400, &format!("content-length {value:?}"));
        }
    }

    #[tokio::test]
    async fn tcp_invalid_or_unsupported_transfer_encoding_is_rejected() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        for te in [
            "Transfer-Encoding: gzip\r\n",
            "Transfer-Encoding: identity\r\n",
            "Transfer-Encoding: gzip, chunked\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
            "Transfer-Encoding: chunked, \r\n",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        ] {
            let req = format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\n{te}\r\n5\r\nhello\r\n0\r\n\r\n"
            );
            let raw = raw_exchange(addr, req.as_bytes()).await;
            assert_single_error(&raw, 400, te.trim_end());
        }
    }

    #[tokio::test]
    async fn tcp_http10_post_with_content_length_is_served_then_closed() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.0\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
        )
        .await;

        let (status, headers, body) = parse_raw_http_response(&raw);
        assert_eq!(status, 200);
        assert_eq!(body, b"got 5 bytes");
        assert_eq!(header_val(&headers, "connection"), Some("close"));
    }

    #[tokio::test]
    async fn tcp_http10_body_without_content_length_or_with_chunked_is_rejected() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        for (name, req) in [
            (
                "POST without length",
                &b"POST /echo HTTP/1.0\r\nHost: localhost\r\n\r\nhello"[..],
            ),
            (
                "chunked on HTTP/1.0",
                b"POST /echo HTTP/1.0\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            ),
        ] {
            let raw = raw_exchange(addr, req).await;
            assert_single_error(&raw, 400, name);
        }
    }

    #[tokio::test]
    #[ignore = "bug #100: streaming responses to HTTP/1.0 clients use chunked transfer-encoding, which HTTP/1.0 does not define (RFC 9112 §6.1); they must be close-delimited"]
    async fn tcp_http10_streaming_response_is_not_chunked() {
        let addr = start_server(|| {
            App::new().get("/stream", |_req: Request| async {
                one_chunk_stream(b"hello")
            })
        })
        .await;

        let raw = raw_exchange(addr, b"GET /stream HTTP/1.0\r\nHost: localhost\r\n\r\n").await;

        let (status, headers, body) = parse_raw_http_response(&raw);
        assert_eq!(status, 200);
        assert_eq!(header_val(&headers, "transfer-encoding"), None);
        assert_eq!(body, b"hello");
    }

    // -- Chunked request bodies ---------------------------------------------

    #[tokio::test]
    async fn tcp_chunked_body_framing_errors_get_400() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        for (name, body) in [
            ("bad CRLF after chunk data", &b"5\r\nhelloXX0\r\n\r\n"[..]),
            ("chunk longer than its size", b"5\r\nhello!\r\n0\r\n\r\n"),
            ("bad final CRLF", b"5\r\nhello\r\n0\r\nXX"),
            (
                "chunk size overflow",
                b"10000000000000000\r\nx\r\n0\r\n\r\n",
            ),
            ("non-hex chunk size", b"zz\r\nhello\r\n0\r\n\r\n"),
        ] {
            let mut req =
                b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n"
                    .to_vec();
            req.extend_from_slice(body);
            let raw = raw_exchange(addr, &req).await;
            assert_single_error(&raw, 400, name);
        }
    }

    #[tokio::test]
    async fn tcp_eof_mid_chunk_closes_without_response() {
        let addr = start_server(|| App::new().post("/echo", echo_body)).await;

        for partial in [&b"5\r\nhel"[..], b"5\r\nhello\r\n", b"5\r\nhello\r\n0\r\n"] {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            stream.write_all(partial).await.unwrap();
            stream.shutdown().await.unwrap();

            let raw = read_until_close(&mut stream).await;
            assert!(
                raw.is_empty(),
                "truncated chunked body {:?} must not produce a response: {:?}",
                String::from_utf8_lossy(partial),
                String::from_utf8_lossy(&raw)
            );
        }
    }

    #[tokio::test]
    async fn tcp_chunked_body_over_limit_across_chunks_gets_413() {
        let addr = start_server_with_config(
            || App::new().post("/echo", echo_body),
            harrow_server_tokio::ServerConfig {
                max_body_size: 10,
                ..Default::default()
            },
        )
        .await;

        // No single chunk exceeds the limit; the running total does.
        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n5\r\nagain\r\n0\r\n\r\n",
        )
        .await;

        assert_single_error(&raw, 413, "chunked body over limit");
    }

    // -- Response writing ---------------------------------------------------

    #[tokio::test]
    async fn tcp_app_set_transfer_encoding_is_replaced_by_server_framing() {
        let addr = start_server(|| {
            App::new().get("/stream", |_req: Request| async {
                one_chunk_stream(b"hello").header("transfer-encoding", "gzip")
            })
        })
        .await;

        let raw = raw_exchange(
            addr,
            b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        let (status, headers, body, end) = try_parse_response(&raw, true).unwrap();
        assert_eq!(status, 200);
        let te: Vec<_> = headers
            .iter()
            .filter(|(k, _)| k == "transfer-encoding")
            .collect();
        assert_eq!(te.len(), 1, "exactly one transfer-encoding: {headers:?}");
        assert_eq!(te[0].1, "chunked");
        assert_eq!(body, b"hello");
        assert_eq!(end, raw.len(), "no bytes after the chunked terminator");
    }

    #[tokio::test]
    async fn tcp_app_set_content_length_and_transfer_encoding_uses_content_length() {
        let addr = start_server(|| {
            App::new()
                .get("/both", |_req: Request| async {
                    Response::text("hello").header("transfer-encoding", "chunked")
                })
                .get("/hello", hello)
        })
        .await;

        // Pipeline a second request: if the response framing were ambiguous
        // the second response would be misread.
        let raw = raw_exchange(
            addr,
            b"GET /both HTTP/1.1\r\nHost: localhost\r\n\r\nGET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        let (status, headers, body, end) = try_parse_response(&raw, true).unwrap();
        assert_eq!(status, 200);
        assert_eq!(header_val(&headers, "transfer-encoding"), None);
        assert_eq!(header_val(&headers, "content-length"), Some("5"));
        assert_eq!(body, b"hello");

        let (status, _, body, end2) = try_parse_response(&raw[end..], true).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
        assert_eq!(end + end2, raw.len());
    }

    #[tokio::test]
    async fn tcp_wrong_fixed_content_length_closes_without_writing_a_response() {
        let addr = start_server(|| {
            App::new()
                .get("/declared-short", |_req: Request| async {
                    Response::text("hello").header("content-length", "4")
                })
                .get("/declared-long", |_req: Request| async {
                    Response::text("hello").header("content-length", "10")
                })
                .get("/stream-overrun", |_req: Request| async {
                    one_chunk_stream(b"hello").header("content-length", "3")
                })
                .get("/stream-underrun", |_req: Request| async {
                    one_chunk_stream(b"hello").header("content-length", "10")
                })
        })
        .await;

        // A mismatched length must never reach the wire: a client (or proxy)
        // would desynchronise on the next response.
        for path in [
            "/declared-short",
            "/declared-long",
            "/stream-overrun",
            "/stream-underrun",
        ] {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let raw = raw_exchange(addr, req.as_bytes()).await;
            assert!(
                raw.is_empty(),
                "{path}: expected connection close without a response, got {:?}",
                String::from_utf8_lossy(&raw)
            );
        }
    }

    #[tokio::test]
    async fn tcp_bodyless_responses_keep_pipelined_connection_in_sync() {
        let addr = start_server(|| {
            App::new()
                .get("/hello", hello)
                .get("/stream", |_req: Request| async {
                    one_chunk_stream(b"hidden")
                })
                .get("/no-content", |_req: Request| async {
                    Response::text("hidden").status(204)
                })
                .get("/not-modified", |_req: Request| async {
                    Response::text("hidden").status(304)
                })
        })
        .await;

        let raw = raw_exchange(
            addr,
            b"HEAD /hello HTTP/1.1\r\nHost: localhost\r\n\r\n\
              HEAD /stream HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /no-content HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /not-modified HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        let mut rest = &raw[..];
        for (name, expected) in [
            ("HEAD fixed", 200),
            ("HEAD streaming", 200),
            ("204", 204),
            ("304", 304),
        ] {
            let (status, headers, _, end) = try_parse_response(rest, false).unwrap();
            assert_eq!(status, expected, "{name}");
            assert_eq!(header_val(&headers, "transfer-encoding"), None, "{name}");
            rest = &rest[end..];
            assert!(
                rest.starts_with(b"HTTP/1.1 "),
                "{name}: body bytes leaked onto the wire: {:?}",
                String::from_utf8_lossy(rest)
            );
        }
        let (status, _, body, end) = try_parse_response(rest, true).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
        assert_eq!(end, rest.len());
    }

    #[tokio::test]
    async fn tcp_streaming_response_error_mid_body_truncates_and_closes() {
        let addr = start_server(|| {
            App::new().get("/fail", |_req: Request| async {
                let stream = futures_util::stream::iter([
                    Ok(http_body::Frame::data(bytes::Bytes::from_static(b"first"))),
                    Err(Box::<dyn std::error::Error + Send + Sync>::from("boom")),
                ]);
                Response::streaming(StatusCode::OK, stream)
            })
        })
        .await;

        // Keep-alive request: the server must still close, because the
        // response cannot be completed.
        let raw = raw_exchange(addr, b"GET /fail HTTP/1.1\r\nHost: localhost\r\n\r\n").await;

        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("5\r\nfirst\r\n"), "{text}");
        assert!(
            try_parse_response(&raw, true).is_none(),
            "a failed stream must not be terminated like a complete body: {text}"
        );
    }

    // -- Lifecycle ----------------------------------------------------------

    #[tokio::test]
    async fn tcp_keep_alive_reuses_connection_for_sequential_requests() {
        let addr = start_server(|| App::new().get("/hello", hello).post("/echo", echo_body)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut pending = Vec::new();
        for i in 0..3 {
            stream
                .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let (status, headers, body) = read_one_response(&mut stream, &mut pending, true).await;
            assert_eq!(status, 200, "get {i}");
            assert_eq!(body, b"hello", "get {i}");
            assert_eq!(header_val(&headers, "connection"), None, "get {i}");

            stream
                .write_all(
                    b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabc",
                )
                .await
                .unwrap();
            let (status, _, body) = read_one_response(&mut stream, &mut pending, true).await;
            assert_eq!(status, 200, "post {i}");
            assert_eq!(body, b"got 3 bytes", "post {i}");
        }
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn tcp_pipelined_requests_after_bodies_are_parsed_from_buffer() {
        let addr = start_server(|| App::new().get("/hello", hello).post("/echo", echo_body)).await;

        // Three requests in one write: the body boundary of each must be
        // exact for the next request line to parse.
        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello\
              POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4\r\ndefg\r\n0\r\n\r\n\
              GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;

        let mut rest = &raw[..];
        for expected in [&b"got 5 bytes"[..], b"got 7 bytes", b"hello"] {
            let (status, _, body, end) = try_parse_response(rest, true).unwrap();
            assert_eq!(status, 200);
            assert_eq!(body, expected);
            rest = &rest[end..];
        }
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn tcp_oversized_content_length_gets_413_before_body_is_sent() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let addr = start_server_with_config(
            || {
                App::new().post("/echo", |_req: Request| async {
                    HITS.fetch_add(1, Ordering::SeqCst);
                    Response::text("dispatched")
                })
            },
            harrow_server_tokio::ServerConfig {
                max_body_size: 16,
                ..Default::default()
            },
        )
        .await;

        // Only the head is sent; the server must answer without waiting for
        // (or reading) a body it will never accept.
        let raw = raw_exchange(
            addr,
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\n\r\n",
        )
        .await;

        assert_single_error(&raw, 413, "oversized content-length");
        assert_eq!(HITS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tcp_early_response_without_reading_body_closes_connection() {
        let addr = start_server(|| {
            App::new().post("/early", |_req: Request| async { Response::text("early") })
        })
        .await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        // Keep-alive request whose body is still in flight when the handler
        // answers. The unread body makes the connection unusable for reuse.
        stream
            .write_all(
                b"POST /early HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nonly-part",
            )
            .await
            .unwrap();
        let mut pending = Vec::new();
        let (status, headers, body) = read_one_response(&mut stream, &mut pending, true).await;
        assert_eq!(status, 200);
        assert_eq!(body, b"early");
        assert_eq!(header_val(&headers, "connection"), Some("close"));

        // Finish the body; the server drains it and then closes.
        stream.write_all(&[b'x'; 91]).await.unwrap();
        let rest = read_until_close(&mut stream).await;
        assert!(
            pending.is_empty() && rest.is_empty(),
            "no second response expected: {:?}",
            String::from_utf8_lossy(&rest)
        );
    }

    #[tokio::test]
    async fn tcp_slow_header_timeout_closes_connection() {
        let addr = start_server_with_config(
            || App::new().get("/", hello),
            harrow_server_tokio::ServerConfig {
                header_read_timeout: Some(Duration::from_millis(200)),
                ..Default::default()
            },
        )
        .await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let started = std::time::Instant::now();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();

        let raw = read_until_close(&mut stream).await;
        let elapsed = started.elapsed();
        assert!(
            raw.is_empty(),
            "header timeout closes silently: {:?}",
            String::from_utf8_lossy(&raw)
        );
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(2),
            "closed after {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn tcp_slow_chunked_body_timeout_gets_408() {
        let addr = start_server_with_config(
            || App::new().post("/echo", echo_body),
            harrow_server_tokio::ServerConfig {
                body_read_timeout: Some(Duration::from_millis(200)),
                ..Default::default()
            },
        )
        .await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
            )
            .await
            .unwrap();

        let raw = read_until_close(&mut stream).await;
        assert_single_error(&raw, 408, "stalled chunked body");
    }

    #[tokio::test]
    async fn tcp_shutdown_during_keep_alive_request_completes_then_closes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = spawn_server_thread(
            || App::new().get("/slow", slow_handler),
            addr,
            rx,
            harrow_server_tokio::ServerConfig {
                drain_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(());

        // The in-flight keep-alive request completes, then the connection is
        // closed instead of waiting for another request.
        let mut pending = Vec::new();
        let (status, _, body) = read_one_response(&mut stream, &mut pending, true).await;
        assert_eq!(status, 200);
        assert_eq!(body, b"slow");
        let rest = read_until_close(&mut stream).await;
        assert!(pending.is_empty() && rest.is_empty());

        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || server.join().unwrap()),
        )
        .await
        .expect("server should stop once the in-flight request finished")
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "bug #98: ShutdownSignal is a polled flag, so an idle keep-alive connection blocked in read_request_head is not woken; shutdown waits for header_read_timeout or drain_timeout"]
    async fn tcp_shutdown_closes_idle_keep_alive_connection_promptly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = spawn_server_thread(
            || App::new().get("/hello", hello),
            addr,
            rx,
            harrow_server_tokio::ServerConfig {
                header_read_timeout: Some(Duration::from_secs(10)),
                drain_timeout: Duration::from_secs(10),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut pending = Vec::new();
        let (status, _, _) = read_one_response(&mut stream, &mut pending, true).await;
        assert_eq!(status, 200);

        // The connection is now idle between requests.
        let _ = tx.send(());
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || server.join().unwrap()),
        )
        .await
        .expect("shutdown should not wait for idle keep-alive connections")
        .unwrap();
    }
} // mod h1_adversarial
