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
use std::convert::Infallible;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use axum::extract::Request as AxumRequest;
use axum::middleware::{self, Next as AxumNext};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::{Json, Router, routing::get};
use bytes::Bytes;
use http_body_util::Full;
use serde::{Deserialize, Serialize};
use tower::{Layer, Service, ServiceBuilder, ServiceExt, service_fn};

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

/// Measure allocations for a TCP benchmark using a real listener and keep-alive client.
fn measure_tcp<F, Fut>(setup: F, path: &str, expected_status: u16) -> AllocResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::net::SocketAddr>,
{
    // Build the runtime with tracking disabled so its allocations don't count.
    disable_tracking();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let addr = rt.block_on(setup());
    let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

    rt.block_on(async {
        for _ in 0..100 {
            let (status, _) = client.get(path).await;
            debug_assert_eq!(status, expected_status);
        }
    });

    reset_tracking();
    enable_tracking();
    rt.block_on(async {
        for _ in 0..ITERATIONS {
            let (status, _) = client.get(path).await;
            debug_assert_eq!(status, expected_status);
        }
    });
    disable_tracking();
    let (bytes, count) = snapshot();

    AllocResult {
        bytes_per_op: bytes / ITERATIONS,
        count_per_op: count / ITERATIONS,
    }
}

/// Measure allocations for an in-process async closure.
fn measure_async<F, Fut>(f: F) -> AllocResult
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    disable_tracking();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

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

/// Measure allocations for a TCP benchmark with custom headers on each request.
fn measure_tcp_with_headers<F, Fut>(
    setup: F,
    path: &str,
    headers: &[(&str, &str)],
    expected_status: u16,
) -> AllocResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::net::SocketAddr>,
{
    disable_tracking();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let addr = rt.block_on(setup());
    let mut client = rt.block_on(harrow_bench::BenchClient::connect(addr));

    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();

    rt.block_on(async {
        let h: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        for _ in 0..100 {
            let (status, _) = client.get_with_headers(path, &h).await;
            debug_assert_eq!(status, expected_status);
        }
    });

    reset_tracking();
    enable_tracking();
    rt.block_on(async {
        let h: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        for _ in 0..ITERATIONS {
            let (status, _) = client.get_with_headers(path, &h).await;
            debug_assert_eq!(status, expected_status);
        }
    });
    disable_tracking();
    let (bytes, count) = snapshot();

    AllocResult {
        bytes_per_op: bytes / ITERATIONS,
        count_per_op: count / ITERATIONS,
    }
}

fn harrow_request(path: &str) -> http::Request<harrow_core::request::Body> {
    let body = harrow_core::request::full_body(Full::new(Bytes::new()));
    http::Request::builder()
        .method(http::Method::GET)
        .uri(path)
        .body(body)
        .unwrap()
}

fn shared_state_from_app(app: harrow::App) -> Arc<harrow_core::dispatch::SharedState> {
    app.into_shared_state()
}

fn build_text_shared_state(depth: usize) -> Arc<harrow_core::dispatch::SharedState> {
    let mut app = harrow::App::new();
    for _ in 0..depth {
        app = app.middleware(harrow_bench::noop_middleware);
    }
    shared_state_from_app(app.get("/echo", harrow_bench::text_handler))
}

fn build_json_1kb_shared_state(depth: usize) -> Arc<harrow_core::dispatch::SharedState> {
    let mut app = harrow::App::new();
    for _ in 0..depth {
        app = app.middleware(harrow_bench::noop_middleware);
    }
    shared_state_from_app(app.get("/echo", harrow_bench::json_1kb_handler))
}

fn measure_dispatch(shared: Arc<harrow_core::dispatch::SharedState>, path: &str) -> AllocResult {
    measure_async(move || {
        let shared = Arc::clone(&shared);
        async move {
            let req = harrow_request(path);
            let resp = harrow_core::dispatch::dispatch(shared, req).await;
            std::hint::black_box(resp);
        }
    })
}

fn axum_request(path: &str) -> http::Request<axum::body::Body> {
    http::Request::builder()
        .method(http::Method::GET)
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn axum_noop(req: AxumRequest, next: AxumNext) -> AxumResponse {
    next.run(req).await
}

async fn axum_text_handler() -> &'static str {
    "ok"
}

async fn axum_json_1kb_handler() -> Json<serde_json::Value> {
    Json(harrow_bench::JSON_1KB.clone())
}

