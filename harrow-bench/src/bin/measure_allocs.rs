//! Measure per-operation allocations and update `benches/baseline.toml`.
//!
//! Usage:
//!   cargo run --release --bin measure-allocs
//!
//! Uses a custom `TrackingAllocator` that wraps the system allocator with
//! atomic counters. Each operation is run N times, then per-op averages are
//! computed and written into `alloc_bytes` / `alloc_count` in baseline.toml.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tracking allocator
// ---------------------------------------------------------------------------

struct TrackingAllocator;

static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static TRACKING_ENABLED: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if TRACKING_ENABLED.load(Ordering::Relaxed) != 0 && !ptr.is_null() {
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

fn reset_tracking() {
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    ALLOC_COUNT.store(0, Ordering::Relaxed);
}

fn enable_tracking() {
    TRACKING_ENABLED.store(1, Ordering::Relaxed);
}

fn disable_tracking() {
    TRACKING_ENABLED.store(0, Ordering::Relaxed);
}

fn snapshot() -> (u64, u64) {
    (
        ALLOC_BYTES.load(Ordering::Relaxed),
        ALLOC_COUNT.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// TOML data model (same as update_baseline)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    metadata: Metadata,
    benchmarks: BTreeMap<String, BenchEntry>,
    axum_benchmarks: BTreeMap<String, BenchEntry>,
    traffic_weights: BTreeMap<String, f64>,
    resource_budget: ResourceBudget,
}

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    version: String,
    date: String,
    platform: String,
    cpu: String,
    rust_version: String,
    notes: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchEntry {
    criterion_path: String,
    description: String,
    mean_ns: f64,
    median_ns: f64,
    alloc_bytes: u64,
    alloc_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResourceBudget {
    target_ops_per_sec: u64,
    cpu_budget_percent: f64,
    memory_budget_mb: f64,
    weighted_mean_ns: f64,
    total_cpu_percent: f64,
    verdict: String,
}

// ---------------------------------------------------------------------------
// Allocation measurement helpers
// ---------------------------------------------------------------------------

const ITERATIONS: u64 = 10_000;

struct AllocResult {
    bytes_per_op: u64,
    count_per_op: u64,
}

/// Measure allocations for a synchronous closure.
fn measure_sync<F: Fn()>(f: F) -> AllocResult {
    // Warmup
    for _ in 0..100 {
        f();
    }

    reset_tracking();
    enable_tracking();
    for _ in 0..ITERATIONS {
        f();
    }
    disable_tracking();
    let (bytes, count) = snapshot();

    AllocResult {
        bytes_per_op: bytes / ITERATIONS,
        count_per_op: count / ITERATIONS,
    }
}

/// Measure allocations for an async closure using a single-threaded runtime.
fn measure_async<F, Fut>(f: F) -> AllocResult
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // Build the runtime with tracking disabled so its allocations don't count.
    disable_tracking();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Warmup
    rt.block_on(async {
        for _ in 0..100 {
            f().await;
        }
    });

    reset_tracking();
    enable_tracking();
    rt.block_on(async {
        for _ in 0..ITERATIONS {
            f().await;
        }
    });
    disable_tracking();
    let (bytes, count) = snapshot();

    AllocResult {
        bytes_per_op: bytes / ITERATIONS,
        count_per_op: count / ITERATIONS,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    use harrow::App;
    use harrow_core::path::PathPattern;
    use http::Method;

    println!("Measuring per-operation allocations ({ITERATIONS} iterations each)...\n");

    let mut results: BTreeMap<String, AllocResult> = BTreeMap::new();

    // -- Micro benchmarks (sync) --

    // path_match_exact_hit
    let exact = PathPattern::parse("/health");
    let r = measure_sync(|| {
        let _ = std::hint::black_box(exact.match_path(std::hint::black_box("/health")));
    });
    println!(
        "  path_match_exact_hit: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("path_match_exact_hit".into(), r);

    // path_match_1_param
    let one_param = PathPattern::parse("/users/:id");
    let r = measure_sync(|| {
        let _ = std::hint::black_box(one_param.match_path(std::hint::black_box("/users/42")));
    });
    println!(
        "  path_match_1_param: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("path_match_1_param".into(), r);

    // path_match_glob
    let glob = PathPattern::parse("/files/*path");
    let r = measure_sync(|| {
        let _ = std::hint::black_box(glob.match_path(std::hint::black_box("/files/a/b/c/d.txt")));
    });
    println!(
        "  path_match_glob: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("path_match_glob".into(), r);

    // route_lookup_100
    let app = harrow_bench::build_app_with_routes(100);
    let table = app.route_table();
    let r = measure_sync(|| {
        let _ = std::hint::black_box(table.match_route_idx(
            std::hint::black_box(&Method::GET),
            std::hint::black_box("/target/42"),
        ));
    });
    println!(
        "  route_lookup_100: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("route_lookup_100".into(), r);

    // -- Client-based benchmarks (async, no TCP) --

    // echo_text
    let client = App::new().get("/echo", harrow_bench::text_handler).client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/echo").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  echo_text: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_text".into(), r);

    // echo_json
    let client = App::new().get("/echo", harrow_bench::json_handler).client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/echo").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  echo_json: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json".into(), r);

    // echo_json_1kb
    let client = App::new()
        .get("/echo", harrow_bench::json_1kb_handler)
        .client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/echo").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  echo_json_1kb: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json_1kb".into(), r);

    // echo_json_10kb
    let client = App::new()
        .get("/echo", harrow_bench::json_10kb_handler)
        .client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/echo").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  echo_json_10kb: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json_10kb".into(), r);

    // echo_param
    let client = App::new()
        .get("/users/:id", harrow_bench::text_handler)
        .client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/users/42").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  echo_param: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_param".into(), r);

    // echo_404
    let client = App::new().get("/echo", harrow_bench::text_handler).client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/nope").await;
            debug_assert_eq!(resp.status(), http::StatusCode::NOT_FOUND);
        }
    });
    println!(
        "  echo_404: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_404".into(), r);

    // full_json_3mw
    let counter = std::sync::Arc::new(harrow_bench::HitCounter(
        std::sync::atomic::AtomicUsize::new(0),
    ));
    let client = App::new()
        .state(counter)
        .middleware(harrow_bench::timing_middleware)
        .middleware(harrow_bench::header_middleware)
        .middleware(harrow_bench::noop_middleware)
        .get("/users/:id", harrow_bench::param_state_handler)
        .get("/health", harrow_bench::text_handler)
        .client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/users/42").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  full_json_3mw: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("full_json_3mw".into(), r);

    // full_health_3mw (same server, /health route)
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/health").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  full_health_3mw: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("full_health_3mw".into(), r);

    // mw_depth_10
    let mut app = App::new();
    for _ in 0..10 {
        app = app.middleware(harrow_bench::noop_middleware);
    }
    let client = app.get("/echo", harrow_bench::text_handler).client();
    let r = measure_async(|| {
        let client = &client;
        async move {
            let resp = client.get("/echo").await;
            debug_assert_eq!(resp.status(), http::StatusCode::OK);
        }
    });
    println!(
        "  mw_depth_10: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("mw_depth_10".into(), r);

    // -- Axum benchmarks (TCP, to match criterion setup) --
    println!("\nAxum allocations:");

    let mut axum_results: BTreeMap<String, AllocResult> = BTreeMap::new();

    // Axum echo_text
    {
        use axum::{Router, routing::get};

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(|| async { "ok" }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        // Warmup
        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/echo").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/echo").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_text: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_text".into(), r);
    }

    // Axum echo_json
    {
        use axum::{Json, Router, routing::get};
        use serde_json::{Value, json};

        async fn json_handler() -> Json<Value> {
            Json(json!({"status": "ok", "code": 200}))
        }

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/echo").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/echo").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_json: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_json".into(), r);
    }

    // Axum echo_json_1kb
    {
        use axum::{Json, Router, routing::get};
        use serde_json::Value;

        async fn json_1kb_handler() -> Json<Value> {
            Json(harrow_bench::JSON_1KB.clone())
        }

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_1kb_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/echo").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/echo").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_json_1kb: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_json_1kb".into(), r);
    }

    // Axum echo_json_10kb
    {
        use axum::{Json, Router, routing::get};
        use serde_json::Value;

        async fn json_10kb_handler() -> Json<Value> {
            Json(harrow_bench::JSON_10KB.clone())
        }

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(json_10kb_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/echo").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/echo").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_json_10kb: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_json_10kb".into(), r);
    }

    // Axum echo_param
    {
        use axum::{Router, extract::Path, routing::get};

        async fn param_handler(Path(_id): Path<String>) -> &'static str {
            "ok"
        }

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/users/{id}", get(param_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/users/42").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/users/42").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_param: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_param".into(), r);
    }

    // Axum echo_404
    {
        use axum::{Router, routing::get};

        disable_tracking();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let addr = rt.block_on(async {
            let app = Router::new().route("/echo", get(|| async { "ok" }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            addr
        });

        let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

        rt.block_on(async {
            for _ in 0..100 {
                let _ = client.get("/nope").await;
            }
        });

        reset_tracking();
        enable_tracking();
        rt.block_on(async {
            for _ in 0..ITERATIONS {
                let _ = client.get("/nope").await;
            }
        });
        disable_tracking();
        let (bytes, count) = snapshot();
        let r = AllocResult {
            bytes_per_op: bytes / ITERATIONS,
            count_per_op: count / ITERATIONS,
        };
        println!(
            "  echo_404: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_404".into(), r);
    }

    // -- Update baseline.toml --
    println!();
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let toml_path = Path::new(manifest_dir).join("benches/baseline.toml");

    let toml_text = match fs::read_to_string(&toml_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("warning: cannot read {}: {e}", toml_path.display());
            eprintln!("Alloc results printed above but not written to TOML.");
            return;
        }
    };

    let mut baseline: Baseline = match toml::from_str(&toml_text) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("warning: cannot parse TOML: {e}");
            return;
        }
    };

    let mut written = 0u32;
    for (name, alloc) in &results {
        if let Some(entry) = baseline.benchmarks.get_mut(name.as_str()) {
            entry.alloc_bytes = alloc.bytes_per_op;
            entry.alloc_count = alloc.count_per_op;
            written += 1;
        }
    }

    for (name, alloc) in &axum_results {
        if let Some(entry) = baseline.axum_benchmarks.get_mut(name.as_str()) {
            entry.alloc_bytes = alloc.bytes_per_op;
            entry.alloc_count = alloc.count_per_op;
            written += 1;
        }
    }

    let output = toml::to_string_pretty(&baseline).unwrap();
    fs::write(&toml_path, &output).unwrap();
    println!("Updated {written} alloc entries in {}", toml_path.display());
}
