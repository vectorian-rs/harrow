#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use http::StatusCode;

async fn hello(_req: Request) -> Response {
    Response::text("hello")
}

async fn echo_body(req: Request) -> Response {
    let body = req.body_bytes().await.unwrap();
    Response::new(StatusCode::OK, body)
}

fn start_meguri_server<F>(
    make_app: F,
    config: harrow_server_meguri::ServerConfig,
) -> (SocketAddr, harrow_server_meguri::ServerHandle)
where
    F: Fn() -> App + Send + Clone + 'static,
{
    let server =
        harrow_server_meguri::start_with_config(make_app, "127.0.0.1:0".parse().unwrap(), config)
            .unwrap();
    let addr = server.local_addr();
    std::thread::sleep(Duration::from_millis(100));
    (addr, server)
}

fn test_config() -> harrow_server_meguri::ServerConfig {
    harrow_server_meguri::ServerConfig {
        workers: Some(2),
        header_read_timeout: Some(Duration::from_millis(250)),
        body_read_timeout: Some(Duration::from_millis(250)),
        connection_timeout: Some(Duration::from_secs(30)),
        drain_timeout: Duration::from_secs(5),
        max_body_size: 1024,
        ..Default::default()
    }
}

async fn http_request(addr: SocketAddr, request: &str) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();

    parse_http_response(&buf)
}

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

#[tokio::test]
async fn head_response_is_not_chunked_on_wire() {
    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_meguri_server(app, test_config());

    let req = "HEAD /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (status, headers, body) = http_request(addr, req).await;

    assert_eq!(status, 200);
    assert_eq!(get_header(&headers, "transfer-encoding"), None);
    assert!(body.is_empty(), "HEAD response should not write body bytes");
}

#[tokio::test]
async fn responses_204_and_304_are_bodyless_on_wire() {
    let app = || {
        App::new()
            .get("/no-content", |_req: Request| async move {
                Response::text("hidden").status(StatusCode::NO_CONTENT.as_u16())
            })
            .get("/not-modified", |_req: Request| async move {
                Response::text("hidden").status(StatusCode::NOT_MODIFIED.as_u16())
            })
    };
    let (addr, _server) = start_meguri_server(app, test_config());

    for (path, expected_status) in [
        ("/no-content", StatusCode::NO_CONTENT),
        ("/not-modified", StatusCode::NOT_MODIFIED),
    ] {
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let (status, headers, body) = http_request(addr, &req).await;
        assert_eq!(status, expected_status.as_u16(), "{path}");
        assert_eq!(
            get_header(&headers, "transfer-encoding"),
            None,
            "{path} should not be chunked"
        );
        assert!(body.is_empty(), "{path} should not write body bytes");
    }
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
    let (addr, _server) = start_meguri_server(app, test_config());

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
}

#[tokio::test]
async fn keep_alive_pipelining_reuses_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || {
        App::new()
            .get("/one", |_req: Request| async move { Response::text("one") })
            .get("/two", |_req: Request| async move { Response::text("two") })
    };
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let raw = String::from_utf8_lossy(&buf);

    assert_eq!(raw.match_indices("HTTP/1.1 200").count(), 2, "{raw}");
    let first = raw.find("one").expect("first response body missing");
    let second = raw.find("two").expect("second response body missing");
    assert!(
        first < second,
        "responses should preserve pipeline order: {raw}"
    );
}

#[tokio::test]
async fn header_read_timeout_closes_connection() {
    use tokio::io::AsyncReadExt;

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .expect("connection should close within timeout")
        .unwrap();
}

#[tokio::test]
async fn body_read_timeout_returns_408() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 10\r\n\r\nhello",
        )
        .await
        .unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .expect("body read timeout should close the connection")
        .unwrap();

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 408);
    assert_eq!(body, "request timeout");
}

#[tokio::test]
async fn payload_too_large_returns_413() {
    let app = || App::new().post("/echo", echo_body);
    let mut config = test_config();
    config.max_body_size = 4;
    let (addr, _server) = start_meguri_server(app, config);

    let req = "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 5\r\n\r\nhello";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 413);
    assert_eq!(body, "payload too large");
}

#[tokio::test]
async fn graceful_shutdown_drains_inflight_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn slow_handler(_req: Request) -> Response {
        std::thread::sleep(Duration::from_millis(500));
        Response::text("done")
    }

    let app = || App::new().get("/slow", slow_handler);
    let (addr, server) = start_meguri_server(app, test_config());

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(server);

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .expect("should receive response before drain timeout")
        .unwrap();

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 200);
    assert_eq!(body, "done");
}
