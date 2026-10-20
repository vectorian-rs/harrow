//! Replay the Monoio backend's real connection lifecycles against the shared
//! model (`harrow_server::h1_lifecycle::Machine`), so the backend and the
//! model cannot drift apart unnoticed (#80). Same approach as the Tokio
//! backend's `tests/lifecycle_traces.rs`.
//!
//! Monoio differs from Tokio on the wire in two places the traces reflect:
//! a header timeout is answered with 408, and a head cut off by the peer
//! with 400 (Tokio closes silently in both cases). It does not watch the
//! shutdown signal per connection, so there is no shutdown scenario here.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_server::h1_lifecycle::Event::{self, *};
use harrow_server::h1_lifecycle::trace::{replay, take_recorded};
use harrow_server_monoio::ServerConfig;

/// Traces go to one global list, so tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn app() -> App {
    App::new()
        .max_body_size(16)
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
}

fn start(header_read_timeout: Duration) -> (SocketAddr, harrow_server_monoio::ServerHandle) {
    let server = harrow_server_monoio::start_with_config(
        app,
        "127.0.0.1:0".parse().unwrap(),
        ServerConfig {
            workers: Some(1),
            header_read_timeout: Some(header_read_timeout),
            body_read_timeout: Some(Duration::from_secs(2)),
            connection_timeout: Some(Duration::from_secs(30)),
            drain_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    // Discard traces left by other tests' servers.
    let _ = take_recorded();
    (addr, server)
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
    let (addr, _server) = start(Duration::from_secs(2));

    let response = exchange(
        addr,
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
    let (addr, _server) = start(Duration::from_secs(2));

    let mut stream = TcpStream::connect(addr).unwrap();
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
    let (addr, _server) = start(Duration::from_secs(2));

    let response = exchange(
        addr,
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
    let (addr, _server) = start(Duration::from_secs(2));

    let response = exchange(
        addr,
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
    let (addr, _server) = start(Duration::from_secs(2));

    let response = exchange(addr, b"NOT A REQUEST\r\n\r\n");
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
    let (addr, _server) = start(Duration::from_secs(2));

    // Headers announce a body that never comes; the handler answers anyway.
    let mut stream = TcpStream::connect(addr).unwrap();
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
    let (addr, _server) = start(Duration::from_secs(2));

    // 20 bytes against a 16-byte limit, only detectable while reading.
    let response = exchange(
        addr,
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
fn header_read_timeout_is_answered_with_408() {
    let _serial = serial();
    let (addr, _server) = start(Duration::from_millis(200));

    // Half a request head, then silence.
    let response = exchange(addr, b"GET /hello HTTP/1.1\r\n");
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_reads(&traces[0]),
        [Accept, ProtocolError, DONE_CLOSE]
    );
}

#[test]
fn peer_closes_mid_headers() {
    let _serial = serial();
    let (addr, _server) = start(Duration::from_secs(2));

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(b"GET /hello HTT").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    let trace = without_partial_reads(&traces[0]);
    // Monoio tries to answer 400; the write may or may not reach the peer.
    assert_eq!(trace[..2], [Accept, ProtocolError]);
    assert!(matches!(trace[2], WriteDone { .. } | IoError), "{trace:?}");
}

#[test]
fn pipelined_requests_report_the_buffered_next_request() {
    let _serial = serial();
    let (addr, _server) = start(Duration::from_secs(2));

    let response = exchange(
        addr,
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
