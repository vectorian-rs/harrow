use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{
    BenchClient, header_middleware, noop_middleware, start_server, text_handler, timing_middleware,
};

// ---------------------------------------------------------------------------
// TCP round-trip: varying noop middleware depth
// ---------------------------------------------------------------------------

fn bench_middleware_depth(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_depth");

    for depth in [0u32, 1, 2, 3, 5, 10] {
        let client = {
            let addr = rt.block_on(async {
                let mut app = App::new();
                for _ in 0..depth {
                    app = app.middleware(noop_middleware);
                }
                app = app.get("/ping", text_handler);
                start_server(app).await
            });
            Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
        };

        group.bench_with_input(BenchmarkId::new("noop", depth), &depth, |b, _| {
            let client = Arc::clone(&client);
            b.to_async(&rt).iter(|| {
                let client = Arc::clone(&client);
                async move {
                    let (status, _) = client.lock().await.get("/ping").await;
                    debug_assert_eq!(status, 200);
                }
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: realistic middleware that does work
// ---------------------------------------------------------------------------

fn bench_middleware_realistic(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware_realistic");

    // 0 middleware baseline
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/ping", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("baseline_0mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 3 middleware: timing + header + noop
    let client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .middleware(noop_middleware)
                .get("/ping", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("3mw_mixed", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 5 middleware: timing + 2x header + 2x noop
    let client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .middleware(noop_middleware)
                .middleware(header_middleware)
                .middleware(noop_middleware)
                .get("/ping", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("5mw_mixed", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_middleware_depth, bench_middleware_realistic);
criterion_main!(benches);
