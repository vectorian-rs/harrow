use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use harrow::App;
use harrow_bench::{BenchClient, run_concurrent, text_handler};
use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::box_incoming;

const CLIENT_WORKERS: usize = 4;
const SERVER_WORKERS: usize = 4;
const CONCURRENCY_LEVELS: &[usize] = &[32, 128];
const KEEP_ALIVE_REQS_PER_CONN: usize = 10;
const NEW_CONN_REQS_PER_TASK: usize = 10;
const MAX_CONNECTIONS: usize = 8192;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
struct HarrowVariant {
    name: &'static str,
    use_timers: bool,
    use_semaphore: bool,
    use_joinset: bool,
}

const HARROW_VARIANTS: &[HarrowVariant] = &[
    HarrowVariant {
        name: "harrow/full",
        use_timers: true,
        use_semaphore: true,
        use_joinset: true,
    },
    HarrowVariant {
        name: "harrow/no_timers",
        use_timers: false,
        use_semaphore: true,
        use_joinset: true,
    },
    HarrowVariant {
        name: "harrow/no_semaphore",
        use_timers: true,
        use_semaphore: false,
        use_joinset: true,
    },
    HarrowVariant {
        name: "harrow/no_joinset",
        use_timers: true,
        use_semaphore: true,
        use_joinset: false,
    },
    HarrowVariant {
        name: "harrow/minimal",
        use_timers: false,
        use_semaphore: false,
        use_joinset: false,
    },
];

async fn axum_text_handler() -> &'static str {
    "ok"
}

async fn start_axum_server(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(STARTUP_DELAY).await;
    addr
}

async fn start_harrow_variant_server(app: App, variant: HarrowVariant) -> SocketAddr {
    let shared = app.into_shared_state();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(run_harrow_variant_listener(listener, shared, variant));
    tokio::time::sleep(STARTUP_DELAY).await;
    addr
}

async fn run_harrow_variant_listener(
    listener: TcpListener,
    shared: Arc<SharedState>,
    variant: HarrowVariant,
) {
    let semaphore = if variant.use_semaphore {
        Some(Arc::new(Semaphore::new(MAX_CONNECTIONS)))
    } else {
        None
    };
    let mut connections = if variant.use_joinset {
        Some(JoinSet::<()>::new())
    } else {
        None
    };

    loop {
        if let Some(connections) = connections.as_mut() {
            while connections.try_join_next().is_some() {}
        }

        let (stream, _remote) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let permit = if let Some(semaphore) = semaphore.as_ref() {
            match semaphore.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    drop(stream);
                    continue;
                }
            }
        } else {
            None
        };

        let shared = Arc::clone(&shared);
        let connection_task = async move {
            let _permit = permit;
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: http::Request<Incoming>| {
                let shared = Arc::clone(&shared);
                async move {
                    let boxed = req.map(box_incoming);
                    Ok::<_, std::convert::Infallible>(dispatch(shared, boxed).await)
                }
            });

            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            if variant.use_timers {
                builder
                    .http1()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT);
            }
            let conn = builder.serve_connection(io, service);

            if variant.use_timers {
                let _ = tokio::time::timeout(CONNECTION_TIMEOUT, conn).await;
            } else {
                let _ = conn.await;
            }
        };

        if let Some(connections) = connections.as_mut() {
            connections.spawn(connection_task);
        } else {
            tokio::spawn(connection_task);
        }
    }
}

async fn run_concurrent_new_conn(
    addr: SocketAddr,
    path: &str,
    concurrency: usize,
    reqs_per_task: usize,
) {
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let path = path.to_string();
        handles.push(tokio::spawn(async move {
            for _ in 0..reqs_per_task {
                let mut client = BenchClient::connect(addr).await;
                let (status, _) = client.get(&path).await;
                debug_assert_eq!(status, 200);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

fn bench_server_variants(c: &mut Criterion) {
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(CLIENT_WORKERS)
        .enable_all()
        .build()
        .unwrap();
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(SERVER_WORKERS)
        .enable_all()
        .build()
        .unwrap();

    let harrow_servers: Vec<(HarrowVariant, SocketAddr)> = HARROW_VARIANTS
        .iter()
        .copied()
        .map(|variant| {
            let addr = server_rt.block_on(async {
                let app = App::new().get("/text", text_handler);
                start_harrow_variant_server(app, variant).await
            });
            (variant, addr)
        })
        .collect();
    let axum_addr = server_rt.block_on(async {
        let app = Router::new().route("/text", get(axum_text_handler));
        start_axum_server(app).await
    });

    let mut keep_alive = c.benchmark_group("server_variants/keep_alive");
    keep_alive.sample_size(10);
    keep_alive.warm_up_time(Duration::from_secs(1));
    keep_alive.measurement_time(Duration::from_secs(3));

    for &conc in CONCURRENCY_LEVELS {
        for &(variant, addr) in &harrow_servers {
            keep_alive.bench_with_input(BenchmarkId::new(variant.name, conc), &conc, |b, &conc| {
                b.to_async(&client_rt)
                    .iter(|| run_concurrent(addr, "/text", conc, KEEP_ALIVE_REQS_PER_CONN))
            });
        }

        keep_alive.bench_with_input(
            BenchmarkId::new("axum/baseline", conc),
            &conc,
            |b, &conc| {
                b.to_async(&client_rt)
                    .iter(|| run_concurrent(axum_addr, "/text", conc, KEEP_ALIVE_REQS_PER_CONN))
            },
        );
    }
    keep_alive.finish();

    let mut new_conn = c.benchmark_group("server_variants/new_conn");
    new_conn.sample_size(10);
    new_conn.warm_up_time(Duration::from_secs(1));
    new_conn.measurement_time(Duration::from_secs(3));

    for &conc in CONCURRENCY_LEVELS {
        for &(variant, addr) in &harrow_servers {
            new_conn.bench_with_input(BenchmarkId::new(variant.name, conc), &conc, |b, &conc| {
                b.to_async(&client_rt)
                    .iter(|| run_concurrent_new_conn(addr, "/text", conc, NEW_CONN_REQS_PER_TASK))
            });
        }

        new_conn.bench_with_input(
            BenchmarkId::new("axum/baseline", conc),
            &conc,
            |b, &conc| {
                b.to_async(&client_rt).iter(|| {
                    run_concurrent_new_conn(axum_addr, "/text", conc, NEW_CONN_REQS_PER_TASK)
                })
            },
        );
    }
    new_conn.finish();

    drop(server_rt);
}

criterion_group!(benches, bench_server_variants);
criterion_main!(benches);
