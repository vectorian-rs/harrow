use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{BenchClient, noop_middleware, start_server, text_handler};

fn bench_body_limit(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("body_limit");

    // baseline: text handler, no middleware
    let baseline_addr = rt.block_on(async {
        let app = || App::new().get("/echo", text_handler);
        start_server(app).await
    });

    // body_limit only (1 MiB limit)
    let body_limit_addr = rt.block_on(async {
        let app = || {
            App::new()
                .middleware(harrow::body_limit_middleware(1024 * 1024))
                .get("/echo", text_handler)
        };
        start_server(app).await
    });

    // body_limit + noop middleware
    let stack_addr = rt.block_on(async {
        let app = || {
            App::new()
                .middleware(harrow::body_limit_middleware(1024 * 1024))
                .middleware(noop_middleware)
                .get("/echo", text_handler)
        };
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

    let body_limit_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(body_limit_addr)),
    ));
    group.bench_function("body_limit_only", |b| {
        let client = Arc::clone(&body_limit_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let stack_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(stack_addr))));
    group.bench_function("body_limit_plus_noop", |b| {
        let client = Arc::clone(&stack_client);
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

criterion_group!(benches, bench_body_limit);
criterion_main!(benches);
