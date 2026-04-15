use std::net::SocketAddr;
use std::time::Duration;

use axum::{Json, Router, routing::get};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use serde_json::Value;

use harrow::App;
use harrow_bench::{JSON_1KB, json_1kb_handler, run_concurrent, start_server, text_handler};

// ---------------------------------------------------------------------------
// Axum handlers
// ---------------------------------------------------------------------------

async fn axum_text_handler() -> &'static str {
    "ok"
}

async fn axum_json_1kb_handler() -> Json<Value> {
    Json(JSON_1KB.clone())
}

async fn start_axum_server(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ---------------------------------------------------------------------------
// Worker-thread × concurrency sweep
// ---------------------------------------------------------------------------

const WORKER_COUNTS: &[usize] = &[1, 2, 4, 8];
const CONCURRENCY_LEVELS: &[usize] = &[32, 128];
const REQS_PER_CONN: usize = 10;

fn bench_scaling_text(c: &mut Criterion) {
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("scaling_text");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for &workers in WORKER_COUNTS {
        let server_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();

        let harrow_addr = server_rt.block_on(async {
            let app = || App::new().get("/text", text_handler);
            start_server(app).await
        });

        let axum_addr = server_rt.block_on(async {
            let app = Router::new().route("/text", get(axum_text_handler));
            start_axum_server(app).await
        });

        for &conc in CONCURRENCY_LEVELS {
            group.bench_with_input(
                BenchmarkId::new(format!("harrow/w{workers}"), conc),
                &conc,
                |b, &conc| {
                    b.to_async(&client_rt)
                        .iter(|| run_concurrent(harrow_addr, "/text", conc, REQS_PER_CONN))
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("axum/w{workers}"), conc),
                &conc,
                |b, &conc| {
                    b.to_async(&client_rt)
                        .iter(|| run_concurrent(axum_addr, "/text", conc, REQS_PER_CONN))
                },
            );
        }

        drop(server_rt);
    }

    group.finish();
}

fn bench_scaling_json_1kb(c: &mut Criterion) {
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("scaling_json_1kb");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for &workers in WORKER_COUNTS {
        let server_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap();

        let harrow_addr = server_rt.block_on(async {
            let app = || App::new().get("/json/1kb", json_1kb_handler);
            start_server(app).await
        });

        let axum_addr = server_rt.block_on(async {
            let app = Router::new().route("/json/1kb", get(axum_json_1kb_handler));
            start_axum_server(app).await
        });

        for &conc in CONCURRENCY_LEVELS {
            group.bench_with_input(
                BenchmarkId::new(format!("harrow/w{workers}"), conc),
                &conc,
                |b, &conc| {
                    b.to_async(&client_rt)
                        .iter(|| run_concurrent(harrow_addr, "/json/1kb", conc, REQS_PER_CONN))
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("axum/w{workers}"), conc),
                &conc,
                |b, &conc| {
                    b.to_async(&client_rt)
                        .iter(|| run_concurrent(axum_addr, "/json/1kb", conc, REQS_PER_CONN))
                },
            );
        }

        drop(server_rt);
    }

    group.finish();
}

criterion_group!(benches, bench_scaling_text, bench_scaling_json_1kb);
criterion_main!(benches);