fn build_axum_text_router(depth: usize) -> Router {
    let mut router = Router::new().route("/echo", get(axum_text_handler));
    for _ in 0..depth {
        router = router.layer(middleware::from_fn(axum_noop));
    }
    router
}

fn build_axum_json_1kb_router(depth: usize) -> Router {
    let mut router = Router::new().route("/echo", get(axum_json_1kb_handler));
    for _ in 0..depth {
        router = router.layer(middleware::from_fn(axum_noop));
    }
    router
}

#[derive(Clone, Copy, Debug, Default)]
struct TowerNoopLayer;

#[derive(Clone, Debug)]
struct TowerNoopService<S> {
    inner: S,
}

impl<S> Layer<S> for TowerNoopLayer {
    type Service = TowerNoopService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TowerNoopService { inner }
    }
}

impl<S, Req> Service<Req> for TowerNoopService<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

async fn tower_text_handler(
    _req: http::Request<axum::body::Body>,
) -> Result<AxumResponse, Infallible> {
    Ok("ok".into_response())
}

async fn tower_json_1kb_handler(
    _req: http::Request<axum::body::Body>,
) -> Result<AxumResponse, Infallible> {
    Ok(Json(harrow_bench::JSON_1KB.clone()).into_response())
}

macro_rules! tower_stack {
    ($service:expr $(, $layer:expr )* $(,)?) => {{
        ServiceBuilder::new()
            $(.layer($layer))*
            .service($service)
    }};
}

fn measure_http_service<S>(service: S, path: &str) -> AllocResult
where
    S: Service<http::Request<axum::body::Body>, Response = AxumResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    measure_async(move || {
        let service = service.clone();
        async move {
            let req = axum_request(path);
            let resp = service.oneshot(req).await.unwrap();
            std::hint::black_box(resp);
        }
    })
}

