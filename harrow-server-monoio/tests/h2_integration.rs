//! HTTP/2 integration tests.
//!
//! These tests verify HTTP/2 support using monoio-http's H2 client.

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

async fn echo_body(req: Request) -> Response {
    let body = req.body_bytes().await.unwrap();
    Response::new(StatusCode::OK, body)
}

// -- Helpers -----------------------------------------------------------------

/// Start an HTTP/2 monoio server on a separate OS thread, return the bound address.
/// The server runs until `shutdown_tx` is dropped.
fn start_h2_server(app: App) -> (SocketAddr, std::sync::mpsc::Sender<()>) {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    std::thread::spawn(move || {
        let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
            .enable_timer()
            .build()
            .unwrap();
        rt.block_on(async {
            // Bind to a random port.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);

            addr_tx.send(addr).unwrap();

            let config = harrow_server_monoio::ServerConfig {
                header_read_timeout: Some(Duration::from_secs(2)),
                connection_timeout: Some(Duration::from_secs(30)),
                drain_timeout: Duration::from_secs(5),
                enable_http2: true, // Enable H2!
                ..Default::default()
            };

            harrow_server_monoio::serve_with_config(
                app,
                addr,
                async move {
                    // Block until shutdown signal.
                    loop {
                        monoio::time::sleep(Duration::from_millis(50)).await;
                        if shutdown_rx.try_recv().is_ok() {
                            break;
                        }
                    }
                },
                config,
            )
            .await
            .unwrap();
        });
    });

    let addr = addr_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Give the server a moment to start accepting.
    std::thread::sleep(Duration::from_millis(100));
    (addr, shutdown_tx)
}

// -- Tests -------------------------------------------------------------------

#[monoio::test]
async fn h2_basic_get() {
    let app = App::new().get("/hello", hello);
    let (addr, _shutdown) = start_h2_server(app);

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
    let (addr, _shutdown) = start_h2_server(app);

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
    let (addr, _shutdown) = start_h2_server(app);

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
