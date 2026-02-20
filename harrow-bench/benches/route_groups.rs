use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{
    group_tag_middleware, header_middleware, noop_middleware, start_server, text_handler,
    timing_middleware, BenchClient,
};

// ---------------------------------------------------------------------------
// TCP round-trip: group routes vs top-level routes
// ---------------------------------------------------------------------------

fn bench_group_vs_toplevel(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("group_vs_toplevel");

    // Baseline: top-level route, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/ping", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("toplevel_0mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Group route, 0 group middleware (just prefix overhead)
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().group("/api", |g| g.get("/ping", text_handler));
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("group_0mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Group route, 1 group middleware
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().group("/api", |g| {
                g.middleware(group_tag_middleware)
                    .get("/ping", text_handler)
            });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("group_1mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: scaling group middleware depth
// ---------------------------------------------------------------------------

fn bench_group_middleware_depth(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("group_mw_depth");

    for depth in [0u32, 1, 2, 3, 5] {
        let client = {
            let addr = rt.block_on(async {
                let app = App::new().group("/api", |mut g| {
                    for _ in 0..depth {
                        g = g.middleware(noop_middleware);
                    }
                    g.get("/ping", text_handler)
                });
                start_server(app).await
            });
            Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
        };

        group.bench_with_input(
            BenchmarkId::new("noop", depth),
            &depth,
            |b, _| {
                let client = Arc::clone(&client);
                b.to_async(&rt).iter(|| {
                    let client = Arc::clone(&client);
                    async move {
                        let (status, _) = client.lock().await.get("/api/ping").await;
                        debug_assert_eq!(status, 200);
                    }
                })
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: nested groups with middleware at each level
// ---------------------------------------------------------------------------

fn bench_nested_groups(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("nested_groups");

    // 1 level: /api (1 mw)
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().group("/api", |g| {
                g.middleware(group_tag_middleware)
                    .get("/ping", text_handler)
            });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("1_level_1mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 2 levels: /api (1 mw) -> /v1 (1 mw) = 2 mw total
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().group("/api", |g| {
                g.middleware(group_tag_middleware).group("/v1", |v1| {
                    v1.middleware(header_middleware)
                        .get("/ping", text_handler)
                })
            });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("2_levels_2mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/v1/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 3 levels: /api (1 mw) -> /v1 (1 mw) -> /admin (1 mw) = 3 mw total
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().group("/api", |g| {
                g.middleware(group_tag_middleware).group("/v1", |v1| {
                    v1.middleware(header_middleware).group("/admin", |admin| {
                        admin
                            .middleware(timing_middleware)
                            .get("/ping", text_handler)
                    })
                })
            });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("3_levels_3mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/v1/admin/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: global + group middleware combined
// ---------------------------------------------------------------------------

fn bench_global_plus_group(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("global_plus_group");

    // 2 global + 0 group (baseline for comparison)
    let client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .group("/api", |g| g.get("/ping", text_handler));
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("2global_0group", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 2 global + 2 group = 4 mw total
    let client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .group("/api", |g| {
                    g.middleware(group_tag_middleware)
                        .middleware(noop_middleware)
                        .get("/ping", text_handler)
                });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("2global_2group", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 2 global + 3 group (nested) = 5 mw total
    let client = {
        let addr = rt.block_on(async {
            let app = App::new()
                .middleware(timing_middleware)
                .middleware(header_middleware)
                .group("/api", |g| {
                    g.middleware(group_tag_middleware).group("/v1", |v1| {
                        v1.middleware(noop_middleware)
                            .middleware(header_middleware)
                            .get("/ping", text_handler)
                    })
                });
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("2global_3group_nested", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/api/v1/ping").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_group_vs_toplevel,
    bench_group_middleware_depth,
    bench_nested_groups,
    bench_global_plus_group,
);
criterion_main!(benches);
