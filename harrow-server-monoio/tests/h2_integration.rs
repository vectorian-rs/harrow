//! HTTP/2 integration tests.
//!
//! These tests verify HTTP/2 support using monoio-http's H2 client.

use std::io::Read;
use std::net::SocketAddr;
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use http::StatusCode;

// -- Handlers ----------------------------------------------------------------

async fn hello(_req: Request) -> Response {
    Response::text("hello h2")
}

async fn hello_with_headers(_req: Request) -> Response {
    Response::text("hello h2").header("cache-control", "no-store")
}

async fn echo_body(req: Request) -> Response {
    let body = req.body_bytes().await.unwrap();
    Response::new(StatusCode::OK, body)
}

// -- Helpers -----------------------------------------------------------------

/// Start an HTTP/2 monoio server with Harrow's public bootstrap.
fn start_h2_server(app: App) -> (SocketAddr, harrow_server_monoio::ServerHandle) {
    start_h2_server_with_config(
        app,
        harrow_server_monoio::ServerConfig {
            workers: Some(1),
            header_read_timeout: Some(Duration::from_secs(2)),
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_secs(30)),
            drain_timeout: Duration::from_secs(5),
            enable_http2: true,
            max_h2_streams: 32,
            ..Default::default()
        },
    )
}

fn start_h2_server_with_config(
    app: App,
    config: harrow_server_monoio::ServerConfig,
) -> (SocketAddr, harrow_server_monoio::ServerHandle) {
    let server =
        harrow_server_monoio::start_with_config(app, "127.0.0.1:0".parse().unwrap(), config)
            .unwrap();

    let addr = server.local_addr();
    // Give the server a moment to start accepting.
    std::thread::sleep(Duration::from_millis(100));
    (addr, server)
}

// -- Tests -------------------------------------------------------------------

#[monoio::test]
async fn h2_basic_get() {
    let app = App::new().get("/hello", hello);
    let (addr, _server) = start_h2_server(app);

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        if let Err(e) = h2.await {
            tracing::debug!("h2 connection error: {}", e);
        }
    });

    let request = http::Request::builder()
        .uri("http://localhost/hello")
        .method("GET")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();

    // End stream (no body)
    stream.send_data(bytes::Bytes::new(), true).unwrap();

    let response = response.await.unwrap();
    let status = response.status();

    // Read response body using RecvStream
    let mut body = response.into_body();
    let mut body_data = Vec::new();

    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body_data.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body_data), "hello h2");
}

#[monoio::test]
async fn h2_post_with_body() {
    let app = App::new().post("/echo", echo_body);
    let (addr, _server) = start_h2_server(app);

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    let body_data = bytes::Bytes::from("test h2 body");

    let request = http::Request::builder()
        .uri("http://localhost/echo")
        .method("POST")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();

    // Send body
    stream.send_data(body_data.clone(), true).unwrap();

    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Read response body
    let mut response_body = Vec::new();
    let mut body = response.into_body();

    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        response_body.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    assert_eq!(response_body, body_data);
}

#[monoio::test]
async fn h2_not_found() {
    let app = App::new().get("/hello", hello);
    let (addr, _server) = start_h2_server(app);

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    let request = http::Request::builder()
        .uri("http://localhost/nonexistent")
        .method("GET")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();
    stream.send_data(bytes::Bytes::new(), true).unwrap();

    let response = response.await.unwrap();
    let status = response.status();

    // Read body
    let mut body = response.into_body();
    let mut body_data = Vec::new();

    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body_data.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(String::from_utf8_lossy(&body_data), "not found");
}

#[monoio::test]
async fn h2_preserves_response_headers() {
    let app = App::new().get("/hello", hello_with_headers);
    let (addr, _server) = start_h2_server(app);

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    let request = http::Request::builder()
        .uri("http://localhost/hello")
        .method("GET")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();
    stream.send_data(bytes::Bytes::new(), true).unwrap();

    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(http::header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
}

#[monoio::test]
async fn h2_body_limit_returns_413() {
    let app = App::new().max_body_size(5).post("/echo", echo_body);
    let (addr, _server) = start_h2_server(app);

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    let request = http::Request::builder()
        .uri("http://localhost/echo")
        .method("POST")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();
    stream
        .send_data(bytes::Bytes::from_static(b"abcdef"), true)
        .unwrap();

    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let mut response_body = Vec::new();
    let mut body = response.into_body();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        response_body.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    assert_eq!(String::from_utf8_lossy(&response_body), "payload too large");
}

#[test]
fn h2_handshake_timeout_closes_idle_socket() {
    let app = App::new().get("/hello", hello);
    let (addr, _server) = start_h2_server_with_config(
        app,
        harrow_server_monoio::ServerConfig {
            workers: Some(1),
            header_read_timeout: Some(Duration::from_millis(200)),
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_secs(5)),
            drain_timeout: Duration::from_secs(5),
            enable_http2: true,
            max_h2_streams: 32,
            ..Default::default()
        },
    );

    let mut socket = std::net::TcpStream::connect(addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();

    std::thread::sleep(Duration::from_millis(700));

    let mut saw_close = false;
    let mut buf = [0_u8; 64];
    for _ in 0..4 {
        match socket.read(&mut buf) {
            Ok(0) => {
                saw_close = true;
                break;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                saw_close = true;
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("expected closed socket after handshake timeout: {err}"),
        }
    }

    assert!(saw_close, "expected closed socket after handshake timeout");
}

#[monoio::test]
async fn h2_idle_keepalive_timeout_closes_connection() {
    let app = App::new().get("/hello", hello);
    let (addr, _server) = start_h2_server_with_config(
        app,
        harrow_server_monoio::ServerConfig {
            workers: Some(1),
            header_read_timeout: Some(Duration::from_millis(200)),
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_secs(5)),
            drain_timeout: Duration::from_secs(5),
            enable_http2: true,
            max_h2_streams: 32,
            ..Default::default()
        },
    );

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    let request = http::Request::builder()
        .uri("http://localhost/hello")
        .method("GET")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();
    stream.send_data(bytes::Bytes::new(), true).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    std::thread::sleep(Duration::from_millis(700));

    let request = http::Request::builder()
        .uri("http://localhost/hello")
        .method("GET")
        .body(())
        .unwrap();

    match client.send_request(request, true) {
        Err(_) => {}
        Ok((response, _)) => {
            assert!(
                response.await.is_err(),
                "expected idle H2 connection to close"
            );
        }
    }
}

#[monoio::test]
async fn h2_connection_timeout_closes_connection() {
    let app = App::new().get("/hello", hello);
    let (addr, _server) = start_h2_server_with_config(
        app,
        harrow_server_monoio::ServerConfig {
            workers: Some(1),
            header_read_timeout: None,
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_millis(300)),
            drain_timeout: Duration::from_secs(5),
            enable_http2: true,
            max_h2_streams: 32,
            ..Default::default()
        },
    );

    use monoio::net::TcpStream;
    use monoio_http::h2::client;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    monoio::spawn(async move {
        let _ = h2.await;
    });

    std::thread::sleep(Duration::from_millis(700));

    let request = http::Request::builder()
        .uri("http://localhost/hello")
        .method("GET")
        .body(())
        .unwrap();

    match client.send_request(request, true) {
        Err(_) => {}
        Ok((response, _)) => {
            assert!(
                response.await.is_err(),
                "expected H2 connection lifetime timeout to close the connection"
            );
        }
    }
}
