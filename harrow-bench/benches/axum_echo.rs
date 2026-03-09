use std::net::SocketAddr;
use std::sync::Arc;

use axum::{Json, Router, extract::Path, routing::get};
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use harrow_bench::BenchClient;

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

criterion_group!(benches, bench_axum_echo_tcp);
criterion_main!(benches);
