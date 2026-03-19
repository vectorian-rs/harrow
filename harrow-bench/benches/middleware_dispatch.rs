use std::sync::Arc;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http_body_util::Full;

use harrow::App;
use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::full_body;

use harrow_bench::{json_1kb_handler, noop_middleware, text_handler};

const DEPTHS: &[usize] = &[0, 1, 2, 3, 5, 10];

fn harrow_request(method: &str, uri: &str) -> http::Request<harrow_core::request::Body> {
    let body = full_body(Full::new(Bytes::new()));
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap()
}

fn shared_state_from_app(app: App) -> Arc<SharedState> {
    let (route_table, middleware, state, max_body_size) = app.into_parts();
    Arc::new(SharedState {
        route_table,
        middleware,
        state: Arc::new(state),
        max_body_size,
    })
}

fn build_text_shared_state(depth: usize) -> Arc<SharedState> {
    let mut app = App::new();
    for _ in 0..depth {
        app = app.middleware(noop_middleware);
    }
    shared_state_from_app(app.get("/echo", text_handler))
}

fn build_json_shared_state(depth: usize) -> Arc<SharedState> {
    let mut app = App::new();
    for _ in 0..depth {
        app = app.middleware(noop_middleware);
    }
    shared_state_from_app(app.get("/echo", json_1kb_handler))
}

fn bench_text_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_dispatch/text");

    for &depth in DEPTHS {
        let shared = build_text_shared_state(depth);
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, _| {
            let shared = Arc::clone(&shared);
            b.to_async(&rt).iter(|| {
                let shared = Arc::clone(&shared);
                async move {
                    let req = harrow_request("GET", "/echo");
                    let resp = dispatch(shared, req).await;
                    std::hint::black_box(resp);
                }
            })
        });
    }

    group.finish();
}

fn bench_json_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_dispatch/json_1kb");

    for &depth in DEPTHS {
        let shared = build_json_shared_state(depth);
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, _| {
            let shared = Arc::clone(&shared);
            b.to_async(&rt).iter(|| {
                let shared = Arc::clone(&shared);
                async move {
                    let req = harrow_request("GET", "/echo");
                    let resp = dispatch(shared, req).await;
                    std::hint::black_box(resp);
                }
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_text_dispatch, bench_json_dispatch);
criterion_main!(benches);
