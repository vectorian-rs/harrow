//! Replay the Tokio backend's real connection lifecycles against the shared
//! model (`harrow_server::h1_lifecycle::Machine`), so the backend and the
//! model cannot drift apart unnoticed (#80).
//!
//! Each test drives a real server over TCP, collects the per-connection event
//! traces the backend recorded (the `lifecycle-trace` feature is enabled for
//! tests via dev-dependencies), and checks that every trace is accepted by
//! the model and ends with the connection `Free`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_server::h1_lifecycle::Event::{self, *};
use harrow_server::h1_lifecycle::trace::{replay, take_recorded};
use harrow_server_tokio::ServerConfig;

/// Traces go to one global list, so tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn app() -> App {
    App::new()
        .get("/hello", |_req: Request| async { Response::text("hello") })
        .post("/echo", |req: Request| async move {
            match req.body_bytes().await {
                Ok(body) => Response::new(http::StatusCode::OK, body),
                Err(_) => Response::new(http::StatusCode::BAD_REQUEST, "bad body"),
            }
        })
        // Answers without reading the request body.
        .post("/ignore-body", |_req: Request| async {
            Response::text("ignored")
        })
        .get("/slow", |_req: Request| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Response::text("slow")
        })
}

struct Server {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(config: ServerConfig) -> Self {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let shutdown = async {
                        let _ = rx.await;
                    };
                    harrow_server_tokio::serve_with_config(app(), addr, shutdown, config)
                        .await
                        .unwrap();
                });
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(addr).is_err() {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The readiness probe above is itself a connection; discard its trace.
        wait_for_traces(1);
        Self {
            addr,
            shutdown: Some(tx),
            thread: Some(thread),
        }
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn config() -> ServerConfig {
    ServerConfig {
        max_body_size: 16,
        ..ServerConfig::default()
    }
}

/// Wait until `count` connections have finished and return their traces.
fn wait_for_traces(count: usize) -> Vec<Vec<Event>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut traces = Vec::new();
    while traces.len() < count {
        traces.extend(take_recorded());
        assert!(
            Instant::now() < deadline,
            "expected {count} traces, got {traces:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    traces
}

/// Every trace must be accepted by the model and end `Free`.
fn assert_replays(traces: &[Vec<Event>]) {
    for trace in traces {
        if let Err(err) = replay(trace) {
            panic!("trace rejected by the lifecycle model: {err}\ntrace: {trace:?}");
        }
    }
}

/// Write `request`, read until the server closes, return the response text.
fn exchange(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

/// Drop `HeadersNeedMore` / `BodyNeedMore`: how many reads a request takes
/// depends on TCP segmentation, not on the lifecycle.
fn without_partial_reads(trace: &[Event]) -> Vec<Event> {
    trace
        .iter()
        .copied()
        .filter(|e| !matches!(e, HeadersNeedMore | BodyNeedMore))
        .collect()
}

const DONE_CLOSE: Event = WriteDone {
    keep_alive: false,
    buffered_next_request: false,
};
const DONE_KEEP: Event = WriteDone {
    keep_alive: true,
    buffered_next_request: false,
};

#[test]
fn request_without_body() {
    let _serial = serial();
    let server = Server::start(config());

    let response = exchange(
        server.addr,
        b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.ends_with("hello"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: false },
            DispatchDone,
            DONE_CLOSE
        ]
    );
}

#[test]
fn keep_alive_reuse_then_peer_close() {
    let _serial = serial();
    let server = Server::start(config());

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for _ in 0..2 {
        stream
            .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).ends_with("hello"));
    }
    drop(stream);

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: false },
            DispatchDone,
            DONE_KEEP,
            HeadersParsed { has_body: false },
            DispatchDone,
            DONE_KEEP,
            IoError,
        ]
    );
}

