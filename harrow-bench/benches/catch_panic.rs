use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{BenchClient, header_middleware, noop_middleware, start_server, text_handler};

fn bench_catch_panic(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("catch_panic");

    // baseline: text handler, no middleware
    let baseline_addr = rt.block_on(async {
        let app = || App::new().get("/echo", text_handler);
        start_server(app).await
    });

    // catch_panic only
    let catch_panic_addr = rt.block_on(async {
        let app = || {
            App::new()
                .middleware(harrow::catch_panic_middleware)
                .get("/echo", text_handler)
        };
        start_server(app).await
    });

    // catch_panic + noop + header
    let stack_addr = rt.block_on(async {
        let app = || {
            App::new()
                .middleware(harrow::catch_panic_middleware)
                .middleware(noop_middleware)
                .middleware(header_middleware)
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

    let catch_panic_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(catch_panic_addr)),
    ));
    group.bench_function("catch_panic_only", |b| {
        let client = Arc::clone(&catch_panic_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let stack_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(stack_addr))));
    group.bench_function("catch_panic_plus_2mw", |b| {
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

criterion_group!(benches, bench_catch_panic);
criterion_main!(benches);
