use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::timeout::TimeoutLayer;

use harrow::App;
use harrow_bench::{BenchClient, run_concurrent_with_headers, start_server};

// ---------------------------------------------------------------------------
// Handlers — identical 1KB body for both frameworks
// ---------------------------------------------------------------------------

const BODY_1KB: &str = include_str!("body_1kb.txt");

async fn harrow_text_handler(_req: harrow::Request) -> harrow::Response {
    harrow::Response::text(BODY_1KB)
}

async fn axum_text_handler() -> String {
    BODY_1KB.to_string()
}

// ---------------------------------------------------------------------------
// tower-http MakeRequestId impl
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct UuidRequestId;

impl MakeRequestId for UuidRequestId {
    fn make_request_id<B>(&mut self, _request: &http::Request<B>) -> Option<RequestId> {
        let id = uuid::Uuid::new_v4().to_string();
        Some(RequestId::new(id.parse().unwrap()))
    }
}

// ---------------------------------------------------------------------------
// Axum server helper
// ---------------------------------------------------------------------------

async fn start_axum_server(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ---------------------------------------------------------------------------
// Request headers for benchmarks
// ---------------------------------------------------------------------------

const BENCH_HEADERS: &[(&str, &str)] = &[
    ("accept-encoding", "gzip"),
    ("origin", "https://bench.example.com"),
];

// ---------------------------------------------------------------------------
// Group 1: Individual middleware comparison
// ---------------------------------------------------------------------------

fn bench_middleware_individual(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_individual");

    // --- Timeout ---

    let harrow_client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(harrow::timeout_middleware(Duration::from_secs(5)))
                .get("/echo", harrow_text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("timeout/harrow", |b| {
        let client = Arc::clone(&harrow_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let axum_client = {
        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(axum_text_handler)).layer(
                TimeoutLayer::with_status_code(
                    http::StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(5),
                ),
            );
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("timeout/axum", |b| {
        let client = Arc::clone(&axum_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // --- Request ID ---

    let harrow_client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(harrow::request_id_middleware)
                .get("/echo", harrow_text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("request_id/harrow", |b| {
        let client = Arc::clone(&harrow_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let axum_client = {
        let addr = rt.block_on(async {
            let app = Router::new()
                .route("/echo", get(axum_text_handler))
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(SetRequestIdLayer::x_request_id(UuidRequestId));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("request_id/axum", |b| {
        let client = Arc::clone(&axum_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // --- CORS ---

    let harrow_client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .get("/echo", harrow_text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("cors/harrow", |b| {
        let client = Arc::clone(&harrow_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("origin", "https://bench.example.com")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let axum_client = {
        let addr = rt.block_on(async {
            let app = Router::new()
                .route("/echo", get(axum_text_handler))
                .layer(CorsLayer::permissive());
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("cors/axum", |b| {
        let client = Arc::clone(&axum_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("origin", "https://bench.example.com")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // --- Compression ---

    let harrow_client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(harrow::compression_middleware)
                .get("/echo", harrow_text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("compression/harrow", |b| {
        let client = Arc::clone(&harrow_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("accept-encoding", "gzip")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let axum_client = {
        let addr = rt.block_on(async {
            let app = Router::new()
                .route("/echo", get(axum_text_handler))
                .layer(CompressionLayer::new());
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("compression/axum", |b| {
        let client = Arc::clone(&axum_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("accept-encoding", "gzip")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: Full middleware stack — single TCP round-trip
// ---------------------------------------------------------------------------

fn bench_middleware_full_stack(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_full_stack");

    // Harrow: timeout → request_id → cors → compression
    let harrow_client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(harrow::timeout_middleware(Duration::from_secs(5)))
                .middleware(harrow::request_id_middleware)
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .middleware(harrow::compression_middleware)
                .get("/echo", harrow_text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("harrow", |b| {
        let client = Arc::clone(&harrow_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", BENCH_HEADERS)
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Axum: TimeoutLayer → SetRequestId+Propagate → CorsLayer → CompressionLayer
    let axum_client = {
        let addr = rt.block_on(async {
            let app = Router::new()
                .route("/echo", get(axum_text_handler))
                .layer(CompressionLayer::new())
                .layer(CorsLayer::permissive())
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(SetRequestIdLayer::x_request_id(UuidRequestId))
                .layer(TimeoutLayer::with_status_code(
                    http::StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(5),
                ));
            start_axum_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("axum", |b| {
        let client = Arc::clone(&axum_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", BENCH_HEADERS)
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: Full stack under concurrency
// ---------------------------------------------------------------------------

fn bench_middleware_concurrent(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("middleware_concurrent");
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(30);

    // Harrow full stack server
    let harrow_addr = rt.block_on(async {
        let app = App::new()
            .middleware(harrow::timeout_middleware(Duration::from_secs(5)))
            .middleware(harrow::request_id_middleware)
            .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
            .middleware(harrow::compression_middleware)
            .get("/echo", harrow_text_handler);
        start_server(app).await
    });

    // Axum full stack server
    let axum_addr = rt.block_on(async {
        let app = Router::new()
            .route("/echo", get(axum_text_handler))
            .layer(CompressionLayer::new())
            .layer(CorsLayer::permissive())
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId))
            .layer(TimeoutLayer::with_status_code(
                http::StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(5),
            ));
        start_axum_server(app).await
    });

    for conns in [8, 32] {
        group.bench_with_input(BenchmarkId::new("harrow", conns), &conns, |b, &conns| {
            b.to_async(&rt).iter(|| {
                run_concurrent_with_headers(harrow_addr, "/echo", BENCH_HEADERS, conns, 10)
            })
        });

        group.bench_with_input(BenchmarkId::new("axum", conns), &conns, |b, &conns| {
            b.to_async(&rt)
                .iter(|| run_concurrent_with_headers(axum_addr, "/echo", BENCH_HEADERS, conns, 10))
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_middleware_individual,
    bench_middleware_full_stack,
    bench_middleware_concurrent
);
criterion_main!(benches);
