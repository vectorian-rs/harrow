use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{
    BenchClient, HitCounter, header_middleware, noop_middleware, param_state_handler, start_server,
    text_handler, timing_middleware,
};

// ---------------------------------------------------------------------------
// TCP round-trip: realistic full-stack service
// ---------------------------------------------------------------------------

fn bench_full_stack(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("full_stack");

    // Full stack: state + path param + JSON response + 3 middleware
    let addr = rt.block_on(async {
        let counter = Arc::new(HitCounter(AtomicUsize::new(0)));
        let app = move || {
            App::new()
                .state(counter)
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .middleware(noop_middleware)
                .get("/users/:id", param_state_handler)
                .get("/health", text_handler)
        };
        start_server(app).await
    });

    let client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))));
    group.bench_function("json_3mw_state_param", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/users/42").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Comparison: same server, hit the /health route (exact match, no params)
    let client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))));
    group.bench_function("text_3mw_health", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/health").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: route table size impact with full middleware
// ---------------------------------------------------------------------------

fn bench_route_table_scaling(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("route_table_scaling");

    for n in [1usize, 10, 50, 100, 200] {
        let client = {
            let addr = rt.block_on(async {
                let app = move || {
                    let mut app = App::new()
                        .middleware(timing_middleware)
                        .middleware(header_middleware);

                    for i in 0..n.saturating_sub(1) {
                        let pattern: &'static str =
                            Box::leak(format!("/decoy-{i}").into_boxed_str());
                        app = app.get(pattern, text_handler);
                    }
                    app.get("/target/:id", text_handler)
                };
                start_server(app).await
            });
            Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
        };

        group.bench_function(format!("{n}_routes_2mw"), |b| {
            let client = Arc::clone(&client);
            b.to_async(&rt).iter(|| {
                let client = Arc::clone(&client);
                async move {
                    let (status, _) = client.lock().await.get("/target/42").await;
                    debug_assert_eq!(status, 200);
                }
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_full_stack, bench_route_table_scaling);
criterion_main!(benches);
