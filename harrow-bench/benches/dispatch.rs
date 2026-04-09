use std::sync::Arc;

use axum::{Json, Router, routing::get};
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http_body_util::Full;
use serde_json::Value;

use harrow::App;
use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::full_body;

use harrow_bench::{JSON_1KB, json_1kb_handler, text_handler};

// ---------------------------------------------------------------------------
// Axum handlers (same responses as Harrow)
// ---------------------------------------------------------------------------

async fn axum_text_handler() -> &'static str {
    "ok"
}

async fn axum_json_1kb_handler() -> Json<Value> {
    Json(JSON_1KB.clone())
}

// ---------------------------------------------------------------------------
// Helper: build an http::Request<Body> for dispatch
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Benchmark: in-process dispatch (no TCP, no HTTP parsing)
// ---------------------------------------------------------------------------

fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("dispatch");

    // --- Harrow setup ---
    let harrow_text_shared = App::new().get("/echo", text_handler).into_shared_state();

    let harrow_json_shared = App::new()
        .get("/echo", json_1kb_handler)
        .into_shared_state();

    // --- Axum setup ---
    let axum_text_router = Router::new().route("/echo", get(axum_text_handler));
    let axum_json_router = Router::new().route("/echo", get(axum_json_1kb_handler));

    // --- Harrow text ---
    group.bench_function("harrow_text", |b| {
        let shared = Arc::clone(&harrow_text_shared);
        b.to_async(&rt).iter(|| {
            let shared = Arc::clone(&shared);
            async move {
                let req = harrow_request("GET", "/echo");
                let resp = dispatch(shared, req).await;
                std::hint::black_box(resp);
            }
        })
    });

    // --- Axum text ---
    group.bench_function("axum_text", |b| {
        let router = axum_text_router.clone();
        b.to_async(&rt).iter(|| {
            let router = router.clone();
            async move {
                use tower::ServiceExt;
                let req = axum_request("GET", "/echo");
                let resp = router.oneshot(req).await.unwrap();
                std::hint::black_box(resp);
            }
        })
    });

    // --- Harrow json 1kb ---
    group.bench_function("harrow_json_1kb", |b| {
        let shared = Arc::clone(&harrow_json_shared);
        b.to_async(&rt).iter(|| {
            let shared = Arc::clone(&shared);
            async move {
                let req = harrow_request("GET", "/echo");
                let resp = dispatch(shared, req).await;
                std::hint::black_box(resp);
            }
        })
    });

    // --- Axum json 1kb ---
    group.bench_function("axum_json_1kb", |b| {
        let router = axum_json_router.clone();
        b.to_async(&rt).iter(|| {
            let router = router.clone();
            async move {
                use tower::ServiceExt;
                let req = axum_request("GET", "/echo");
                let resp = router.oneshot(req).await.unwrap();
                std::hint::black_box(resp);
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
