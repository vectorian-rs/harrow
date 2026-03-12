//! Statistical benchmarking: runs many independent trials, computes
//! confidence intervals and required sample size for significance.
//!
//! Usage:
//!   cargo run --release --bin stat-bench -- [trials]
//!
//! Default: 30 trials. Each trial = 50 rounds of (32 conn × 10 reqs).

use std::net::SocketAddr;
use std::time::Instant;

const CONCURRENCY: usize = 32;
const REQS_PER_CONN: usize = 10;
const ROUNDS_PER_TRIAL: usize = 50;

fn main() {
    let trials: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    println!(
        "Running {trials} paired trials ({ROUNDS_PER_TRIAL} rounds × {CONCURRENCY} conn × {REQS_PER_CONN} rpc each)\n"
    );

    // --- Text handler ---
    let harrow_text = rt.block_on(start_harrow_text());
    let axum_text = rt.block_on(start_axum_text());
    let (h_text, a_text) = run_paired_trials(&rt, harrow_text, axum_text, "/echo", trials);
    println!("=== Text handler ===");
    analyze("Text", &h_text, &a_text);

    // --- JSON 1KB handler ---
    let harrow_json = rt.block_on(start_harrow_json());
    let axum_json = rt.block_on(start_axum_json());
    let (h_json, a_json) = run_paired_trials(&rt, harrow_json, axum_json, "/echo", trials);
    println!("=== JSON 1KB handler ===");
    analyze("JSON 1KB", &h_json, &a_json);

    // --- Simulated I/O handler ---
    let harrow_io = rt.block_on(start_harrow_io());
    let axum_io = rt.block_on(start_axum_io());
    let (h_io, a_io) = run_paired_trials(&rt, harrow_io, axum_io, "/echo", trials);
    println!("=== Simulated I/O handler ===");
    analyze("Sim I/O", &h_io, &a_io);
}

/// Run N paired trials, alternating Harrow/Axum to minimize bias.
/// Returns (harrow_ms[], axum_ms[]).
fn run_paired_trials(
    rt: &tokio::runtime::Runtime,
    harrow_addr: SocketAddr,
    axum_addr: SocketAddr,
    path: &str,
    trials: usize,
) -> (Vec<f64>, Vec<f64>) {
    // Warmup both
    for _ in 0..10 {
        rt.block_on(harrow_bench::run_concurrent(
            harrow_addr,
            path,
            CONCURRENCY,
            REQS_PER_CONN,
        ));
        rt.block_on(harrow_bench::run_concurrent(
            axum_addr,
            path,
            CONCURRENCY,
            REQS_PER_CONN,
        ));
    }

    let mut harrow_times = Vec::with_capacity(trials);
    let mut axum_times = Vec::with_capacity(trials);

    for i in 0..trials {
        // Alternate order to cancel out thermal/cache effects
        if i % 2 == 0 {
            harrow_times.push(measure_trial(rt, harrow_addr, path));
            axum_times.push(measure_trial(rt, axum_addr, path));
        } else {
            axum_times.push(measure_trial(rt, axum_addr, path));
            harrow_times.push(measure_trial(rt, harrow_addr, path));
        }

        if (i + 1) % 10 == 0 {
            eprint!("  {}/{trials} trials done\r", i + 1);
        }
    }
    eprintln!("  {trials}/{trials} trials done    ");

    (harrow_times, axum_times)
}

/// Single trial: run ROUNDS_PER_TRIAL rounds and return average ms.
fn measure_trial(rt: &tokio::runtime::Runtime, addr: SocketAddr, path: &str) -> f64 {
    let start = Instant::now();
    for _ in 0..ROUNDS_PER_TRIAL {
        rt.block_on(harrow_bench::run_concurrent(
            addr,
            path,
            CONCURRENCY,
            REQS_PER_CONN,
        ));
    }
    start.elapsed().as_secs_f64() * 1000.0 / ROUNDS_PER_TRIAL as f64
}

fn analyze(_label: &str, harrow: &[f64], axum: &[f64]) {
    let n = harrow.len() as f64;

    let h_mean = mean(harrow);
    let a_mean = mean(axum);
    let h_std = std_dev(harrow);
    let a_std = std_dev(axum);

    // Paired differences for paired t-test
    let diffs: Vec<f64> = harrow.iter().zip(axum.iter()).map(|(h, a)| h - a).collect();
    let d_mean = mean(&diffs);
    let d_std = std_dev(&diffs);

    // Paired t-statistic
    let t_stat = d_mean / (d_std / n.sqrt());
    // Two-tailed p-value approximation (using normal for n≥30)
    let p_value = 2.0 * normal_cdf(-t_stat.abs());

    // 95% CI for the mean difference
    let t_crit = 1.96; // approximate for large n
    let ci_low = d_mean - t_crit * d_std / n.sqrt();
    let ci_high = d_mean + t_crit * d_std / n.sqrt();

    // Effect size (Cohen's d for paired samples)
    let cohens_d = d_mean / d_std;

    // Required sample size for 80% power to detect observed effect
    let required_n = if cohens_d.abs() > 0.001 {
        let z_alpha = 1.96; // two-tailed α=0.05
        let z_beta = 0.842; // power=0.80
        ((z_alpha + z_beta) / cohens_d).powi(2).ceil() as usize
    } else {
        usize::MAX // effect too small
    };

    // Relative difference
    let rel_diff = (h_mean - a_mean) / a_mean * 100.0;

    println!("  Harrow:  {h_mean:.3} ± {h_std:.3} ms");
    println!("  Axum:    {a_mean:.3} ± {a_std:.3} ms");
    println!("  Diff:    {d_mean:+.3} ms ({rel_diff:+.2}%)");
    println!("  95% CI:  [{ci_low:+.3}, {ci_high:+.3}] ms");
    println!("  t={t_stat:.3}, p={p_value:.4}, Cohen's d={cohens_d:.3}");
    if required_n < 10000 {
        println!("  Required n for 80% power: {required_n} trials");
    } else {
        println!("  Required n for 80% power: >10000 (effect too small to detect)");
    }
    let sig = if p_value < 0.05 { "YES" } else { "no" };
    println!("  Significant at α=0.05? {sig}");
    println!();
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn std_dev(xs: &[f64]) -> f64 {
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0);
    var.sqrt()
}

/// Standard normal CDF approximation (Abramowitz & Stegun 26.2.17).
fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let d = 0.3989422804014327; // 1/sqrt(2π)
    let p = d * (-x * x / 2.0).exp();
    let poly = t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    if x >= 0.0 { 1.0 - p * poly } else { p * poly }
}

// ---------------------------------------------------------------------------
// Server setup
// ---------------------------------------------------------------------------

async fn start_harrow_text() -> SocketAddr {
    let app = harrow::App::new().get("/echo", harrow_bench::text_handler);
    harrow_bench::start_server(app).await
}

async fn start_harrow_json() -> SocketAddr {
    let app = harrow::App::new().get("/echo", harrow_bench::json_1kb_handler);
    harrow_bench::start_server(app).await
}

async fn start_harrow_io() -> SocketAddr {
    let app = harrow::App::new().get("/echo", harrow_bench::simulated_io_handler);
    harrow_bench::start_server(app).await
}

async fn axum_server(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
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

async fn start_axum_io() -> SocketAddr {
    use axum::{Json, Router, routing::get};
    use serde_json::Value;
    async fn handler() -> Json<Value> {
        tokio::time::sleep(std::time::Duration::from_micros(100)).await;
        Json(harrow_bench::JSON_1KB.clone())
    }
    axum_server(Router::new().route("/echo", get(handler))).await
}
