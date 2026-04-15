use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::sync::Mutex;

use harrow::App;
use harrow_bench::{
    BenchClient, bench_session_config, bench_session_cookie, large_text_handler, noop_middleware,
    seed_bench_session, session_get_handler, session_large_get_handler,
    session_large_write_handler, session_noop_handler, session_set_handler, session_write_handler,
    start_server, text_handler,
};

const STACK_ACCEPT_ENCODING: &str = "gzip";
const STACK_ORIGIN: &str = "https://bench.example.com";

fn stack_headers(cookie: &str) -> [(&'static str, &str); 3] {
    [
        ("cookie", cookie),
        ("accept-encoding", STACK_ACCEPT_ENCODING),
        ("origin", STACK_ORIGIN),
    ]
}

fn bench_session(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("session");

    let cookie = bench_session_cookie();

    // baseline: text handler, no middleware
    let baseline_addr = rt.block_on(async {
        let app = || App::new().get("/echo", text_handler);
        start_server(app).await
    });

    // session middleware active but handler does not access the session (pure middleware overhead)
    let noop_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", session_noop_handler)
        };
        start_server(app).await
    });

    // session new: handler sets data, no inbound cookie → getrandom + blake3 + DashMap insert
    let new_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", session_set_handler)
        };
        start_server(app).await
    });

    // session read: valid cookie, handler reads only → blake3 verify + DashMap read, no Set-Cookie
    let read_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        seed_bench_session(&store).await;
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", session_get_handler)
        };
        start_server(app).await
    });

    // session write: valid cookie, handler mutates → blake3 verify + DashMap read + write + Set-Cookie
    let write_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        seed_bench_session(&store).await;
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .get("/echo", session_write_handler)
        };
        start_server(app).await
    });

    // session + noop middleware
    let stack_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        seed_bench_session(&store).await;
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .middleware(noop_middleware)
                .get("/echo", session_get_handler)
        };
        start_server(app).await
    });

    // realistic stack baseline: larger body and realistic headers, but no middleware
    let realistic_baseline_addr = rt.block_on(async {
        let app = || App::new().get("/echo", large_text_handler);
        start_server(app).await
    });

    // realistic stack: session + cors + compression, read-only
    let realistic_read_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        seed_bench_session(&store).await;
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .middleware(harrow::compression_middleware)
                .get("/echo", session_large_get_handler)
        };
        start_server(app).await
    });

    // realistic stack: session + cors + compression, write path
    let realistic_write_addr = rt.block_on(async {
        let store = harrow_bench::InMemorySessionStore::new();
        seed_bench_session(&store).await;
        let config = bench_session_config();
        let app = move || {
            App::new()
                .middleware(harrow::session_middleware(store, config))
                .middleware(harrow::cors_middleware(harrow::CorsConfig::default()))
                .middleware(harrow::compression_middleware)
                .get("/echo", session_large_write_handler)
        };
        start_server(app).await
    });

    let baseline_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(baseline_addr))));
    group.bench_function("baseline_0mw", |b| {
        let client = Arc::clone(&baseline_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let noop_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(noop_addr))));
    group.bench_function("session_noop", |b| {
        let client = Arc::clone(&noop_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let new_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(new_addr))));
    group.bench_function("session_new", |b| {
        let client = Arc::clone(&new_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client.lock().await.get("/echo").await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let read_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(read_addr))));
    group.bench_function("session_existing_read", |b| {
        let client = Arc::clone(&read_client);
        let cookie = cookie.clone();
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            let cookie = cookie.clone();
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("cookie", &cookie)])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let write_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(write_addr))));
    group.bench_function("session_existing_write", |b| {
        let client = Arc::clone(&write_client);
        let cookie = cookie.clone();
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            let cookie = cookie.clone();
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("cookie", &cookie)])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let stack_client = Arc::new(Mutex::new(rt.block_on(BenchClient::connect(stack_addr))));
    group.bench_function("session_read_plus_noop", |b| {
        let client = Arc::clone(&stack_client);
        let cookie = cookie.clone();
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            let cookie = cookie.clone();
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &[("cookie", &cookie)])
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let realistic_baseline_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(realistic_baseline_addr)),
    ));
    group.bench_function("realistic_stack_baseline", |b| {
        let client = Arc::clone(&realistic_baseline_client);
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            async move {
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers(
                        "/echo",
                        &[
                            ("accept-encoding", STACK_ACCEPT_ENCODING),
                            ("origin", STACK_ORIGIN),
                        ],
                    )
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let realistic_read_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(realistic_read_addr)),
    ));
    group.bench_function("realistic_stack_read", |b| {
        let client = Arc::clone(&realistic_read_client);
        let cookie = cookie.clone();
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            let cookie = cookie.clone();
            async move {
                let headers = stack_headers(&cookie);
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &headers)
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    let realistic_write_client = Arc::new(Mutex::new(
        rt.block_on(BenchClient::connect(realistic_write_addr)),
    ));
    group.bench_function("realistic_stack_write", |b| {
        let client = Arc::clone(&realistic_write_client);
        let cookie = cookie.clone();
        b.to_async(&rt).iter(|| {
            let client = Arc::clone(&client);
            let cookie = cookie.clone();
            async move {
                let headers = stack_headers(&cookie);
                let (status, _) = client
                    .lock()
                    .await
                    .get_with_headers("/echo", &headers)
                    .await;
                debug_assert_eq!(status, 200);
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_session);
criterion_main!(benches);
