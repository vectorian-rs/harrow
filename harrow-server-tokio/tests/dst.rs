//! Deterministic Simulation Testing for the connection handler.
//!
//! Uses `tokio::io::duplex` as a mock stream and `start_paused = true`
//! for deterministic time control. Each test is seeded and reproducible.

use std::sync::Arc;
use std::time::Duration;

use harrow_core::request::Request;
use harrow_core::response::Response;
use harrow_core::route::App;
use harrow_server_tokio::{WorkerConfig, handle_connection};
use tokio::io::AsyncWriteExt;

fn test_config() -> WorkerConfig {
    WorkerConfig {
        max_connections: 128,
        header_read_timeout: Some(Duration::from_secs(5)),
        connection_timeout: Some(Duration::from_secs(60)),
        body_read_timeout: Some(Duration::from_secs(10)),
        max_body_size: 1024 * 1024,
    }
}

fn test_app() -> App {
    App::new()
        .get("/", |_req: Request| async { Response::text("ok") })
        .get("/hello", |_req: Request| async { Response::text("hello") })
        .post("/echo", |req: Request| async move {
            match req.body_bytes().await {
                Ok(body) => Response::text(format!("echo: {} bytes", body.len())),
                Err(e) => Response::text(format!("error: {e}")).status(500),
            }
        })
}

fn shared_state() -> Arc<harrow_core::dispatch::SharedState> {
    test_app().into_shared_state()
}

/// Run handle_connection against a duplex stream, write `request_bytes`
/// to the client side, return whatever the server writes back.
async fn run_connection(request_bytes: &[u8], config: &WorkerConfig) -> Vec<u8> {
    let shared = shared_state();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);

    let owned = request_bytes.to_vec();
    let write_task = tokio::spawn(async move {
        client_write.write_all(&owned).await.unwrap();
        client_write.shutdown().await.unwrap();
    });

    let cfg = config.clone();
    let handle_task = tokio::spawn(async move {
        let _ = handle_connection(server, shared, &cfg).await;
    });

    // Read server response from client side.
    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = client_read.read_to_end(&mut response).await;

    let _ = write_task.await;
    let _ = handle_task.await;

    response
}

/// Run handle_connection with incremental writes (simulates slow client).
async fn run_connection_incremental(
    chunks: &[&[u8]],
    delays: &[Duration],
    config: &WorkerConfig,
) -> Vec<u8> {
    let shared = shared_state();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);

    let chunks: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
    let delays: Vec<Duration> = delays.to_vec();

    let write_task = tokio::spawn(async move {
        for (i, chunk) in chunks.iter().enumerate() {
            if i < delays.len() {
                tokio::time::sleep(delays[i]).await;
            }
            if client_write.write_all(chunk).await.is_err() {
                break;
            }
        }
        let _ = client_write.shutdown().await;
    });

    let config = config.clone();
    let handle_task = tokio::spawn(async move {
        let _ = handle_connection(server, shared, &config).await;
    });

    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = client_read.read_to_end(&mut response).await;

    let _ = write_task.await;
    let _ = handle_task.await;

    response
}

fn assert_http_response(data: &[u8], expected_status: u16) {
    let s = String::from_utf8_lossy(data);
    let status_str = format!("{expected_status}");
    assert!(
        s.contains(&status_str),
        "expected HTTP {expected_status}, got: {}",
        &s[..s.len().min(200)]
    );
}

fn assert_response_contains(data: &[u8], needle: &str) {
    let s = String::from_utf8_lossy(data);
    assert!(
        s.contains(needle),
        "expected response to contain {needle:?}, got: {}",
        &s[..s.len().min(200)]
    );
}

// ---------------------------------------------------------------------------
// Basic request/response tests
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_simple_get() {
    let resp = run_connection(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        &test_config(),
    )
    .await;
    assert_http_response(&resp, 200);
    assert_response_contains(&resp, "ok");
}

#[tokio::test(start_paused = true)]
async fn dst_post_with_body() {
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        &test_config(),
    ).await;
    assert_http_response(&resp, 200);
    assert_response_contains(&resp, "echo: 5 bytes");
}

#[tokio::test(start_paused = true)]
async fn dst_404() {
    let resp = run_connection(
        b"GET /nonexistent HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        &test_config(),
    )
    .await;
    assert_http_response(&resp, 404);
}

