//! Profiling binary for session middleware scenarios.
//!
//! Usage:
//!   cargo run --release --bin profile-session -- read
//!   cargo flamegraph --bin profile-session -- stack-write
//!   cargo run --release --bin profile-session -- read 64 10 100

use std::net::SocketAddr;
use std::time::Instant;

use harrow::App;

const DEFAULT_CONCURRENCY: usize = 64;
const DEFAULT_REQS_PER_CONN: usize = 10;
const DEFAULT_ROUNDS: usize = 100;
const WARMUP_ROUNDS: usize = 5;

type HeaderList = Vec<(String, String)>;

fn parse_args() -> (String, usize, usize, usize) {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().unwrap_or_else(|| "read".to_string());
    let concurrency = args
        .next()
        .map(|s| s.parse().expect("invalid concurrency"))
        .unwrap_or(DEFAULT_CONCURRENCY);
    let reqs_per_conn = args
        .next()
        .map(|s| s.parse().expect("invalid reqs_per_conn"))
        .unwrap_or(DEFAULT_REQS_PER_CONN);
    let rounds = args
        .next()
        .map(|s| s.parse().expect("invalid rounds"))
        .unwrap_or(DEFAULT_ROUNDS);
    (scenario, concurrency, reqs_per_conn, rounds)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let (scenario, concurrency, reqs_per_conn, rounds) = parse_args();
    let (addr, headers) = start_scenario(&scenario).await;

    println!(
        "scenario={scenario} addr={addr} concurrency={concurrency} reqs_per_conn={reqs_per_conn} rounds={rounds}"
    );

    for _ in 0..WARMUP_ROUNDS {
        run_once(addr, &headers, concurrency, reqs_per_conn).await;
    }

    let start = Instant::now();
    for _ in 0..rounds {
        run_once(addr, &headers, concurrency, reqs_per_conn).await;
    }

    let elapsed = start.elapsed();
    println!(
        "elapsed_ms_per_round={:.3}",
        elapsed.as_secs_f64() * 1000.0 / rounds as f64
    );
}

async fn run_once(
    addr: SocketAddr,
    headers: &HeaderList,
    concurrency: usize,
    reqs_per_conn: usize,
) {
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    harrow_bench::run_concurrent_with_headers(addr, "/echo", &refs, concurrency, reqs_per_conn)
        .await;
}

async fn start_scenario(name: &str) -> (SocketAddr, HeaderList) {
    let cookie = harrow_bench::bench_session_cookie();

    match name {
        "baseline" => {
            let addr =
                harrow_bench::start_server(App::new().get("/echo", harrow_bench::text_handler))
                    .await;
            (addr, vec![])
        }
        "no-touch" => {
            let store = harrow::InMemorySessionStore::new();
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", harrow_bench::session_noop_handler);
            (harrow_bench::start_server(app).await, vec![])
        }
        "new" => {
            let store = harrow::InMemorySessionStore::new();
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", harrow_bench::session_set_handler);
            (harrow_bench::start_server(app).await, vec![])
        }
        "read" => {
            let store = harrow::InMemorySessionStore::new();
            harrow_bench::seed_bench_session(&store).await;
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", harrow_bench::session_get_handler);
            (
                harrow_bench::start_server(app).await,
                vec![("cookie".to_string(), cookie)],
            )
        }
        "write" => {
            let store = harrow::InMemorySessionStore::new();
            harrow_bench::seed_bench_session(&store).await;
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", harrow_bench::session_write_handler);
            (
                harrow_bench::start_server(app).await,
                vec![("cookie".to_string(), cookie)],
            )
        }
        "stack-read" => {
            let store = harrow::InMemorySessionStore::new();
            harrow_bench::seed_bench_session(&store).await;
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .middleware(harrow::compression_middleware)
                .get("/echo", harrow_bench::session_large_get_handler);
            (
                harrow_bench::start_server(app).await,
                vec![
                    ("cookie".to_string(), cookie),
                    ("accept-encoding".to_string(), "gzip".to_string()),
                    (
                        "origin".to_string(),
                        "https://bench.example.com".to_string(),
                    ),
                ],
            )
        }
        "stack-write" => {
            let store = harrow::InMemorySessionStore::new();
            harrow_bench::seed_bench_session(&store).await;
            let config = harrow_bench::bench_session_config();
            let app = App::new()
                .middleware(harrow::session_middleware(store, config))
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .middleware(harrow::compression_middleware)
                .get("/echo", harrow_bench::session_large_write_handler);
            (
                harrow_bench::start_server(app).await,
                vec![
                    ("cookie".to_string(), cookie),
                    ("accept-encoding".to_string(), "gzip".to_string()),
                    (
                        "origin".to_string(),
                        "https://bench.example.com".to_string(),
                    ),
                ],
            )
        }
        other => panic!(
            "unknown scenario: {other}. expected one of baseline|no-touch|new|read|write|stack-read|stack-write"
        ),
    }
}
