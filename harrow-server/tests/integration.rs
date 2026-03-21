use std::net::SocketAddr;
use std::sync::Arc;
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
use harrow_middleware::rate_limit::{HeaderKeyExtractor, InMemoryBackend, rate_limit_middleware};
use harrow_middleware::session::{
    InMemorySessionStore, Session, SessionConfig, session_middleware,
};
use harrow_middleware::timeout::timeout_middleware;
use harrow_o11y::O11yConfig;
use http::StatusCode;

// HTTP/2 h2c imports
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http2 as h2_client;
use hyper_util::rt::{TokioExecutor, TokioIo};

/// Shared counter used as application state.
struct HitCounter(AtomicUsize);

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

/// Spin up the server on a random port, return the bound address.
async fn start_server(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        harrow_server::serve_with_shutdown(app, addr, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    std::mem::forget(tx);
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

#[tokio::test]
async fn client_timeout_middleware_returns_408_on_slow_handler() {
    let client = App::new()
        .middleware(timeout_middleware(Duration::from_millis(50)))
        .get("/slow", slow_handler)
        .client();

    let resp = client.get("/slow").await;
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(resp.text(), "request timeout");
}

#[tokio::test]
async fn client_timeout_middleware_passes_fast_handler() {
    let client = App::new()
        .middleware(timeout_middleware(Duration::from_secs(1)))
        .get("/hello", hello)
        .client();

    let resp = client.get("/hello").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text(), "hello");
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
    let app = App::new().get("/hello", hello).get("/greet/:name", greet);
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
    let app = App::new()
        .state(Arc::new(String::from("primary-db")))
        .health("/health")
        .liveness("/live")
        .readiness_handler("/ready", |req| async move {
            let db = req.require_state::<Arc<String>>().unwrap();
            Response::text(format!("ready {}", db.as_str()))
        });
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
    let app = App::new().get("/boom", panicking_handler);
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/boom").await;
    assert_eq!(status, 0);
    assert!(body.is_empty());
}

#[tokio::test]
async fn tcp_catch_panic_middleware_returns_500() {
    let app = App::new()
        .middleware(catch_panic_middleware)
        .get("/boom", panicking_handler);
    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/boom").await;
    assert_eq!(status, 500);
    assert_eq!(body, "internal server error");
}

#[tokio::test]
async fn tcp_o11y_middleware_adds_request_id_header() {
    let app = App::new()
        .state(Arc::new(O11yConfig::default()))
        .middleware(o11y_middleware)
        .get("/hello", hello);

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
    let app = App::new()
        .state(Arc::new(O11yConfig::default()))
        .middleware(o11y_middleware)
        .get("/hello", hello);

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
    let app = App::new().get("/slow", slow_handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        harrow_server::serve_with_config(
            app,
            addr,
            async {
                let _ = rx.await;
            },
            harrow_server::ServerConfig {
                drain_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    });

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
}

#[tokio::test]
async fn tcp_head_returns_get_headers_without_body() {
    let app = App::new().get("/hello", hello);
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
async fn tcp_default_problem_details_formats_404_and_405() {
    let app = App::new().default_problem_details().get("/users", hello);
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
    let app = App::new()
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
        });
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
    let app = App::new().default_problem_details().post("/submit", hello);
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
    let app = App::new()
        .middleware(body_limit_middleware(50))
        .post("/upload", echo_body);
    let addr = start_server(app).await;

    let body = vec![b'x'; 200];
    let (status, _, _) = http_post(addr, "/upload", &body).await;
    assert_eq!(status, 413);
}

#[tokio::test]
async fn tcp_body_limit_allows_small() {
    let app = App::new()
        .middleware(body_limit_middleware(1024))
        .post("/upload", echo_body);
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
// ============================================================================

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
