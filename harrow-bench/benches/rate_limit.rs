use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{BenchClient, noop_middleware, start_server, text_handler};

fn bench_rate_limit(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("rate_limit");

    // baseline: text handler, no middleware
    let baseline_addr = rt.block_on(async {
        let app = App::new().get("/echo", text_handler);
        start_server(app).await
    });

    // rate_limit only (high limit so requests always pass)
    let rate_limit_addr = rt.block_on(async {
        let backend = harrow::InMemoryBackend::per_second(1_000_000).burst(1_000_000);
        let app = App::new()
            .middleware(harrow::rate_limit_middleware(
                backend,
                harrow::HeaderKeyExtractor::new("x-api-key"),
            ))
            .get("/echo", text_handler);
        start_server(app).await
    });

    // rate_limit + noop middleware
    let stack_addr = rt.block_on(async {
        let backend = harrow::InMemoryBackend::per_second(1_000_000).burst(1_000_000);
        let app = App::new()
            .middleware(harrow::rate_limit_middleware(
                backend,
                harrow::HeaderKeyExtractor::new("x-api-key"),
            ))
            .middleware(noop_middleware)
            .get("/echo", text_handler);
        start_server(app).await
    });

    // rate_limit with no key header (skip path)
    let skip_addr = rt.block_on(async {
        let backend = harrow::InMemoryBackend::per_second(1_000_000).burst(1_000_000);
        let app = App::new()
            .middleware(harrow::rate_limit_middleware(
                backend,
                harrow::HeaderKeyExtractor::new("x-api-key"),
            ))
            .get("/echo", text_handler);
        start_server(app).await
    });

    let baseline_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(baseline_addr))));
    group.bench_function("baseline_0mw", |b| {
        let client = Arc::clone(&baseline_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let rate_limit_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(rate_limit_addr)),
    ));
    group.bench_function("rate_limit_only", |b| {
        let client = Arc::clone(&rate_limit_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("x-api-key", "bench-key")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let stack_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(stack_addr))));
    group.bench_function("rate_limit_plus_noop", |b| {
        let client = Arc::clone(&stack_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("x-api-key", "bench-key")])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let skip_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(skip_addr))));
    group.bench_function("rate_limit_skip_no_key", |b| {
        let client = Arc::clone(&skip_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_rate_limit);
criterion_main!(benches);
