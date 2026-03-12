use std::net::SocketAddr;
use std::sync::Arc;

use axum::{Json, Router, extract::Path, routing::get};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use harrow_bench::{BenchClient, JSON_1KB, JSON_10KB, run_concurrent, run_concurrent_mixed};

// ---------------------------------------------------------------------------
// Axum handlers (identical responses to Harrow echo.rs)
// ---------------------------------------------------------------------------

async fn text_handler() -> &'static str {
    "ok"
}

async fn json_handler() -> Json<Value> {
    Json(json!({"status": "ok", "code": 200}))
}

async fn param_handler(Path(_id): Path<String>) -> &'static str {
    "ok"
}

async fn json_1kb_handler() -> Json<Value> {
    Json(JSON_1KB.clone())
}

async fn json_10kb_handler() -> Json<Value> {
    Json(JSON_10KB.clone())
}

async fn simulated_io_handler() -> Json<Value> {
    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
    Json(JSON_1KB.clone())
}

// ---------------------------------------------------------------------------
// Server helper
// ---------------------------------------------------------------------------

async fn start_axum_server(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

// ---------------------------------------------------------------------------
// TCP round-trip: Axum echo handler, 0 middleware
// ---------------------------------------------------------------------------

fn bench_axum_echo_tcp(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("axum_echo_tcp");

    // Text echo
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(text_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("text_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // JSON echo
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Path param echo
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/users/{id}", get(param_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("param_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/users/42").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // JSON 1KB echo
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_1kb_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_1kb_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // JSON 10KB echo
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_10kb_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_10kb_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 404 path
    let client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(text_handler));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("404_miss", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/nope").await;
                debug_assert_eq!(status, 404);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Concurrent TCP: multiple connections hitting the Axum server simultaneously
// ---------------------------------------------------------------------------

fn bench_axum_concurrent_tcp(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("axum_concurrent_tcp");
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.sample_size(30);

    // --- Keep-alive pipelines: N connections × 10 requests each ---

    let addr_text = rt.block_on(async {
        let app = Router::new().route("/echo", get(text_handler));
        start_axum_server(app).await
    });

    for n in [2, 8, 32] {
        group.bench_with_input(BenchmarkId::new("text_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_text, "/echo", n, 10))
        });
    }

    let addr_json = rt.block_on(async {
        let app = Router::new().route("/echo", get(json_1kb_handler));
        start_axum_server(app).await
    });

    for n in [2, 8, 32] {
        group.bench_with_input(BenchmarkId::new("json_1kb_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_json, "/echo", n, 10))
        });
    }

    // --- Simulated I/O: 100µs sleep per request (models DB query) ---

    let addr_io = rt.block_on(async {
        let app = Router::new().route("/echo", get(simulated_io_handler));
        start_axum_server(app).await
    });

    for n in [8, 32, 128] {
        group.bench_with_input(BenchmarkId::new("sim_io_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_io, "/echo", n, 10))
        });
    }

    // --- Mixed routes: /health(text), /echo(json 1kb), /slow(sim I/O) ---

    let addr_mixed = rt.block_on(async {
        let app = Router::new()
            .route("/health", get(text_handler))
            .route("/echo", get(json_1kb_handler))
            .route("/slow", get(simulated_io_handler));
        start_axum_server(app).await
    });

    let mixed_paths: &[&str] = &["/health", "/echo", "/echo", "/slow"];

    for n in [8, 32, 128] {
        group.bench_with_input(BenchmarkId::new("mixed_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent_mixed(addr_mixed, mixed_paths, n, 10))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_axum_echo_tcp, bench_axum_concurrent_tcp);
criterion_main!(benches);
