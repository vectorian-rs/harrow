use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
extern crate http;
use harrow_core::timeout::timeout_middleware;
use harrow_o11y::O11yConfig;
use harrow_o11y::o11y_middleware::o11y_middleware;
use http::StatusCode;

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
    let counter = req.state::<Arc<HitCounter>>();
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
    assert_eq!(methods, vec!["DELETE", "GET", "POST"]);
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

async fn panicking_handler(_req: Request) -> Response {
    panic!("handler bug");
}

#[tokio::test]
async fn tcp_panic_in_handler_returns_500() {
    let app = App::new().get("/boom", panicking_handler);
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
    assert_eq!(rid.len(), 32, "expected 32-char hex trace ID");
    assert!(
        rid.chars().all(|c| c.is_ascii_hexdigit()),
        "expected hex characters only"
    );
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