#[test]
fn request_with_body() {
    let _serial = serial();
    let server = Server::start(config());

    let response = exchange(
        server.addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
    );
    assert!(response.ends_with("abcd"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    let trace = without_partial_reads(&traces[0]);
    assert_eq!(trace[..2], [Accept, HeadersParsed { has_body: true }]);
    assert_eq!(trace.last(), Some(&DONE_CLOSE));
}

#[test]
fn content_length_over_the_limit_is_rejected_before_the_body() {
    let _serial = serial();
    let server = Server::start(config());

    let response = exchange(
        server.addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [Accept, BodyLimitExceeded, DONE_CLOSE]
    );
}

#[test]
fn malformed_request_head_gets_an_error_response() {
    let _serial = serial();
    let server = Server::start(config());

    let response = exchange(server.addr, b"NOT A REQUEST\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [Accept, ProtocolError, DONE_CLOSE]
    );
}

#[test]
fn handler_returns_before_body_eof() {
    let _serial = serial();
    let server = Server::start(config());

    // Send headers announcing a body, but no body: the handler answers anyway.
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"POST /ignore-body HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).ends_with("ignored"));
    drop(stream);

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    let trace = without_partial_reads(&traces[0]);
    assert_eq!(
        trace[..3],
        [Accept, HeadersParsed { has_body: true }, EarlyResponse]
    );
    assert_eq!(trace.last(), Some(&DONE_CLOSE));
}

#[test]
fn chunked_body_over_the_limit_is_rejected_mid_body() {
    let _serial = serial();
    let server = Server::start(config());

    // 20 bytes against a 16-byte limit, only detectable while reading.
    let response = exchange(
        server.addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n14\r\naaaaaaaaaaaaaaaaaaaa\r\n0\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 413") || response.starts_with("HTTP/1.1 400"),
        "{response}"
    );

    let traces = wait_for_traces(1);
    assert_replays(&traces);
}

#[test]
fn header_read_timeout() {
    let _serial = serial();
    let server = Server::start(ServerConfig {
        header_read_timeout: Some(Duration::from_millis(100)),
        ..config()
    });

    // Half a request head, then silence.
    let response = exchange(server.addr, b"GET /hello HTTP/1.1\r\n");
    assert!(response.is_empty(), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [Accept, Timeout, ClosedCqe]
    );
}

#[test]
fn peer_closes_mid_headers() {
    let _serial = serial();
    let server = Server::start(config());

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream.write_all(b"GET /hello HTT").unwrap();
    drop(stream);

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(without_partial_reads(&traces[0]), [Accept, IoError]);
}

#[test]
fn pipelined_requests_report_the_buffered_next_request() {
    let _serial = serial();
    let server = Server::start(config());

    let response = exchange(
        server.addr,
        b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\nGET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response.matches("hello").count(), 2, "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert!(
        traces[0].contains(&WriteDone {
            keep_alive: true,
            buffered_next_request: true
        }),
        "{:?}",
        traces[0]
    );
}

#[test]
fn shutdown_during_an_active_request_and_with_an_idle_connection() {
    let _serial = serial();
    // A short header timeout: an idle keep-alive connection does not see the
    // shutdown signal while it waits for the next request, so it closes when
    // this timeout fires (#98; see the assertion below).
    let mut server = Server::start(ServerConfig {
        header_read_timeout: Some(Duration::from_millis(300)),
        ..config()
    });

    // Idle keep-alive connection: one request done, waiting for the next.
    let mut idle = TcpStream::connect(server.addr).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    idle.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 256];
    let _ = idle.read(&mut buf).unwrap();

    // Active request: the handler is still running when shutdown starts.
    let addr = server.addr;
    let active =
        std::thread::spawn(move || exchange(addr, b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n"));
    std::thread::sleep(Duration::from_millis(100));
    server.shutdown();

    let response = active.join().unwrap();
    assert!(
        response.ends_with("slow"),
        "in-flight request must finish: {response}"
    );
    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "response during shutdown must not advertise keep-alive: {response}"
    );

    let traces = wait_for_traces(2);
    assert_replays(&traces);
    let ends: Vec<Vec<Event>> = traces.iter().map(|t| without_partial_reads(t)).collect();
    assert!(
        ends.iter()
            .any(|t| t.ends_with(&[DispatchDone, DONE_CLOSE])),
        "in-flight request should answer without keep-alive: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|t| t.ends_with(&[DONE_KEEP, Timeout, ClosedCqe])),
        "idle connection closes when its header timeout fires: {ends:?}"
    );
}