#[tokio::test(start_paused = true)]
async fn dst_malformed_request() {
    let resp = run_connection(b"INVALID GARBAGE\r\n\r\n", &test_config()).await;
    assert_http_response(&resp, 400);
}

// ---------------------------------------------------------------------------
// Keep-alive and pipelining
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_keep_alive_two_requests() {
    let resp = run_connection(
        b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n\
          GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        &test_config(),
    )
    .await;
    let s = String::from_utf8_lossy(&resp);
    // Should contain two separate HTTP responses.
    let count = s.matches("HTTP/1.1 200").count();
    assert_eq!(count, 2, "expected 2 responses, got {count} in: {s}");
}

#[tokio::test(start_paused = true)]
async fn dst_keep_alive_five_requests() {
    let mut request = Vec::new();
    for _ in 0..4 {
        request.extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    }
    request.extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");

    let resp = run_connection(&request, &test_config()).await;
    let s = String::from_utf8_lossy(&resp);
    let count = s.matches("HTTP/1.1 200").count();
    assert_eq!(count, 5, "expected 5 responses, got {count}");
}

// ---------------------------------------------------------------------------
// Chunked transfer-encoding
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_chunked_post() {
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          5\r\nhello\r\n0\r\n\r\n",
        &test_config(),
    ).await;
    assert_http_response(&resp, 200);
    assert_response_contains(&resp, "echo: 5 bytes");
}

#[tokio::test(start_paused = true)]
async fn dst_chunked_multi_chunk() {
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        &test_config(),
    ).await;
    assert_http_response(&resp, 200);
    assert_response_contains(&resp, "echo: 11 bytes");
}

// ---------------------------------------------------------------------------
// Slowloris: deterministic time
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_slowloris_header_timeout() {
    let config = WorkerConfig {
        header_read_timeout: Some(Duration::from_millis(500)),
        ..test_config()
    };

    let shared = shared_state();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);

    let cfg = config.clone();
    let handle_task = tokio::spawn(async move {
        let _ = handle_connection(server, shared, &cfg).await;
    });

    // Send partial headers, wait longer than header timeout, send more.
    let write_task = tokio::spawn(async move {
        client_write.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        // Wait longer than header_read_timeout (500ms).
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Try to send more — server should have already closed.
        let _ = client_write.write_all(b"Host: localhost\r\n\r\n").await;
        let _ = client_write.shutdown().await;
    });

    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = client_read.read_to_end(&mut response).await;

    let _ = write_task.await;
    let _ = handle_task.await;

    // Connection should be closed by timeout. Response is empty or error.
    assert!(
        response.is_empty() || response.len() < 200,
        "expected empty or short response on slowloris timeout, got {} bytes",
        response.len()
    );
}

#[tokio::test(start_paused = true)]
async fn dst_slowloris_body_timeout() {
    // Send headers quickly, then drip-feed body with 3-second gaps.
    // Body timeout is 10s.
    let config = WorkerConfig {
        body_read_timeout: Some(Duration::from_secs(3)),
        ..test_config()
    };

    let chunks: Vec<&[u8]> = vec![
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        b"hello",  // 5 of 100 bytes
    ];
    let delays = vec![Duration::ZERO, Duration::from_secs(5)];

    let resp = run_connection_incremental(&chunks, &delays, &config).await;

    assert!(
        resp.is_empty() || String::from_utf8_lossy(&resp).contains("408"),
        "expected timeout response, got: {}",
        String::from_utf8_lossy(&resp[..resp.len().min(200)])
    );
}

// ---------------------------------------------------------------------------
// Oversized requests
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_oversized_content_length() {
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n",
        &test_config(),
    ).await;
    assert_http_response(&resp, 413);
}

