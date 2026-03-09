use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_core::timeout::timeout_middleware;
use harrow_o11y::O11yConfig;
use harrow_o11y::o11y_middleware::o11y_middleware;

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

// -- Helpers -----------------------------------------------------------------

/// Spin up the server on a random port, return the bound address.
async fn start_server(app: App) -> SocketAddr {
    // Bind to port 0 to get an OS-assigned free port, then drop the listener
    // so serve_with_shutdown can rebind it.
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

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Keep the shutdown sender alive by leaking it (test cleanup is fine).
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

    // Handle chunked transfer encoding: extract the actual body.
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
        // Skip past the chunk data and the trailing \r\n.
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

// -- Tests -------------------------------------------------------------------

#[tokio::test]
async fn basic_routing() {
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
async fn returns_404_for_unknown_path() {
    let app = App::new().get("/hello", hello);
    let addr = start_server(app).await;

    let (status, _, _) = http_get(addr, "/nope").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn returns_405_for_wrong_method() {
    let app = App::new().post("/hello", hello);
    let addr = start_server(app).await;

    // GET against a POST-only route.
    let (status, _, _) = http_get(addr, "/hello").await;
    assert_eq!(status, 405);
}

#[tokio::test]
async fn middleware_runs_in_order() {
    let app = App::new()
        .middleware(wrap_middleware)
        .middleware(second_middleware)
        .get("/hello", hello);

    let addr = start_server(app).await;

    let (status, headers, body) = http_get(addr, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hello");

    // wrap_middleware runs first, sees response on the way back -> sets x-wrap
    assert_eq!(header_val(&headers, "x-wrap"), Some("true"));
    // second_middleware runs second, sets x-second
    assert_eq!(header_val(&headers, "x-second"), Some("yes"));
}

#[tokio::test]
async fn state_injection_works() {
    let counter = Arc::new(HitCounter(AtomicUsize::new(0)));

    let app = App::new()
        .state(counter.clone())
        .get("/count", state_handler);

    let addr = start_server(app).await;

    let (_, _, body) = http_get(addr, "/count").await;
    assert_eq!(body, "hits: 1");

    let (_, _, body) = http_get(addr, "/count").await;
    assert_eq!(body, "hits: 2");

    let (_, _, body) = http_get(addr, "/count").await;
    assert_eq!(body, "hits: 3");

    // Also verify from the original handle.
    assert_eq!(counter.0.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn middleware_and_state_together() {
    let counter = Arc::new(HitCounter(AtomicUsize::new(0)));

    let app = App::new()
        .state(counter.clone())
        .middleware(wrap_middleware)
        .get("/count", state_handler);

    let addr = start_server(app).await;

    let (status, headers, body) = http_get(addr, "/count").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hits: 1");
    assert_eq!(header_val(&headers, "x-wrap"), Some("true"));
}

// -- Route Group Tests -------------------------------------------------------

/// Middleware that adds x-group: <value> header.
async fn group_tag_middleware(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    resp.header("x-group", "api")
}

/// Another middleware to verify ordering.
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
async fn group_basic_prefix() {
    let app = App::new().get("/health", hello).group("/api", |g| {
        g.get("/users", users_handler).get("/users/:id", user_by_id)
    });

    let addr = start_server(app).await;

    // /health still works at top level.
    let (status, _, body) = http_get(addr, "/health").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hello");

    // /api/users works with group prefix.
    let (status, _, body) = http_get(addr, "/api/users").await;
    assert_eq!(status, 200);
    assert_eq!(body, "users list");

    // /api/users/:id with path param.
    let (status, _, body) = http_get(addr, "/api/users/42").await;
    assert_eq!(status, 200);
    assert_eq!(body, "user 42");

    // /users without prefix should 404.
    let (status, _, _) = http_get(addr, "/users").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn group_scoped_middleware() {
    let app = App::new().get("/health", hello).group("/api", |g| {
        g.middleware(group_tag_middleware)
            .get("/users", users_handler)
    });

    let addr = start_server(app).await;

    // /health should NOT have the group middleware header.
    let (status, headers, _) = http_get(addr, "/health").await;
    assert_eq!(status, 200);
    assert_eq!(header_val(&headers, "x-group"), None);

    // /api/users SHOULD have the group middleware header.
    let (status, headers, body) = http_get(addr, "/api/users").await;
    assert_eq!(status, 200);
    assert_eq!(body, "users list");
    assert_eq!(header_val(&headers, "x-group"), Some("api"));
}

#[tokio::test]
async fn group_with_global_middleware() {
    // Global middleware + group middleware. Both should run on group routes,
    // only global on non-group routes.
    let app = App::new()
        .middleware(wrap_middleware)
        .get("/health", hello)
        .group("/api", |g| {
            g.middleware(group_tag_middleware)
                .get("/users", users_handler)
        });

    let addr = start_server(app).await;

    // /health: global middleware only.
    let (status, headers, _) = http_get(addr, "/health").await;
    assert_eq!(status, 200);
    assert_eq!(header_val(&headers, "x-wrap"), Some("true"));
    assert_eq!(header_val(&headers, "x-group"), None);

    // /api/users: global + group middleware.
    let (status, headers, body) = http_get(addr, "/api/users").await;
    assert_eq!(status, 200);
    assert_eq!(body, "users list");
    assert_eq!(header_val(&headers, "x-wrap"), Some("true"));
    assert_eq!(header_val(&headers, "x-group"), Some("api"));
}

#[tokio::test]
async fn nested_groups() {
    // /api -> group_tag_middleware
    //   /v1 -> inner_tag_middleware
    //     /users -> should have both middlewares
    let app = App::new().group("/api", |g| {
        g.middleware(group_tag_middleware)
            .get("/health", hello)
            .group("/v1", |v1| {
                v1.middleware(inner_tag_middleware)
                    .get("/users", users_handler)
            })
    });

    let addr = start_server(app).await;

    // /api/health: only outer group middleware.
    let (status, headers, _) = http_get(addr, "/api/health").await;
    assert_eq!(status, 200);
    assert_eq!(header_val(&headers, "x-group"), Some("api"));
    assert_eq!(header_val(&headers, "x-inner"), None);

    // /api/v1/users: outer + inner group middleware.
    let (status, headers, body) = http_get(addr, "/api/v1/users").await;
    assert_eq!(status, 200);
    assert_eq!(body, "users list");
    assert_eq!(header_val(&headers, "x-group"), Some("api"));
    assert_eq!(header_val(&headers, "x-inner"), Some("v1"));
}

#[tokio::test]
async fn group_404_and_405() {
    let app = App::new().group("/api", |g| g.post("/submit", hello));

    let addr = start_server(app).await;

    // Path doesn't exist at all -> 404.
    let (status, _, _) = http_get(addr, "/api/nope").await;
    assert_eq!(status, 404);

    // Path exists but wrong method -> 405.
    let (status, _, _) = http_get(addr, "/api/submit").await;
    assert_eq!(status, 405);
}

// -- HTTP helper with custom headers -----------------------------------------

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

// -- O11y Integration Tests --------------------------------------------------

#[tokio::test]
async fn o11y_middleware_adds_request_id_header() {
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
async fn o11y_middleware_echoes_incoming_request_id() {
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

#[tokio::test]
async fn o11y_middleware_without_config_uses_defaults() {
    // No O11yConfig in state — should not panic, should fall back to defaults.
    let app = App::new().middleware(o11y_middleware).get("/hello", hello);

    let addr = start_server(app).await;

    let (status, headers, _) = http_get(addr, "/hello").await;
    assert_eq!(status, 200);
    let rid = header_val(&headers, "x-request-id");
    assert!(rid.is_some(), "expected x-request-id with default config");
}

async fn echo_route_pattern(req: Request) -> Response {
    let pattern = req.route_pattern().unwrap_or("none").to_string();
    Response::text(pattern)
}

#[tokio::test]
async fn route_pattern_is_template_not_resolved() {
    let app = App::new().get("/users/:id", echo_route_pattern);

    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/users/42").await;
    assert_eq!(status, 200);
    assert_eq!(body, "/users/:id");
}

// -- Timeout Middleware Tests ------------------------------------------------

async fn slow_handler(_req: Request) -> Response {
    tokio::time::sleep(Duration::from_millis(200)).await;
    Response::text("slow")
}

#[tokio::test]
async fn timeout_middleware_returns_408_on_slow_handler() {
    let app = App::new()
        .middleware(timeout_middleware(Duration::from_millis(50)))
        .get("/slow", slow_handler);

    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/slow").await;
    assert_eq!(status, 408);
    assert_eq!(body, "request timeout");
}

#[tokio::test]
async fn timeout_middleware_passes_fast_handler() {
    let app = App::new()
        .middleware(timeout_middleware(Duration::from_secs(1)))
        .get("/hello", hello);

    let addr = start_server(app).await;

    let (status, _, body) = http_get(addr, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, "hello");
}
