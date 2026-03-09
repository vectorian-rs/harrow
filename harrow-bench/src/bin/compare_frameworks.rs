//! Harrow vs Axum load-test comparison runner.
//!
//! Builds both servers, runs bench against each across multiple scenarios
//! and concurrency levels, produces a markdown report and SVG charts.
//!
//! Usage:
//!   compare-frameworks --bench-bin /path/to/bench
//!   compare-frameworks --remote --server-host 10.0.1.5 --bench-bin /path/to/bench
//!   compare-frameworks --bench-bin /path/to/bench --duration 30

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const HARROW_PORT: u16 = 3090;
const AXUM_PORT: u16 = 3091;

const ENDPOINTS: &[(&str, &str)] = &[
    ("/", "root"),
    ("/greet/bench", "greet_bench"),
    ("/health", "health"),
    ("/nonexistent", "404_miss"),
];

const CONCURRENCY_LEVELS: &[u32] = &[1, 8, 32, 128];

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

struct Args {
    bench_bin: PathBuf,
    remote: bool,
    server_host: String,
    bind: Option<String>,
    duration: u32,
    warmup: u32,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut bench_bin: Option<PathBuf> = std::env::var("BENCH_BIN").ok().map(PathBuf::from);
    let mut remote = false;
    let mut server_host = std::env::var("SERVER_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut bind: Option<String> = None;
    let mut duration: u32 = 60;
    let mut warmup: u32 = 5;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bench-bin" => {
                bench_bin = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--remote" => {
                remote = true;
                i += 1;
            }
            "--server-host" => {
                server_host = args[i + 1].clone();
                i += 2;
            }
            "--bind" => {
                bind = Some(args[i + 1].clone());
                i += 2;
            }
            "--duration" => {
                duration = args[i + 1].parse().expect("invalid --duration");
                i += 2;
            }
            "--warmup" => {
                warmup = args[i + 1].parse().expect("invalid --warmup");
                i += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!(
                    "usage: compare-frameworks --bench-bin PATH [--remote] \
                     [--server-host HOST] [--bind ADDR] [--duration SECS] [--warmup SECS]"
                );
                std::process::exit(1);
            }
        }
    }

    // Auto-discover bench binary
    if bench_bin.is_none() {
        let exe = std::env::current_exe().unwrap_or_default();
        let repo_root = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .unwrap_or(Path::new("."));
        let candidates = [
            repo_root.join("../mcp-servers/target/release/bench"),
            repo_root.join("../mcp-load-tester/target/release/bench"),
        ];
        for c in &candidates {
            if c.exists() {
                bench_bin = Some(c.canonicalize().unwrap_or_else(|_| c.clone()));
                break;
            }
        }
    }

    let bench_bin = bench_bin.unwrap_or_else(|| {
        eprintln!("error: bench binary not found. Use --bench-bin or set BENCH_BIN.");
        std::process::exit(1);
    });

    if !bench_bin.exists() {
        eprintln!("error: bench binary not found at {}", bench_bin.display());
        std::process::exit(1);
    }

    Args {
        bench_bin,
        remote,
        server_host,
        bind,
        duration,
        warmup,
    }
}

// ---------------------------------------------------------------------------
// Server management
// ---------------------------------------------------------------------------

fn wait_for_server(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("server on {addr} did not start within {timeout:?}"))
}

