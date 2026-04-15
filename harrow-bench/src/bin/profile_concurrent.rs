//! Profiling binary for concurrent benchmarks.
//!
//! Sweeps multiple concurrency levels and handler types to find real gaps
//! between Harrow and Axum under concurrent load.
//!
//! Usage:
//!   cargo run --release --bin profile-concurrent
//!   cargo flamegraph --bin profile-concurrent -- harrow   # flamegraph one framework

use std::net::SocketAddr;
use std::time::Instant;

const REQS_PER_CONN: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "both".into());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // ---------- Simulated I/O (100µs sleep + JSON 1KB) ----------

    println!("\n=== Simulated I/O handler (100µs sleep + JSON 1KB) ===");
    println!(
        "{:<8} {:>14} {:>14} {:>10}",
        "Conns", "Harrow", "Axum", "Delta"
    );
    println!("{}", "-".repeat(50));

    let harrow_io = rt.block_on(start_harrow_io());
    let axum_io = rt.block_on(start_axum_io());

    for conns in [8, 32, 64, 128, 256] {
        let h = if mode == "axum" {
            0.0
        } else {
            bench_one(&rt, harrow_io, "/echo", conns)
        };
        let a = if mode == "harrow" {
            0.0
        } else {
            bench_one(&rt, axum_io, "/echo", conns)
        };
        if mode == "both" {
            let delta = ((h - a) / a) * 100.0;
            println!("{:<8} {:>12.2}ms {:>12.2}ms {:>+9.1}%", conns, h, a, delta);
        } else {
            println!("{:<8} {:>12.2}ms {:>12.2}ms", conns, h, a);
        }
    }

    // ---------- Pure framework overhead (text handler, no sleep) ----------

    println!("\n=== Pure text handler (no sleep, framework overhead only) ===");
    println!(
        "{:<8} {:>14} {:>14} {:>10}",
        "Conns", "Harrow", "Axum", "Delta"
    );
    println!("{}", "-".repeat(50));

    let harrow_text = rt.block_on(start_harrow_text());
    let axum_text = rt.block_on(start_axum_text());

    for conns in [8, 32, 64, 128, 256] {
        let h = if mode == "axum" {
            0.0
        } else {
            bench_one(&rt, harrow_text, "/echo", conns)
        };
        let a = if mode == "harrow" {
            0.0
        } else {
            bench_one(&rt, axum_text, "/echo", conns)
        };
        if mode == "both" {
            let delta = ((h - a) / a) * 100.0;
            println!("{:<8} {:>12.2}ms {:>12.2}ms {:>+9.1}%", conns, h, a, delta);
        } else {
            println!("{:<8} {:>12.2}ms {:>12.2}ms", conns, h, a);
        }
    }

    // ---------- JSON 1KB (serialization under load) ----------

    println!("\n=== JSON 1KB handler (serialization under concurrent load) ===");
    println!(
        "{:<8} {:>14} {:>14} {:>10}",
        "Conns", "Harrow", "Axum", "Delta"
    );
    println!("{}", "-".repeat(50));

    let harrow_json = rt.block_on(start_harrow_json());
    let axum_json = rt.block_on(start_axum_json());

    for conns in [8, 32, 64, 128, 256] {
        let h = if mode == "axum" {
            0.0
        } else {
            bench_one(&rt, harrow_json, "/echo", conns)
        };
        let a = if mode == "harrow" {
            0.0
        } else {
            bench_one(&rt, axum_json, "/echo", conns)
        };
        if mode == "both" {
            let delta = ((h - a) / a) * 100.0;
            println!("{:<8} {:>12.2}ms {:>12.2}ms {:>+9.1}%", conns, h, a, delta);
        } else {
            println!("{:<8} {:>12.2}ms {:>12.2}ms", conns, h, a);
        }
    }

    println!();
}

/// Run ROUNDS iterations of (conns × REQS_PER_CONN) and return avg ms/round.
fn bench_one(rt: &tokio::runtime::Runtime, addr: SocketAddr, path: &str, conns: usize) -> f64 {
    // Warmup
    for _ in 0..5 {
        rt.block_on(harrow_bench::run_concurrent(
            addr,
            path,
            conns,
            REQS_PER_CONN,
        ));
    }
    let start = Instant::now();
    for _ in 0..ROUNDS {
        rt.block_on(harrow_bench::run_concurrent(
            addr,
            path,
            conns,
            REQS_PER_CONN,
        ));
    }
    start.elapsed().as_secs_f64() * 1000.0 / ROUNDS as f64
}

// ---------------------------------------------------------------------------
// Server setup helpers
// ---------------------------------------------------------------------------

async fn start_harrow_io() -> SocketAddr {
    let app = || harrow::App::new().get("/echo", harrow_bench::simulated_io_handler);
    harrow_bench::start_server(app).await
}

async fn start_harrow_text() -> SocketAddr {
    let app = || harrow::App::new().get("/echo", harrow_bench::text_handler);
    harrow_bench::start_server(app).await
}

async fn start_harrow_json() -> SocketAddr {
    let app = || harrow::App::new().get("/echo", harrow_bench::json_1kb_handler);
    harrow_bench::start_server(app).await
}

async fn axum_server(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

async fn start_axum_io() -> SocketAddr {
    use axum::{Json, Router, routing::get};
    use serde_json::Value;
    async fn handler() -> Json<Value> {
        tokio::time::sleep(std::time::Duration::from_micros(100)).await;
        Json(harrow_bench::JSON_1KB.clone())
    }
    axum_server(Router::new().route("/echo", get(handler))).await
}

async fn start_axum_text() -> SocketAddr {
    use axum::{Router, routing::get};
    axum_server(Router::new().route("/echo", get(|| async { "ok" }))).await
}

async fn start_axum_json() -> SocketAddr {
    use axum::{Json, Router, routing::get};
    use serde_json::Value;
    async fn handler() -> Json<Value> {
        Json(harrow_bench::JSON_1KB.clone())
    }
    axum_server(Router::new().route("/echo", get(handler))).await
}
