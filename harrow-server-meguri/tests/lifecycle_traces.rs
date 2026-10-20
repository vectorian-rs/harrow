//! Replay the Meguri backend's real connection lifecycles against the shared
//! model (`harrow_server::h1_lifecycle::Machine`), so the backend and the
//! model cannot drift apart unnoticed (#80). Same approach as the Tokio and
//! Monoio backends' `tests/lifecycle_traces.rs`.
//!
//! Meguri is a completion-driven io_uring loop, so its traces contain real
//! closed CQEs: a connection closed by a timeout keeps its slab slot until
//! the RECV/SEND it abandoned completes, and only then records `ClosedCqe`.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_server::h1_lifecycle::Event::{self, *};
use harrow_server::h1_lifecycle::trace::{replay, take_recorded};
use harrow_server_meguri::ServerConfig;

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
        // Sends a first chunk, then stalls for a second before the rest.
        .get("/stall", |_req: Request| async {
            let body = futures_util::stream::unfold(0u8, |step| async move {
                match step {
                    0 => Some((Ok(http_body::Frame::data(bytes::Bytes::from("first"))), 1)),
                    1 => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Some((Ok(http_body::Frame::data(bytes::Bytes::from("second"))), 2))
                    }
                    _ => None,
                }
            });
            Response::streaming(http::StatusCode::OK, body)
        })
}

fn config() -> ServerConfig {
    ServerConfig {
        workers: Some(1),
        header_read_timeout: Some(Duration::from_secs(2)),
        body_read_timeout: Some(Duration::from_secs(2)),
        connection_timeout: Some(Duration::from_secs(30)),
        drain_timeout: Duration::from_secs(5),
        max_body_size: 16,
        ..Default::default()
    }
}

fn start(config: ServerConfig) -> (SocketAddr, harrow_server_meguri::ServerHandle) {
    let server =
        harrow_server_meguri::start_with_config(app, "127.0.0.1:0".parse().unwrap(), config)
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

/// Read one keep-alive response, which may arrive in several segments (the
/// head and the body are separate SENDs), up to its known last bytes.
fn read_until_suffix(stream: &mut TcpStream, suffix: &str) {
    let mut response = Vec::new();
    while !response.ends_with(suffix.as_bytes()) {
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).unwrap();
        assert!(
            n > 0,
            "closed early: {}",
            String::from_utf8_lossy(&response)
        );
        response.extend_from_slice(&buf[..n]);
    }
}

/// Drop the self-loops whose count depends on TCP segmentation and on how
/// the response is chunked, not on the lifecycle: `HeadersNeedMore`,
/// `BodyNeedMore` and `WriteProgress`.
fn without_partial_io(trace: &[Event]) -> Vec<Event> {
    trace
        .iter()
        .copied()
        .filter(|e| !matches!(e, HeadersNeedMore | BodyNeedMore | WriteProgress))
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
    let (addr, _server) = start(config());

    let response = exchange(
        addr,
        b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.ends_with("hello"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
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
    let (addr, _server) = start(config());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for _ in 0..2 {
        stream
            .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        read_until_suffix(&mut stream, "hello");
    }
    drop(stream);

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
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
    let (addr, _server) = start(config());

    let response = exchange(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
    );
    assert!(response.ends_with("abcd"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: true },
            BodyDone,
            DispatchDone,
            DONE_CLOSE
        ]
    );
}

#[test]
fn request_body_in_several_reads() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 8\r\nConnection: close\r\n\r\nab",
        )
        .unwrap();
    for part in [&b"cd"[..], b"ef", b"gh"] {
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(part).unwrap();
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    assert!(response.ends_with("abcdefgh"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert!(traces[0].contains(&BodyNeedMore), "{:?}", traces[0]);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: true },
            BodyDone,
            DispatchDone,
            DONE_CLOSE
        ]
    );
}

#[test]
fn content_length_over_the_limit_is_rejected_before_the_body() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let response = exchange(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [Accept, BodyLimitExceeded, DONE_CLOSE]
    );
}

#[test]
fn malformed_request_head_gets_an_error_response() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let response = exchange(addr, b"NOT A REQUEST\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [Accept, ProtocolError, DONE_CLOSE]
    );
}

#[test]
fn handler_returns_before_body_eof() {
    let _serial = serial();
    let (addr, _server) = start(config());

    // Headers announce a body that never comes; the handler answers anyway.
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"POST /ignore-body HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    assert!(response.ends_with("ignored"), "{response}");
    drop(stream);

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    let trace = without_partial_io(&traces[0]);
    // The handler drops the request body and answers. The server sees
    // either the response first (`EarlyResponse`) or the dropped body first
    // (`BodyDone`, then `DispatchDone`), depending on which it polls first.
    assert!(
        trace
            == [
                Accept,
                HeadersParsed { has_body: true },
                EarlyResponse,
                DONE_CLOSE
            ]
            || trace
                == [
                    Accept,
                    HeadersParsed { has_body: true },
                    BodyDone,
                    DispatchDone,
                    DONE_CLOSE
                ],
        "{trace:?}"
    );
}

