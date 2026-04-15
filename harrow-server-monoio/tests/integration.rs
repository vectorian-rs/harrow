use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use http::StatusCode;

// -- Handlers ----------------------------------------------------------------

async fn hello(_req: Request) -> Response {
    Response::text("hello")
}

async fn echo_body(req: Request) -> Response {
    let body = req.body_bytes().await.unwrap();
    Response::new(StatusCode::OK, body)
}

async fn greet(req: Request) -> Response {
    let name = req.param("name");
    Response::text(format!("hello, {name}"))
}

/// Shared counter used as application state.
struct HitCounter(AtomicUsize);

async fn state_handler(req: Request) -> Response {
    let counter = req.require_state::<Arc<HitCounter>>().unwrap();
    let count = counter.0.fetch_add(1, Ordering::Relaxed) + 1;
    Response::text(format!("hits: {count}"))
}

// -- Helpers -----------------------------------------------------------------

/// Start a monoio server with Harrow's public thread-per-core bootstrap.
fn start_monoio_server<F>(make_app: F) -> (SocketAddr, harrow_server_monoio::ServerHandle)
where
    F: Fn() -> App + Send + Clone + 'static,
{
    let server = harrow_server_monoio::start_with_config(
        make_app,
        "127.0.0.1:0".parse().unwrap(),
        harrow_server_monoio::ServerConfig {
            workers: Some(2),
            header_read_timeout: Some(Duration::from_secs(2)),
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_secs(30)),
            drain_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .unwrap();

    let addr = server.local_addr();
    // Give the server a moment to start accepting.
    std::thread::sleep(Duration::from_millis(100));
    (addr, server)
}

/// Simple HTTP/1.1 request via raw TCP using tokio, returns (status, headers, body).
async fn http_request(addr: SocketAddr, request: &str) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();

    parse_http_response(&buf)
}

/// Parse a raw HTTP response into (status, headers, body).
fn parse_http_response(buf: &[u8]) -> (u16, Vec<(String, String)>, String) {
    let raw = String::from_utf8_lossy(buf);

    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body_raw = parts.next().unwrap_or("");

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

    // Handle chunked transfer-encoding
    let is_chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));

    let body = if is_chunked {
        decode_chunked_body(body_raw)
    } else {
        body_raw.to_string()
    };

    (status, headers, body)
}

/// Decode a chunked HTTP body from a string.
fn decode_chunked_body(raw: &str) -> String {
    let mut result = String::new();
    let mut remaining = raw;
    while let Some(crlf) = remaining.find("\r\n") {
        let size_str = &remaining[..crlf];
        let size = match usize::from_str_radix(size_str.trim(), 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        if size == 0 {
            break;
        }
        remaining = &remaining[crlf + 2..];
        if remaining.len() < size {
            break;
        }
        result.push_str(&remaining[..size]);
        remaining = &remaining[size..];
        if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
        }
    }
    result
}

fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// -- Tests -------------------------------------------------------------------

#[tokio::test]
async fn basic_get() {
    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let req = "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(body, "hello");
}

#[tokio::test]
async fn post_with_body() {
    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_monoio_server(app);

    let body_data = "test body data";
    let req = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body_data.len(),
        body_data
    );
    let (status, _headers, body) = http_request(addr, &req).await;

    assert_eq!(status, 200);
    assert_eq!(body, body_data);
}

#[tokio::test]
async fn request_body_streams_before_request_eof() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let (addr, _server) = start_monoio_server(app);

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
async fn path_params() {
    let app = || App::new().get("/greet/:name", greet);
    let (addr, _server) = start_monoio_server(app);

    let req = "GET /greet/world HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(body, "hello, world");
}

#[tokio::test]
async fn not_found_404() {
    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let req = "GET /nonexistent HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 404);
    assert_eq!(body, "not found");
}

#[tokio::test]
async fn method_not_allowed_405() {
    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let req =
        "POST /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    let (status, headers, _body) = http_request(addr, req).await;

    assert_eq!(status, 405);
    let allow = get_header(&headers, "allow").unwrap();
    assert!(allow.contains("GET"));
}

