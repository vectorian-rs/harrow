use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Path, Request as AxumRequest, State};
use axum::middleware::{self, Next as AxumNext};
use axum::response::Response as AxumResponse;
use axum::{Json, Router, routing::get};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::HeaderValue;
use http_body_util::Full;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;

use harrow::App;
use harrow_core::dispatch::dispatch;
use harrow_core::request::full_body;

use harrow_bench::{
    BenchClient, HitCounter, header_middleware, noop_middleware, param_state_handler, start_server,
    text_handler, timing_middleware,
};

async fn start_axum_server(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn harrow_request(method: &str, uri: &str) -> http::Request<harrow_core::request::Body> {
    let body = full_body(Full::new(Bytes::new()));
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap()
}

fn axum_request(method: &str, uri: &str) -> http::Request<axum::body::Body> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn axum_text_handler() -> &'static str {
    "ok"
}

async fn axum_param_state_handler(
    Path(id): Path<String>,
    State(counter): State<Arc<AtomicUsize>>,
) -> Json<Value> {
    counter.fetch_add(1, Ordering::Relaxed);
    Json(json!({ "id": id, "status": "ok" }))
}

async fn axum_noop(req: AxumRequest, next: AxumNext) -> AxumResponse {
    next.run(req).await
}

async fn axum_header(req: AxumRequest, next: AxumNext) -> AxumResponse {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("x-bench", HeaderValue::from_static("1"));
    resp
}

async fn axum_timing(req: AxumRequest, next: AxumNext) -> AxumResponse {
    let _start = Instant::now();
    next.run(req).await
}

fn bench_response_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("perf_isolation/response_build");

    group.bench_function("harrow/text_static", |b| {
        b.iter(|| std::hint::black_box(harrow::Response::text("ok")))
    });

    group.bench_function("harrow/text_owned", |b| {
        b.iter(|| std::hint::black_box(harrow::Response::text(String::from("ok"))))
    });

    group.bench_function("harrow/into_response_static", |b| {
        b.iter(|| std::hint::black_box(<&'static str as harrow::IntoResponse>::into_response("ok")))
    });

    group.bench_function("harrow/into_response_bytes", |b| {
        b.iter(|| {
            std::hint::black_box(<Bytes as harrow::IntoResponse>::into_response(
                Bytes::from_static(b"ok"),
            ))
        })
    });

    group.bench_function("axum/into_response_static", |b| {
        b.iter(|| {
            std::hint::black_box(
                <&'static str as axum::response::IntoResponse>::into_response("ok"),
            )
        })
    });

    group.bench_function("axum/into_response_owned", |b| {
        b.iter(|| {
            std::hint::black_box(<String as axum::response::IntoResponse>::into_response(
                String::from("ok"),
            ))
        })
    });

    group.bench_function("axum/into_response_bytes", |b| {
        b.iter(|| {
            std::hint::black_box(<Bytes as axum::response::IntoResponse>::into_response(
                Bytes::from_static(b"ok"),
            ))
        })
    });

    group.finish();
}

fn bench_inproc_full_pipeline(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("perf_isolation/inproc_full_pipeline");

    let harrow_shared = {
        let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
        let app = App::new()
            .state(counter)
            .middleware(timing_middleware)
            .middleware(header_middleware)
            .middleware(noop_middleware)
            .get("/users/:id", param_state_handler)
            .get("/health", text_handler);
        app.into_shared_state()
    };

    let axum_router = {
        let counter = Arc::new(AtomicUsize::new(0));
        Router::new()
            .route("/users/{id}", get(axum_param_state_handler))
            .route("/health", get(axum_text_handler))
            .layer(middleware::from_fn(axum_noop))
            .layer(middleware::from_fn(axum_header))
            .layer(middleware::from_fn(axum_timing))
            .with_state(counter)
    };

    group.bench_function("harrow/health_3mw", |b| {
        let shared = Arc::clone(&harrow_shared);
        b.to_async(&rt).iter(|| {
            let shared = Arc::clone(&shared);
            async move {
                let req = harrow_request("GET", "/health");
                let resp = dispatch(shared, req).await;
                std::hint::black_box(resp);
            }
        })
    });

    group.bench_function("axum/health_3mw", |b| {
        let router = axum_router.clone();
        b.to_async(&rt).iter(|| {
            let router = router.clone();
            async move {
                let req = axum_request("GET", "/health");
                let resp = router.oneshot(req).await.unwrap();
                std::hint::black_box(resp);
            }
        })
    });

    group.bench_function("harrow/json_state_param_3mw", |b| {
        let shared = Arc::clone(&harrow_shared);
        b.to_async(&rt).iter(|| {
            let shared = Arc::clone(&shared);
            async move {
                let req = harrow_request("GET", "/users/42");
                let resp = dispatch(shared, req).await;
                std::hint::black_box(resp);
            }
        })
    });

    group.bench_function("axum/json_state_param_3mw", |b| {
        let router = axum_router.clone();
        b.to_async(&rt).iter(|| {
            let router = router.clone();
            async move {
                let req = axum_request("GET", "/users/42");
                let resp = router.oneshot(req).await.unwrap();
                std::hint::black_box(resp);
            }
        })
    });

    group.finish();
}

fn bench_connection_lifecycle(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("perf_isolation/connection_lifecycle");

    let harrow_addr = rt.block_on(async {
        let app = || App::new().get("/echo", text_handler);
        start_server(app).await
    });

    let axum_addr = rt.block_on(async {
        let app = Router::new().route("/echo", get(axum_text_handler));
        start_axum_server(app).await
    });

    let harrow_keep_alive = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(harrow_addr))));
    group.bench_function(BenchmarkId::new("harrow", "keep_alive"), |b| {
        let client = Arc::clone(&harrow_keep_alive);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.bench_function(BenchmarkId::new("harrow", "new_conn"), |b| {
        b.to_async(&rt).iter(|| async move {
            let mut client = BenchClient::connect(harrow_addr).await;
            let (status, _) = client.get("/echo").await;
            debug_assert_eq!(status, 200);
        })
    });

    let axum_keep_alive = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(axum_addr))));
    group.bench_function(BenchmarkId::new("axum", "keep_alive"), |b| {
        let client = Arc::clone(&axum_keep_alive);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.bench_function(BenchmarkId::new("axum", "new_conn"), |b| {
        b.to_async(&rt).iter(|| async move {
            let mut client = BenchClient::connect(axum_addr).await;
            let (status, _) = client.get("/echo").await;
            debug_assert_eq!(status, 200);
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_response_build,
    bench_inproc_full_pipeline,
    bench_connection_lifecycle
);
criterion_main!(benches);