fn measure_tower_text(depth: usize) -> AllocResult {
    match depth {
        0 => measure_http_service(tower_stack!(service_fn(tower_text_handler)), "/echo"),
        1 => measure_http_service(
            tower_stack!(service_fn(tower_text_handler), TowerNoopLayer),
            "/echo",
        ),
        2 => measure_http_service(
            tower_stack!(
                service_fn(tower_text_handler),
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        3 => measure_http_service(
            tower_stack!(
                service_fn(tower_text_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        5 => measure_http_service(
            tower_stack!(
                service_fn(tower_text_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        10 => measure_http_service(
            tower_stack!(
                service_fn(tower_text_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        _ => panic!("unsupported tower depth: {depth}"),
    }
}

fn measure_tower_json_1kb(depth: usize) -> AllocResult {
    match depth {
        0 => measure_http_service(tower_stack!(service_fn(tower_json_1kb_handler)), "/echo"),
        1 => measure_http_service(
            tower_stack!(service_fn(tower_json_1kb_handler), TowerNoopLayer),
            "/echo",
        ),
        2 => measure_http_service(
            tower_stack!(
                service_fn(tower_json_1kb_handler),
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        3 => measure_http_service(
            tower_stack!(
                service_fn(tower_json_1kb_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        5 => measure_http_service(
            tower_stack!(
                service_fn(tower_json_1kb_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        10 => measure_http_service(
            tower_stack!(
                service_fn(tower_json_1kb_handler),
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer,
                TowerNoopLayer
            ),
            "/echo",
        ),
        _ => panic!("unsupported tower depth: {depth}"),
    }
}

fn upsert_alloc_entry(
    benchmarks: &mut BTreeMap<String, BenchEntry>,
    name: &str,
    alloc: &AllocResult,
) -> bool {
    if let Some(entry) = benchmarks.get_mut(name) {
        entry.alloc_bytes = alloc.bytes_per_op;
        entry.alloc_count = alloc.count_per_op;
    } else {
        benchmarks.insert(
            name.to_string(),
            BenchEntry {
                criterion_path: format!("alloc_only/{name}"),
                description: format!("Allocation-only benchmark: {name}"),
                mean_ns: 0.0,
                median_ns: 0.0,
                alloc_bytes: alloc.bytes_per_op,
                alloc_count: alloc.count_per_op,
            },
        );
    }

    true
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

    // -- In-process dispatch benchmarks, matched to middleware_dispatch criterion --
    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("dispatch_text_{depth}");
        let r = measure_dispatch(build_text_shared_state(depth), "/echo");
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("dispatch_json_1kb_{depth}");
        let r = measure_dispatch(build_json_1kb_shared_state(depth), "/echo");
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("axum_from_fn_text_{depth}");
        let r = measure_http_service(build_axum_text_router(depth), "/echo");
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("axum_from_fn_json_1kb_{depth}");
        let r = measure_http_service(build_axum_json_1kb_router(depth), "/echo");
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("tower_noop_text_{depth}");
        let r = measure_tower_text(depth);
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    for depth in [0usize, 1, 2, 3, 5, 10] {
        let name = format!("tower_noop_json_1kb_{depth}");
        let r = measure_tower_json_1kb(depth);
        println!(
            "  {name}: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert(name, r);
    }

    // -- TCP benchmarks (real listener + BenchClient), matched to criterion --

    // echo_text
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/echo", harrow_bench::text_handler)).await
        },
        "/echo",
        200,
    );
    println!(
        "  echo_text: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_text".into(), r);

    // echo_json
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/echo", harrow_bench::json_handler)).await
        },
        "/echo",
        200,
    );
    println!(
        "  echo_json: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json".into(), r);

    // echo_json_1kb
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/echo", harrow_bench::json_1kb_handler))
                .await
        },
        "/echo",
        200,
    );
    println!(
        "  echo_json_1kb: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json_1kb".into(), r);

    // echo_json_10kb
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/echo", harrow_bench::json_10kb_handler))
                .await
        },
        "/echo",
        200,
    );
    println!(
        "  echo_json_10kb: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_json_10kb".into(), r);

    // echo_param
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/users/:id", harrow_bench::text_handler))
                .await
        },
        "/users/42",
        200,
    );
    println!(
        "  echo_param: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_param".into(), r);

    // echo_404
    let r = measure_tcp(
        || async {
            harrow_bench::start_server(App::new().get("/echo", harrow_bench::text_handler)).await
        },
        "/nope",
        404,
    );
    println!(
        "  echo_404: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("echo_404".into(), r);

    // full_json_3mw
    let r = measure_tcp(
        || async {
            let counter = std::sync::Arc::new(harrow_bench::HitCounter(
                std::sync::atomic::AtomicUsize::new(0),
            ));
            harrow_bench::start_server(
                App::new()
                    .state(counter)
                    .middleware(harrow_bench::timing_middleware)
                    .middleware(harrow_bench::header_middleware)
                    .middleware(harrow_bench::noop_middleware)
                    .get("/users/:id", harrow_bench::param_state_handler)
                    .get("/health", harrow_bench::text_handler),
            )
            .await
        },
        "/users/42",
        200,
    );
    println!(
        "  full_json_3mw: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("full_json_3mw".into(), r);

    // full_health_3mw
    let r = measure_tcp(
        || async {
            let counter = std::sync::Arc::new(harrow_bench::HitCounter(
                std::sync::atomic::AtomicUsize::new(0),
            ));
            harrow_bench::start_server(
                App::new()
                    .state(counter)
                    .middleware(harrow_bench::timing_middleware)
                    .middleware(harrow_bench::header_middleware)
                    .middleware(harrow_bench::noop_middleware)
                    .get("/users/:id", harrow_bench::param_state_handler)
                    .get("/health", harrow_bench::text_handler),
            )
            .await
        },
        "/health",
        200,
    );
    println!(
        "  full_health_3mw: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("full_health_3mw".into(), r);

    // mw_depth_10
    let r = measure_tcp(
        || async {
            let mut app = App::new();
            for _ in 0..10 {
                app = app.middleware(harrow_bench::noop_middleware);
            }
            harrow_bench::start_server(app.get("/echo", harrow_bench::text_handler)).await
        },
        "/echo",
        200,
    );
    println!(
        "  mw_depth_10: {} bytes, {} allocs per op",
        r.bytes_per_op, r.count_per_op
    );
    results.insert("mw_depth_10".into(), r);

    // -- Session middleware benchmarks (TCP) --
    println!("\nSession middleware allocations:");

    {
        let bench_cookie = harrow_bench::bench_session_cookie();

        // session_new: no cookie, handler sets data
        let r = measure_tcp(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .get("/echo", harrow_bench::session_set_handler),
                )
                .await
            },
            "/echo",
            200,
        );
        println!(
            "  session_new: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("session_new".into(), r);

        // session_noop: no cookie, session inserted but never modified
        let r = measure_tcp(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .get("/echo", harrow_bench::session_noop_handler),
                )
                .await
            },
            "/echo",
            200,
        );
        println!(
            "  session_noop: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("session_noop".into(), r);

        // session_existing_read: valid cookie, handler reads only
        let cookie_for_read = bench_cookie.clone();
        let r = measure_tcp_with_headers(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                harrow_bench::seed_bench_session(&store).await;
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .get("/echo", harrow_bench::session_get_handler),
                )
                .await
            },
            "/echo",
            &[("cookie", &cookie_for_read)],
            200,
        );
        println!(
            "  session_existing_read: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("session_existing_read".into(), r);

        // session_existing_write: valid cookie, handler mutates
        let cookie_for_write = bench_cookie.clone();
        let r = measure_tcp_with_headers(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                harrow_bench::seed_bench_session(&store).await;
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .get("/echo", harrow_bench::session_write_handler),
                )
                .await
            },
            "/echo",
            &[("cookie", &cookie_for_write)],
            200,
        );
        println!(
            "  session_existing_write: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("session_existing_write".into(), r);

        // session_read_plus_noop: valid cookie, read-only plus one extra middleware layer
        let cookie_for_noop = bench_cookie.clone();
        let r = measure_tcp_with_headers(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                harrow_bench::seed_bench_session(&store).await;
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .middleware(harrow_bench::noop_middleware)
                        .get("/echo", harrow_bench::session_get_handler),
                )
                .await
            },
            "/echo",
            &[("cookie", &cookie_for_noop)],
            200,
        );
        println!(
            "  session_read_plus_noop: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("session_read_plus_noop".into(), r);

        // realistic_stack_baseline: 1KB text body and realistic headers, but no middleware
        let r = measure_tcp_with_headers(
            || async {
                harrow_bench::start_server(
                    App::new().get("/echo", harrow_bench::large_text_handler),
                )
                .await
            },
            "/echo",
            &[
                ("accept-encoding", "gzip"),
                ("origin", "https://bench.example.com"),
            ],
            200,
        );
        println!(
            "  realistic_stack_baseline: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("realistic_stack_baseline".into(), r);

        // realistic_stack_read: session + cors + compression with a valid cookie
        let cookie_for_stack_read = bench_cookie.clone();
        let r = measure_tcp_with_headers(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                harrow_bench::seed_bench_session(&store).await;
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                        .middleware(harrow::compression_middleware)
                        .get("/echo", harrow_bench::session_large_get_handler),
                )
                .await
            },
            "/echo",
            &[
                ("cookie", &cookie_for_stack_read),
                ("accept-encoding", "gzip"),
                ("origin", "https://bench.example.com"),
            ],
            200,
        );
        println!(
            "  realistic_stack_read: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("realistic_stack_read".into(), r);

        // realistic_stack_write: session + cors + compression with a valid cookie, write path
        let cookie_for_stack_write = bench_cookie.clone();
        let r = measure_tcp_with_headers(
            || async {
                let store = harrow_bench::InMemorySessionStore::new();
                harrow_bench::seed_bench_session(&store).await;
                let config = harrow_bench::bench_session_config();
                harrow_bench::start_server(
                    App::new()
                        .middleware(harrow::session_middleware(store, config))
                        .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                        .middleware(harrow::compression_middleware)
                        .get("/echo", harrow_bench::session_large_write_handler),
                )
                .await
            },
            "/echo",
            &[
                ("cookie", &cookie_for_stack_write),
                ("accept-encoding", "gzip"),
                ("origin", "https://bench.example.com"),
            ],
            200,
        );
        println!(
            "  realistic_stack_write: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        results.insert("realistic_stack_write".into(), r);
    }

    // -- Axum benchmarks (TCP, to match criterion setup) --
    println!("\nAxum allocations:");

    let mut axum_results: BTreeMap<String, AllocResult> = BTreeMap::new();

    // Axum echo_text
    {
        use axum::{Router, routing::get};

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/echo", get(|| async { "ok" }));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/echo",
            200,
        );
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

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/echo", get(json_handler));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/echo",
            200,
        );
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

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/echo", get(json_1kb_handler));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/echo",
            200,
        );
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

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/echo", get(json_10kb_handler));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/echo",
            200,
        );
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

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/users/{id}", get(param_handler));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/users/42",
            200,
        );
        println!(
            "  echo_param: {} bytes, {} allocs per op",
            r.bytes_per_op, r.count_per_op
        );
        axum_results.insert("echo_param".into(), r);
    }

    // Axum echo_404
    {
        use axum::{Router, routing::get};

        let r = measure_tcp(
            || async {
                let app = Router::new().route("/echo", get(|| async { "ok" }));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                addr
            },
            "/nope",
            404,
        );
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
        written += upsert_alloc_entry(&mut baseline.benchmarks, name.as_str(), alloc) as u32;
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
