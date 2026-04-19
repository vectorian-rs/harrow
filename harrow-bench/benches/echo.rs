use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_core::path::PathPattern;
use http::Method;

use harrow_bench::{
    BenchClient, build_app_with_routes, json_1kb_handler, json_10kb_handler, json_handler,
    run_concurrent, run_concurrent_mixed, simulated_io_handler, start_server, text_handler,
};

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
            let app = || App::new().get("/echo", text_handler);
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
            let app = || App::new().get("/echo", json_handler);
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
            let app = || App::new().get("/users/:id", text_handler);
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

    // JSON 1KB echo, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = || App::new().get("/echo", json_1kb_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_1kb_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // JSON 10KB echo, 0 middleware
    let client = {
        let addr = rt.block_on(async {
            let app = || App::new().get("/echo", json_10kb_handler);
            start_server(app).await
        });
        Arc::new(Mutex::new(rt.block_on(BenchClient::connect(addr))))
    };
    group.bench_function("json_10kb_no_mw", |b| {
        let client = Arc::clone(&client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    // 404 path (zero-alloc miss)
    let client = {
        let addr = rt.block_on(async {
            let app = || App::new().get("/echo", text_handler);
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

// ---------------------------------------------------------------------------
// Concurrent TCP: multiple connections hitting the server simultaneously
// ---------------------------------------------------------------------------

fn bench_concurrent_tcp(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("concurrent_tcp");
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.sample_size(30);

    // --- Keep-alive pipelines: N connections × 10 requests each ---

    let addr_text = rt.block_on(async {
        let app = || App::new().get("/echo", text_handler);
        start_server(app).await
    });

    for n in [2, 8, 32] {
        group.bench_with_input(BenchmarkId::new("text_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_text, "/echo", n, 10))
        });
    }

    let addr_json = rt.block_on(async {
        let app = || App::new().get("/echo", json_1kb_handler);
        start_server(app).await
    });

    for n in [2, 8, 32] {
        group.bench_with_input(BenchmarkId::new("json_1kb_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_json, "/echo", n, 10))
        });
    }

    // --- Simulated I/O: 100µs sleep per request (models DB query) ---

    let addr_io = rt.block_on(async {
        let app = || App::new().get("/echo", simulated_io_handler);
        start_server(app).await
    });

    for n in [8, 32, 128] {
        group.bench_with_input(BenchmarkId::new("sim_io_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent(addr_io, "/echo", n, 10))
        });
    }

    // --- Mixed routes: health(text), /echo(json 1kb), /slow(sim I/O) ---

    let addr_mixed = rt.block_on(async {
        let app = || {
            App::new()
                .get("/health", text_handler)
                .get("/echo", json_1kb_handler)
                .get("/slow", simulated_io_handler)
        };
        start_server(app).await
    });

    let mixed_paths: &[&str] = &["/health", "/echo", "/echo", "/slow"];

    for n in [8, 32, 128] {
        group.bench_with_input(BenchmarkId::new("mixed_10rpc", n), &n, |b, &n| {
            b.to_async(&rt)
                .iter(|| run_concurrent_mixed(addr_mixed, mixed_paths, n, 10))
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_path_matching,
    bench_route_table_lookup,
    bench_echo_tcp,
    bench_concurrent_tcp
);
criterion_main!(benches);