#[tokio::test]
async fn keep_alive_multiple_requests() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // First request (keep-alive by default in HTTP/1.1)
    let req1 = "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(req1.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second request on the same connection
    let req2 = "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req2.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    // Should have received two HTTP responses
    let response_count = raw.matches("HTTP/1.1 200").count();
    assert_eq!(response_count, 2, "expected 2 responses, got raw: {raw}");
}

#[tokio::test]
async fn connection_close_header() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let req = "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 200);
    assert_eq!(body, "hello");
}

#[tokio::test]
async fn head_response_has_no_body_or_chunked_framing() {
    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let req = "HEAD /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(body, "");
    assert_eq!(get_header(&headers, "transfer-encoding"), None);
}

#[tokio::test]
async fn application_state() {
    let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
    let app = move || {
        App::new()
            .state(counter.clone())
            .get("/count", state_handler)
    };
    let (addr, _server) = start_monoio_server(app);

    let req = "GET /count HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(body, "hits: 1");

    // Second request
    let (status, _headers, body) = http_request(addr, req).await;
    assert_eq!(status, 200);
    assert_eq!(body, "hits: 2");
}

#[tokio::test]
async fn body_limit_content_length_rejection() {
    let app = || App::new().max_body_size(10).post("/echo", echo_body);
    let (addr, _server) = start_monoio_server(app);

    // Content-Length exceeds limit — dispatch rejects before handler reads body.
    let payload = "x".repeat(100);
    let req = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 100\r\n\r\n{}",
        payload
    );
    let (status, _headers, body) = http_request(addr, &req).await;

    assert_eq!(status, 413);
    assert_eq!(body, "payload too large");
}

#[tokio::test]
async fn body_limit_chunked_rejection() {
    let app = || App::new().max_body_size(5).post("/echo", echo_body);
    let (addr, _server) = start_monoio_server(app);

    let req = "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 413);
    assert_eq!(body, "payload too large");
}

#[tokio::test]
async fn rejects_content_length_and_chunked_together() {
    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_monoio_server(app);

    let req = "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 400);
    assert_eq!(body, "bad request");
}

#[tokio::test]
async fn malformed_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"NOT A VALID HTTP REQUEST\r\n\r\n")
        .await
        .unwrap();

    // Server should send 400 Bad Request and close.
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 400);
    assert_eq!(body, "bad request");
}

#[tokio::test]
async fn large_response_chunked() {
    async fn large_body(_req: Request) -> Response {
        let data = "x".repeat(100_000);
        Response::text(data)
    }

    let app = || App::new().get("/large", large_body);
    let (addr, _server) = start_monoio_server(app);

    let req = "GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(body.len(), 100_000);
    assert!(body.chars().all(|c| c == 'x'));
}

#[tokio::test]
async fn header_read_timeout() {
    use tokio::io::AsyncReadExt;

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_monoio_server(app);

    // Connect but send nothing — should timeout and close.
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Wait for the server to close the connection (header_read_timeout = 2s in test config).
    let mut buf = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    assert!(result.is_ok(), "connection should close within timeout");
}

#[tokio::test]
async fn body_read_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_monoio_server(app);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 10\r\n\r\nhello";
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    assert!(
        result.is_ok(),
        "body read timeout should close the connection"
    );

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 408);
    assert_eq!(body, "request timeout");
}

#[tokio::test]
async fn graceful_shutdown_drains_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn slow_handler(_req: Request) -> Response {
        // Simulate slow processing — this uses tokio::time but we're running in
        // a monoio context. We use a sync sleep via std::thread::sleep in the handler
        // which blocks the monoio thread. For a real handler this is suboptimal
        // but serves to test the drain logic.
        std::thread::sleep(Duration::from_millis(500));
        Response::text("done")
    }

    let app = || App::new().get("/slow", slow_handler);
    let (addr, server) = start_monoio_server(app);

    // Start a request
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();

    // Immediately signal shutdown
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(server);

    // The in-flight request should still complete.
    let mut buf = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf)).await;
    assert!(
        result.is_ok(),
        "should receive response before drain timeout"
    );

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 200);
    assert_eq!(body, "done");
}
