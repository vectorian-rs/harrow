use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::Method;

use harrow_core::path::PathPattern;
use harrow_core::route::{Route, RouteMetadata, RouteTable};

async fn dummy(_req: harrow_core::Request) -> harrow_core::Response {
    harrow_core::Response::text("ok")
}

fn make_route(method: Method, pattern: &str) -> Route {
    Route {
        method,
        pattern: PathPattern::parse(pattern),
        handler: harrow_core::handler::wrap(dummy),
        metadata: RouteMetadata::default(),
        middleware: Vec::new(),
    }
}

/// Translate harrow pattern syntax to matchit syntax: `:id` → `{id}`, `*path` → `{*path}`
fn to_matchit_pattern(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':') {
                format!("{{{name}}}")
            } else if let Some(name) = seg.strip_prefix('*') {
                format!("{{*{name}}}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Generate `n` decoy routes plus a target param route at the end.
fn route_patterns(n: usize) -> Vec<String> {
    let mut patterns = Vec::with_capacity(n);
    for i in 0..n.saturating_sub(1) {
        patterns.push(format!("/decoy-{i}"));
    }
    patterns.push("/target/:id".to_string());
    patterns
}

// ---------------------------------------------------------------------------
// Trie vs matchit: param hit, exact hit, 404 miss
// ---------------------------------------------------------------------------

fn bench_trie_vs_matchit(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_vs_matchit");

    for n in [1, 10, 50, 100, 200, 500] {
        let patterns = route_patterns(n);

        // Build harrow RouteTable
        let mut table = RouteTable::new();
        for p in &patterns {
            table.push(make_route(Method::GET, p));
        }

        // Build matchit router
        let mut matchit_router = matchit::Router::new();
        for (i, p) in patterns.iter().enumerate() {
            matchit_router.insert(to_matchit_pattern(p), i).unwrap();
        }

        // Param hit (target is last route)
        group.bench_with_input(BenchmarkId::new("harrow_param_hit", n), &n, |b, _| {
            b.iter(|| {
                table.match_route_idx(
                    std::hint::black_box(&Method::GET),
                    std::hint::black_box("/target/42"),
                )
            })
        });

        group.bench_with_input(BenchmarkId::new("matchit_param_hit", n), &n, |b, _| {
            b.iter(|| matchit_router.at(std::hint::black_box("/target/42")))
        });

        // Exact hit (first decoy, unless n=1 then target is only route)
        let exact_path = if n > 1 { "/decoy-0" } else { "/target/42" };
        group.bench_with_input(BenchmarkId::new("harrow_exact_hit", n), &n, |b, _| {
            b.iter(|| {
                table.match_route_idx(
                    std::hint::black_box(&Method::GET),
                    std::hint::black_box(exact_path),
                )
            })
        });

        group.bench_with_input(BenchmarkId::new("matchit_exact_hit", n), &n, |b, _| {
            b.iter(|| matchit_router.at(std::hint::black_box(exact_path)))
        });

        // 404 miss
        group.bench_with_input(BenchmarkId::new("harrow_404", n), &n, |b, _| {
            b.iter(|| {
                table.match_route_idx(
                    std::hint::black_box(&Method::GET),
                    std::hint::black_box("/nonexistent/path"),
                )
            })
        });

        group.bench_with_input(BenchmarkId::new("matchit_404", n), &n, |b, _| {
            b.iter(|| matchit_router.at(std::hint::black_box("/nonexistent/path")))
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Trie vs matchit: glob patterns
// ---------------------------------------------------------------------------

fn bench_trie_vs_matchit_glob(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_vs_matchit_glob");

    let mut table = RouteTable::new();
    table.push(make_route(Method::GET, "/static/*path"));
    table.push(make_route(Method::GET, "/api/users/:id"));

    let mut matchit_router = matchit::Router::new();
    matchit_router.insert("/static/{*path}", 0).unwrap();
    matchit_router.insert("/api/users/{id}", 1).unwrap();

    group.bench_function("harrow_glob_hit", |b| {
        b.iter(|| {
            table.match_route_idx(
                std::hint::black_box(&Method::GET),
                std::hint::black_box("/static/css/app.min.css"),
            )
        })
    });

    group.bench_function("matchit_glob_hit", |b| {
        b.iter(|| matchit_router.at(std::hint::black_box("/static/css/app.min.css")))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// any_route_matches_path (harrow-only, no matchit equivalent)
// ---------------------------------------------------------------------------

fn bench_any_route_matches_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("any_route_matches_path");

    for n in [10, 100, 500] {
        let patterns = route_patterns(n);
        let mut table = RouteTable::new();
        for p in &patterns {
            table.push(make_route(Method::GET, p));
        }

        group.bench_with_input(BenchmarkId::new("hit", n), &n, |b, _| {
            b.iter(|| table.any_route_matches_path(std::hint::black_box("/target/42")))
        });

        group.bench_with_input(BenchmarkId::new("miss", n), &n, |b, _| {
            b.iter(|| table.any_route_matches_path(std::hint::black_box("/nonexistent")))
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_trie_vs_matchit,
    bench_trie_vs_matchit_glob,
    bench_any_route_matches_path
);
criterion_main!(benches);
