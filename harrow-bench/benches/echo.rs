use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_core::path::PathPattern;
use http::Method;

use harrow_bench::{BenchClient, build_app_with_routes, json_handler, start_server, text_handler};

// ---------------------------------------------------------------------------
// Micro-benchmarks: PathPattern::match_path (no TCP, no IO)
// ---------------------------------------------------------------------------

fn bench_path_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("path_match");

    let exact = PathPattern::parse("/health");
    group.bench_function("exact_hit", |b| {
        b.iter(|| exact.match_path(std::hint::black_box("/health")))
    });

    group.bench_function("exact_miss", |b| {
        b.iter(|| exact.match_path(std::hint::black_box("/other")))
    });

    let one_param = PathPattern::parse("/users/:id");
    group.bench_function("1_param", |b| {
        b.iter(|| one_param.match_path(std::hint::black_box("/users/42")))
    });

    let two_params = PathPattern::parse("/orgs/:org/repos/:repo");
    group.bench_function("2_params", |b| {
        b.iter(|| two_params.match_path(std::hint::black_box("/orgs/acme/repos/widget")))
    });

    let glob = PathPattern::parse("/files/*path");
    group.bench_function("glob", |b| {
        b.iter(|| glob.match_path(std::hint::black_box("/files/a/b/c/d.txt")))
    });

    // Zero-alloc matches() for 404/405 detection
    group.bench_function("matches_no_alloc", |b| {
        b.iter(|| one_param.matches(std::hint::black_box("/users/42")))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Micro-benchmarks: RouteTable::match_route_idx (no TCP, no IO)
// ---------------------------------------------------------------------------

fn bench_route_table_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_table_lookup");

    for n in [1usize, 10, 50, 100, 200, 500] {
        // Build a route table with n routes where the target is last.
        let app = build_app_with_routes(n);
        let table = app.route_table();

        group.bench_with_input(BenchmarkId::new("worst_case", n), &n, |b, _| {
            b.iter(|| {
                table.match_route_idx(
                    std::hint::black_box(&Method::GET),
                    std::hint::black_box("/target/42"),
                )
            })
        });
    }

    // Best case: target is the first route.
    let app = App::new()
        .get("/target/:id", text_handler)
        .get("/other1", text_handler)
        .get("/other2", text_handler);
    let table = app.route_table();
    group.bench_function("best_case_3", |b| {
        b.iter(|| {
            table.match_route_idx(
                std::hint::black_box(&Method::GET),
                std::hint::black_box("/target/42"),
            )
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// TCP round-trip: echo handler, 0 middleware
// ---------------------------------------------------------------------------

fn bench_echo_tcp(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("echo_tcp");

    // Text echo, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/echo", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("text_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // JSON echo, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/echo", json_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // Text echo with path param, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/users/:id", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("param_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/users/42").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 404 path (zero-alloc miss)
    let client = {
        let addr = rt.block_on(async {
            let app = App::new().get("/echo", text_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("404_miss", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/nope").await;
                debug_assert_eq!(status, 404);
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_path_matching,
    bench_route_table_lookup,
    bench_echo_tcp
);
criterion_main!(benches);