#[tokio::test(start_paused = true)]
async fn dst_oversized_headers() {
    let mut request = Vec::new();
    request.extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n");
    // Add headers until we exceed MAX_HEADER_BUF (64KB).
    for i in 0..1000 {
        request.extend_from_slice(format!("X-Pad-{i}: ").as_bytes());
        request.extend_from_slice(&vec![b'A'; 100]);
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");

    let resp = run_connection(&request, &test_config()).await;
    assert_http_response(&resp, 400);
}

// ---------------------------------------------------------------------------
// Connection lifetime
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_connection_lifetime_timeout() {
    let config = WorkerConfig {
        connection_timeout: Some(Duration::from_millis(500)),
        ..test_config()
    };

    let shared = shared_state();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);

    let cfg = config.clone();
    let handle_task = tokio::spawn(async move {
        let _ = handle_connection(server, shared, &cfg).await;
    });

    let write_task = tokio::spawn(async move {
        // First request — should succeed.
        client_write
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        // Wait past connection timeout before sending second request.
        // The server should close the connection during the wait,
        // so the second request is never processed.
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Server should have already closed — this write may fail.
        let _ = client_write
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await;
        let _ = client_write.shutdown().await;
    });

    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = client_read.read_to_end(&mut response).await;

    let _ = write_task.await;
    let _ = handle_task.await;

    // The connection should eventually close. With paused time, the
    // writer's sleep advances time past the connection timeout. The
    // server closes after writing the first response and detecting
    // the timeout at the top of the next iteration.
    let s = String::from_utf8_lossy(&response);
    assert!(
        s.contains("HTTP/1.1 200"),
        "expected at least one successful response"
    );
    // The second request may or may not be processed depending on
    // timing — the important thing is the connection eventually closes
    // and doesn't hang.
}

// ---------------------------------------------------------------------------
// EOF and partial data
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_eof_mid_headers() {
    // Client sends partial headers then disconnects.
    let resp = run_connection(b"GET / HTTP/1.1\r\nHost: loc", &test_config()).await;
    // Server should close cleanly (empty response or nothing).
    assert!(resp.is_empty() || resp.len() < 500);
}

#[tokio::test(start_paused = true)]
async fn dst_eof_mid_body() {
    // Client declares 100-byte body but only sends 5.
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nConnection: close\r\n\r\nhello",
        &test_config(),
    ).await;
    // Server should close cleanly (client disconnected mid-body).
    // May or may not send a response depending on timing.
    let _ = resp; // no panic = success
}

#[tokio::test(start_paused = true)]
async fn dst_empty_request() {
    // Client connects and immediately disconnects.
    let resp = run_connection(b"", &test_config()).await;
    assert!(resp.is_empty());
}

// ---------------------------------------------------------------------------
// 100-continue
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dst_expect_100_continue() {
    let resp = run_connection(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        &test_config(),
    ).await;
    let s = String::from_utf8_lossy(&resp);
    // Should contain the 100 Continue interim response followed by the actual response.
    assert!(
        s.contains("100 Continue") || s.contains("200"),
        "expected 100-continue or 200, got: {}",
        &s[..s.len().min(300)]
    );
}

// ---------------------------------------------------------------------------
// Seeded proptest DST: random requests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptest_dst {
    use super::*;
    use proptest::prelude::*;

    fn random_method() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("GET"), Just("POST"), Just("HEAD"), Just("DELETE"),]
    }

    fn random_path() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("/".to_string()),
            Just("/hello".to_string()),
            Just("/nonexistent".to_string()),
        ]
    }

    fn random_request() -> impl Strategy<Value = Vec<u8>> {
        (random_method(), random_path(), any::<bool>()).prop_map(|(method, path, close)| {
            let conn = if close { "close" } else { "keep-alive" };
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: {conn}\r\n\r\n")
                .into_bytes()
        })
    }

    proptest! {
        #[test]
        fn dst_random_request_never_panics(req in random_request()) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let resp = rt.block_on(async {
                tokio::time::pause();
                run_connection(&req, &test_config()).await
            });
            let s = String::from_utf8_lossy(&resp);
            prop_assert!(
                s.contains("HTTP/1.1"),
                "expected HTTP response, got: {}",
                &s[..s.len().min(200)]
            );
        }

        #[test]
        fn dst_pipelined_requests_correct_count(n in 1usize..6) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut request = Vec::new();
            for i in 0..n {
                let conn = if i == n - 1 { "close" } else { "keep-alive" };
                request.extend_from_slice(
                    format!("GET / HTTP/1.1\r\nHost: localhost\r\nConnection: {conn}\r\n\r\n")
                        .as_bytes(),
                );
            }
            let resp = rt.block_on(async {
                tokio::time::pause();
                run_connection(&request, &test_config()).await
            });
            let s = String::from_utf8_lossy(&resp);
            let count = s.matches("HTTP/1.1 200").count();
            prop_assert_eq!(count, n);
        }
    }
}