fn start_server(binary: &str, port: u16, bind: Option<&str>) -> Child {
    let mut cmd = Command::new(binary);
    cmd.arg("--port").arg(port.to_string());
    if let Some(addr) = bind {
        cmd.arg("--bind").arg(addr);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn()
        .unwrap_or_else(|e| panic!("failed to start {binary}: {e}"))
}

fn kill_server(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

fn run_bench(
    bench_bin: &Path,
    url: &str,
    concurrency: u32,
    duration: u32,
    warmup: u32,
) -> Option<Value> {
    let output = Command::new(bench_bin)
        .args([
            "-u",
            url,
            "-M",
            "-c",
            &concurrency.to_string(),
            "-d",
            &duration.to_string(),
            "-w",
            &warmup.to_string(),
            "-j",
            "-q",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).ok(),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("    bench failed (exit {}): {}", o.status, stderr.trim());
            None
        }
        Err(e) => {
            eprintln!("    failed to run bench: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

fn generate_report(results: &BTreeMap<String, Value>, outdir: &Path, duration: u32, warmup: u32) {
    let now = chrono_lite_utc();
    let mut report = format!(
        "# Harrow vs Axum — Framework Comparison\n\
         \n\
         **Generated:** {now}\n\
         **Duration:** {duration} seconds per test, {warmup} seconds warmup\n\
         **Tool:** mcp-load-tester bench (max-throughput mode)\n\
         **Target requests:** ~2M ({duration}s x high concurrency)\n\
         \n\
         ---\n"
    );

    for &(path, name) in ENDPOINTS {
        report.push_str(&format!("\n## Endpoint: `{path}`\n\n"));
        report.push_str(
            "| Concurrency | Framework | Req/s | p50 (ms) | p99 (ms) | p999 (ms) | Errors |\n",
        );
        report.push_str(
            "|-------------|-----------|-------|----------|----------|-----------|--------|\n",
        );

        for &c in CONCURRENCY_LEVELS {
            for fw in ["harrow", "axum"] {
                let key = format!("{fw}_{name}_c{c}");
                let (rps, p50, p99, p999, errors) = match results.get(&key) {
                    Some(v) => (
                        val_str(v, "rps"),
                        val_str(v, "latency_p50_ms"),
                        val_str(v, "latency_p99_ms"),
                        val_str(v, "latency_p999_ms"),
                        val_str(v, "failed_requests"),
                    ),
                    None => (s("N/A"), s("N/A"), s("N/A"), s("N/A"), s("N/A")),
                };
                report.push_str(&format!(
                    "| {c} | {fw} | {rps} | {p50} | {p99} | {p999} | {errors} |\n"
                ));
            }
        }
    }

    report.push_str("\n---\n\n*Raw JSON results are in `target/comparison/`.*\n");

    let report_path = outdir.join("comparison-report.md");
    fs::write(&report_path, &report).unwrap();
    println!("Report written to: {}", report_path.display());
}

fn val_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Number(n)) => {
            if let Some(f) = n.as_f64() {
                if f == f.floor() && f.abs() < 1e15 {
                    format!("{}", f as i64)
                } else {
                    format!("{f:.3}")
                }
            } else {
                n.to_string()
            }
        }
        Some(v) => v.to_string(),
        None => "N/A".into(),
    }
}

fn s(v: &str) -> String {
    v.into()
}

/// Minimal UTC timestamp without pulling in chrono.
fn chrono_lite_utc() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args = parse_args();

    println!("Using bench binary: {}", args.bench_bin.display());

    // Build
    if !args.remote {
        println!("Building both servers in release mode...");
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "--bin",
                "harrow-server",
                "--bin",
                "axum-server",
            ])
            .status()
            .expect("failed to run cargo build");
        if !status.success() {
            eprintln!("cargo build failed");
            std::process::exit(1);
        }
    }

    let outdir = PathBuf::from("target/comparison");
    fs::create_dir_all(&outdir).unwrap();

    let total = ENDPOINTS.len() * CONCURRENCY_LEVELS.len() * 2;
    let mut current = 0usize;
    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    let mode = if args.remote {
        format!("remote (servers on {})", args.server_host)
    } else {
        "local".into()
    };

    println!();
    println!("Starting framework comparison...");
    println!("  Mode: {mode}");
    println!(
        "  Duration: {}s per test, {}s warmup",
        args.duration, args.warmup
    );
    println!("  Concurrency: {CONCURRENCY_LEVELS:?}");
    println!(
        "  Endpoints: {:?}",
        ENDPOINTS.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    println!();

    let frameworks: &[(&str, u16, &str)] = &[
        ("harrow", HARROW_PORT, "target/release/harrow-server"),
        ("axum", AXUM_PORT, "target/release/axum-server"),
    ];

    for &(path, name) in ENDPOINTS {
        for &conc in CONCURRENCY_LEVELS {
            println!("--- Endpoint: {path}, Concurrency: {conc} ---");

            for &(fw, port, binary) in frameworks {
                let mut server: Option<Child> = None;

                if !args.remote {
                    let child = start_server(binary, port, args.bind.as_deref());
                    if let Err(e) =
                        wait_for_server(&args.server_host, port, Duration::from_secs(10))
                    {
                        eprintln!("  {e}");
                        kill_server(child);
                        continue;
                    }
                    server = Some(child);
                }

                current += 1;
                let key = format!("{fw}_{name}_c{conc}");
                let url = format!("http://{}:{port}{path}", args.server_host);
                println!("  [{current}/{total}] Bench {fw}: {path} c={conc}");

                let data = run_bench(&args.bench_bin, &url, conc, args.duration, args.warmup);

                if let Some(ref v) = data {
                    let json_path = outdir.join(format!("{key}.json"));
                    let pretty = serde_json::to_string_pretty(v).unwrap();
                    fs::write(&json_path, &pretty).unwrap();
                    results.insert(key, v.clone());
                } else {
                    results.insert(key, Value::Object(Default::default()));
                }

                if let Some(child) = server {
                    kill_server(child);
                    thread::sleep(Duration::from_millis(300));
                }
            }

            println!();
        }
    }

    // Report
    generate_report(&results, &outdir, args.duration, args.warmup);
    println!("Raw JSON results in: {}", outdir.display());

    // SVG charts
    let histogram_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("generate-histogram")));

    if let Some(bin) = histogram_bin.filter(|p| p.exists()) {
        println!();
        println!("Generating SVG comparison charts...");
        let status = Command::new(&bin).arg(outdir.to_str().unwrap()).status();
        match status {
            Ok(s) if s.success() => println!("SVG charts written to {}/", outdir.display()),
            _ => eprintln!("generate-histogram failed"),
        }
    } else {
        println!(
            "Note: generate-histogram binary not found, skipping SVG generation. \
             Build it with: cargo build --release --bin generate-histogram"
        );
    }
}