#[test]
fn chunked_body_over_the_limit_is_rejected_mid_body() {
    let _serial = serial();
    let (addr, _server) = start(config());

    // 20 bytes against a 16-byte limit, only detectable while reading.
    let response = exchange(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n14\r\naaaaaaaaaaaaaaaaaaaa\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: true },
            BodyLimitExceeded,
            DONE_CLOSE
        ]
    );
}

#[test]
fn body_read_timeout_is_answered_with_408() {
    let _serial = serial();
    let (addr, _server) = start(ServerConfig {
        body_read_timeout: Some(Duration::from_millis(200)),
        ..config()
    });

    // Half a body, then silence: the pending RECV is cancelled and its CQE
    // turns into a 408.
    let response = exchange(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc",
    );
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: true },
            ProtocolError,
            DONE_CLOSE
        ]
    );
}

#[test]
fn header_read_timeout_waits_for_the_closed_cqe() {
    let _serial = serial();
    let (addr, _server) = start(ServerConfig {
        header_read_timeout: Some(Duration::from_millis(200)),
        ..config()
    });

    // Half a request head, then silence. Meguri closes without a response;
    // the RECV it abandoned completes after the socket is shut down.
    let response = exchange(addr, b"GET /hello HTTP/1.1\r\n");
    assert!(response.is_empty(), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(without_partial_io(&traces[0]), [Accept, Timeout, ClosedCqe]);
}

#[test]
fn peer_closes_mid_headers() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(b"GET /hello HTT").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(without_partial_io(&traces[0]), [Accept, IoError]);
}

#[test]
fn pipelined_requests_report_the_buffered_next_request() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let response = exchange(
        addr,
        b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\nGET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response.matches("hello").count(), 2, "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: false },
            DispatchDone,
            WriteDone {
                keep_alive: true,
                buffered_next_request: true
            },
            HeadersParsed { has_body: false },
            DispatchDone,
            DONE_CLOSE
        ]
    );
}

#[test]
fn connection_lifetime_reached_mid_response() {
    let _serial = serial();
    let (addr, _server) = start(ServerConfig {
        connection_timeout: Some(Duration::from_millis(300)),
        ..config()
    });

    // The response stalls after its first chunk, past the connection
    // lifetime: meguri abandons it and closes the connection.
    let response = exchange(
        addr,
        b"GET /stall HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.contains("first"), "{response}");
    assert!(!response.contains("second"), "{response}");

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        without_partial_io(&traces[0]),
        [
            Accept,
            HeadersParsed { has_body: false },
            DispatchDone,
            Timeout,
            ClosedCqe
        ]
    );
}

/// The peer half-closes before the body it announced is complete. Meguri
/// used to keep queueing RECVs that returned 0 at once (hundreds of
/// thousands of `BodyNeedMore`) until the body read timeout answered 408.
#[test]
fn peer_closes_mid_body() {
    let _serial = serial();
    let (addr, _server) = start(config());

    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc")
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    assert!(
        response.is_empty(),
        "{}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "closed after {:?}, not at the peer's EOF",
        started.elapsed()
    );

    let traces = wait_for_traces(1);
    assert_replays(&traces);
    assert_eq!(
        traces[0],
        [Accept, HeadersParsed { has_body: true }, IoError]
    );
}

#[test]
fn shutdown_during_an_active_request_and_with_an_idle_connection() {
    let _serial = serial();
    // 2 s header timeout: the idle connection must close because of the
    // shutdown, not because that timeout fires.
    let (addr, server) = start(config());

    // Idle keep-alive connection: one request done, waiting for the next.
    let mut idle = TcpStream::connect(addr).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    idle.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    read_until_suffix(&mut idle, "hello");

    // Active request: the handler is still running when shutdown starts.
    let active =
        std::thread::spawn(move || exchange(addr, b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n"));
    std::thread::sleep(Duration::from_millis(100));
    let shutdown_started = Instant::now();
    server.shutdown().unwrap();
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(1),
        "shutdown waited {:?}; idle connections should not hold it up",
        shutdown_started.elapsed()
    );

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
    let ends: Vec<Vec<Event>> = traces.iter().map(|t| without_partial_io(t)).collect();
    assert!(
        ends.iter()
            .any(|t| t.ends_with(&[Shutdown, DispatchDone, DONE_CLOSE])),
        "in-flight request should answer without keep-alive: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|t| t.ends_with(&[DONE_KEEP, Shutdown, Timeout, ClosedCqe])),
        "idle connection should close because of the shutdown: {ends:?}"
    );
}
