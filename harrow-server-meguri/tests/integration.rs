#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use http::StatusCode;

fn integration_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

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
    (addr, server)
}

fn test_config() -> harrow_server_meguri::ServerConfig {
    harrow_server_meguri::ServerConfig {
        workers: Some(1),
        header_read_timeout: Some(Duration::from_millis(250)),
        body_read_timeout: Some(Duration::from_millis(250)),
        connection_timeout: Some(Duration::from_secs(30)),
        drain_timeout: Duration::from_secs(5),
        max_body_size: 1024,
        ..Default::default()
    }
}

async fn read_to_end_allow_reset(stream: &mut tokio::net::TcpStream, buf: &mut Vec<u8>) {
    use tokio::io::AsyncReadExt;

    match stream.read_to_end(buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(err) => panic!("failed to read response: {err}"),
    }
}

async fn connect_with_retry(addr: SocketAddr) -> tokio::net::TcpStream {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(err)
                if err.kind() == std::io::ErrorKind::ConnectionRefused
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(err) => panic!("meguri server did not become ready at {addr}: {err}"),
        }
    }
}

async fn http_request(addr: SocketAddr, request: &str) -> (u16, Vec<(String, String)>, String) {
    use tokio::io::AsyncWriteExt;

    let mut stream = connect_with_retry(addr).await;
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    read_to_end_allow_reset(&mut stream, &mut buf).await;

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
    let _guard = integration_lock();
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
    let _guard = integration_lock();
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
    let _guard = integration_lock();
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
    let (addr, server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
    stream
        .write_all(
            b"POST /first-chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n",
        )
        .await
        .unwrap();

    let buf = tokio::time::timeout(Duration::from_millis(250), async {
        let mut acc = Vec::new();
        loop {
            let mut chunk = [0u8; 256];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before response arrived");
            acc.extend_from_slice(&chunk[..n]);
            if acc
                .windows(b"HTTP/1.1 200".len())
                .any(|w| w == b"HTTP/1.1 200")
                && acc.windows(b"hello".len()).any(|w| w == b"hello")
            {
                break acc;
            }
        }
    })
    .await
    .expect("expected response before request EOF");

    drop(stream);
    drop(server);

    let raw = String::from_utf8_lossy(&buf);
    assert!(raw.contains("HTTP/1.1 200"), "unexpected response: {raw}");
    assert!(
        raw.contains("hello"),
        "expected first chunk in response: {raw}"
    );
}

#[tokio::test]
async fn streaming_response_flushes_before_eof() {
    let _guard = integration_lock();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
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
async fn keep_alive_pipelining_reuses_connection() {
    let _guard = integration_lock();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || {
        App::new()
            .get("/one", |_req: Request| async move { Response::text("one") })
            .get("/two", |_req: Request| async move { Response::text("two") })
    };
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
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
    let _guard = integration_lock();

    let app = || App::new().get("/hello", hello);
    let (addr, server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;

    let mut buf = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        read_to_end_allow_reset(&mut stream, &mut buf),
    )
    .await
    .expect("connection should close within timeout");
    drop(stream);
    drop(server);
}

#[tokio::test]
async fn body_read_timeout_returns_408() {
    let _guard = integration_lock();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
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
    let _guard = integration_lock();
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
async fn payload_too_large_content_length_rejects_before_body_eof() {
    let _guard = integration_lock();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = || App::new().post("/echo", echo_body);
    let mut config = test_config();
    config.max_body_size = 4;
    let (addr, _server) = start_meguri_server(app, config);

    let mut stream = connect_with_retry(addr).await;
    stream
        .write_all(
            b"POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 10\r\n\r\nhe",
        )
        .await
        .unwrap();

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_millis(250), stream.read_to_end(&mut buf))
        .await
        .expect("expected 413 before request EOF")
        .unwrap();

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 413);
    assert_eq!(body, "payload too large");
}

#[tokio::test]
async fn rejects_content_length_and_chunked_together() {
    let _guard = integration_lock();
    let app = || App::new().post("/echo", echo_body);
    let (addr, _server) = start_meguri_server(app, test_config());

    let req = "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let (status, _headers, body) = http_request(addr, req).await;

    assert_eq!(status, 400);
    assert_eq!(body, "bad request");
}

#[tokio::test]
async fn malformed_request_returns_400() {
    let _guard = integration_lock();
    use tokio::io::AsyncWriteExt;

    let app = || App::new().get("/hello", hello);
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
    stream
        .write_all(b"NOT A VALID HTTP REQUEST\r\n\r\n")
        .await
        .unwrap();

    let mut buf = Vec::new();
    read_to_end_allow_reset(&mut stream, &mut buf).await;
    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 400);
    assert_eq!(body, "bad request");
}

#[tokio::test]
async fn fixed_length_mismatch_closes_connection_without_serving_next_request() {
    let _guard = integration_lock();
    use tokio::io::AsyncWriteExt;

    let app = || {
        App::new()
            .get("/bad", |_req: Request| async move {
                Response::text("oops").header("content-length", "10")
            })
            .get("/hello", hello)
    };
    let (addr, _server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
    stream
        .write_all(
            b"GET /bad HTTP/1.1\r\nHost: localhost\r\n\r\nGET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();

    let mut buf = Vec::new();
    read_to_end_allow_reset(&mut stream, &mut buf).await;
    let raw = String::from_utf8_lossy(&buf);

    assert_eq!(raw.match_indices("HTTP/1.1 200").count(), 1, "{raw}");
    assert!(raw.contains("content-length: 10\r\n"), "{raw}");
    assert!(raw.contains("\r\n\r\noops"), "{raw}");
    assert!(
        !raw.contains("hello"),
        "connection should close before serving the next request: {raw}"
    );
}

#[tokio::test]
async fn graceful_shutdown_drains_inflight_request() {
    let _guard = integration_lock();
    use tokio::io::AsyncWriteExt;

    async fn slow_handler(_req: Request) -> Response {
        std::thread::sleep(Duration::from_millis(500));
        Response::text("done")
    }

    let app = || App::new().get("/slow", slow_handler);
    let (addr, server) = start_meguri_server(app, test_config());

    let mut stream = connect_with_retry(addr).await;
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(server);

    let mut buf = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        read_to_end_allow_reset(&mut stream, &mut buf),
    )
    .await
    .expect("should receive response before drain timeout");

    let (status, _headers, body) = parse_http_response(&buf);
    assert_eq!(status, 200);
    assert_eq!(body, "done");
}
