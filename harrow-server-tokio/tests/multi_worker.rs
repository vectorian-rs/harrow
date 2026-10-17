//! Lifecycle of the multi-worker server: `start_with_config` / `ServerHandle`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_server_tokio::{ServerConfig, start_with_config};

fn app() -> App {
    App::new()
        .get("/hello", |_req: Request| async { Response::text("hello") })
        .get("/slow", |_req: Request| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Response::text("done")
        })
}

fn config() -> ServerConfig {
    ServerConfig {
        workers: Some(2),
        ..ServerConfig::default()
    }
}

/// GET over a fresh connection; returns the raw response.
fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn start_on_port_zero_reports_bound_address_and_serves() {
    let server = start_with_config(app, "127.0.0.1:0".parse().unwrap(), config()).expect("start");
    let addr = server.local_addr();
    assert_ne!(addr.port(), 0);

    for _ in 0..4 {
        let response = get(addr, "/hello");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.ends_with("hello"), "{response}");
    }

    server.shutdown().expect("clean shutdown");
}

#[test]
fn shutdown_drains_in_flight_requests_then_stops_accepting() {
    let server = start_with_config(app, "127.0.0.1:0".parse().unwrap(), config()).expect("start");
    let addr = server.local_addr();

    let in_flight = std::thread::spawn(move || get(addr, "/slow"));
    std::thread::sleep(Duration::from_millis(100));

    server.shutdown().expect("clean shutdown");

    let response = in_flight.join().unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("done"), "{response}");
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err(),
        "listener still accepting after shutdown"
    );
}

#[test]
fn start_reports_bind_errors_instead_of_panicking() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();

    // `taken` has no SO_REUSEPORT, so the worker's bind fails.
    let Err(err) = start_with_config(app, addr, config()) else {
        panic!("start should fail while the port is taken");
    };
    assert!(!err.to_string().is_empty());
}
