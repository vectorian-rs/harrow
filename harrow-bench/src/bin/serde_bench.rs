//! Serde benchmark orchestrator.
//!
//! Runs on the CLIENT node, orchestrating Docker containers on the SERVER via
//! SSH, running mcp-load-tester benchmarks locally, and generating a markdown
//! summary.
//!
//! Three-phase bench run:
//!   Phase A — Serialization comparison (harrow bare vs axum bare)
//!   Phase B — Per-feature middleware overhead (harrow only)
//!   Phase C — O11y overhead (harrow only, with Vector)
//!
//! Usage:
//!   serde-bench --server-host IP --client-host IP [OPTIONS]
//!   serde-bench --server-host 10.0.1.5 --client-host 10.0.1.6 --duration 30

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_PORT: u16 = 3090;
const SLEEP_BETWEEN: Duration = Duration::from_secs(2);

/// Phase A: serialization comparison endpoints (bare prefix, both frameworks).
const PHASE_A_ENDPOINTS: &[(&str, &str)] = &[
    ("bare/text", "text"),
    ("bare/json/1kb", "json_1kb"),
    ("bare/msgpack/1kb", "msgpack_1kb"),
];

const PHASE_A_CONCURRENCIES: &[u32] = &[1, 8, 32, 128];

/// Phase B: per-feature middleware overhead (harrow only).
const PHASE_B_PREFIXES: &[&str] = &[
    "bare",
    "timeout",
    "request-id",
    "cors",
    "compression",
    "full",
];

const PHASE_B_PAYLOADS: &[(&str, &str)] = &[
    ("text", "text"),
    ("json/1kb", "json_1kb"),
    ("msgpack/1kb", "msgpack_1kb"),
];

const PHASE_B_CONCURRENCIES: &[u32] = &[1, 32, 128];

/// Phase C: o11y overhead endpoints (same payloads as B, harrow only).
const PHASE_C_ENDPOINTS: &[(&str, &str)] = &[
    ("text", "text"),
    ("json/1kb", "json_1kb"),
    ("msgpack/1kb", "msgpack_1kb"),
];

const PHASE_C_CONCURRENCIES: &[u32] = &[1, 32, 128];

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

struct Args {
    server_host: String,
    client_host: String,
    server_user: String,
    port: u16,
    bench_bin: PathBuf,
    duration: u32,
    warmup: u32,
    results_dir: PathBuf,
}

fn usage() -> ! {
    eprintln!(
        "Usage: serde-bench --server-host IP --client-host IP [OPTIONS]\n\
         \n\
         Three-phase benchmark suite:\n\
         \x20 Phase A — Serialization comparison (harrow bare vs axum bare)\n\
         \x20 Phase B — Per-feature middleware overhead (harrow only)\n\
         \x20 Phase C — O11y overhead (harrow only, with Vector)\n\
         \n\
         Options:\n\
         \x20 --server-host IP      Server node IP (required)\n\
         \x20 --client-host IP      Client node private IP (required for Phase C)\n\
         \x20 --server-user USER    SSH user on server (default: alpine)\n\
         \x20 --port PORT           Server port (default: 3090)\n\
         \x20 --bench-bin PATH      Path to mcp-load-tester bench binary (auto-discovered)\n\
         \x20 --duration SECS       Test duration per run (default: 60)\n\
         \x20 --warmup SECS         Warmup duration per run (default: 5)\n\
         \x20 --results-dir DIR     Output directory (default: results)"
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut server_host: Option<String> = None;
    let mut client_host: Option<String> = None;
    let mut server_user = "alpine".to_string();
    let mut port: u16 = DEFAULT_PORT;
    let mut bench_bin: Option<PathBuf> = std::env::var("BENCH_BIN").ok().map(PathBuf::from);
    let mut duration: u32 = 60;
    let mut warmup: u32 = 5;
    let mut results_dir = PathBuf::from("results");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--server-host" => {
                server_host = Some(args[i + 1].clone());
                i += 2;
            }
            "--client-host" => {
                client_host = Some(args[i + 1].clone());
                i += 2;
            }
            "--server-user" => {
                server_user = args[i + 1].clone();
                i += 2;
            }
            "--port" => {
                port = args[i + 1].parse().expect("invalid --port");
                i += 2;
            }
            "--bench-bin" => {
                bench_bin = Some(PathBuf::from(&args[i + 1]));
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
            "--results-dir" => {
                results_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown option: {other}");
                usage();
            }
        }
    }

    let server_host = server_host.unwrap_or_else(|| {
        eprintln!("error: --server-host is required");
        usage();
    });
    let client_host = client_host.unwrap_or_else(|| {
        eprintln!("error: --client-host is required");
        usage();
    });

    // Auto-discover bench binary
    if bench_bin.is_none() {
        if let Ok(p) = which("bench") {
            bench_bin = Some(p);
        } else {
            let home = std::env::var("HOME").unwrap_or_default();
            let candidate = PathBuf::from(format!("{home}/mcp-load-tester/target/release/bench"));
            if candidate.exists() {
                bench_bin = Some(candidate);
            }
        }
    }

    let bench_bin = bench_bin.unwrap_or_else(|| {
        eprintln!(
            "error: bench binary not found. Use --bench-bin, set BENCH_BIN, \
             or ensure 'bench' is in PATH / ~/mcp-load-tester/target/release/"
        );
        std::process::exit(1);
    });

    if !bench_bin.exists() {
        eprintln!("error: bench binary not found at {}", bench_bin.display());
        std::process::exit(1);
    }

    Args {
        server_host,
        client_host,
        server_user,
        port,
        bench_bin,
        duration,
        warmup,
        results_dir,
    }
}

/// Minimal `which` — checks PATH for an executable.
fn which(name: &str) -> Result<PathBuf, ()> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(())
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

fn ssh_cmd(user: &str, host: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg(format!("{user}@{host}"));
    cmd
}

fn ssh_server(args: &Args, remote_cmd: &str) -> std::io::Result<std::process::Output> {
    ssh_cmd(&args.server_user, &args.server_host)
        .arg(remote_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

// ---------------------------------------------------------------------------
// Container management (on server, via SSH)
// ---------------------------------------------------------------------------

fn start_container(args: &Args, name: &str, image: &str, extra_args: &str) {
    println!(">>> Starting container: {name}");
    // Remove any existing container
    let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
    let docker_cmd = if extra_args.is_empty() {
        format!("docker run -d --name {name} --network host {image}")
    } else {
        format!("docker run -d --name {name} --network host {extra_args} {image}")
    };
    let out = ssh_server(args, &docker_cmd);
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("  warning: docker run {name} stderr: {}", stderr.trim());
        }
        Err(e) => eprintln!("  failed to start container {name}: {e}"),
    }
    thread::sleep(Duration::from_secs(2));
}

fn stop_container(args: &Args, name: &str) {
    println!(">>> Stopping container: {name}");
    let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
}

// ---------------------------------------------------------------------------
// Local Docker (Vector on client)
// ---------------------------------------------------------------------------

fn docker_local(cmd_args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("docker")
        .args(cmd_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn start_vector() {
    println!("--- Starting Vector (client-side, blackhole sink) ---");
    let _ = docker_local(&["rm", "-f", "vector"]);
    let home = std::env::var("HOME").unwrap_or_default();
    let config = format!("{home}/vector.toml:/etc/vector/vector.toml:ro");
    let out = docker_local(&[
        "run",
        "-d",
        "--name",
        "vector",
        "--network",
        "host",
        "-v",
        &config,
        "timberio/vector:latest-alpine",
    ]);
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("  warning: vector start stderr: {}", stderr.trim());
        }
        Err(e) => eprintln!("  failed to start vector: {e}"),
    }
}

fn stop_vector() {
    println!("--- Stopping Vector ---");
    let _ = docker_local(&["rm", "-f", "vector"]);
}

// ---------------------------------------------------------------------------
// Health / readiness checks
// ---------------------------------------------------------------------------

fn wait_for_server(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200)).is_ok() {
            println!("    Health check passed");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("server on {addr} did not start within {timeout:?}"))
}

// ---------------------------------------------------------------------------
// Stats / logs collection
// ---------------------------------------------------------------------------

fn collect_docker_stats(args: &Args, label: &str) {
    let remote_cmd =
        "docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}'";
    if let Ok(out) = ssh_server(args, remote_cmd) {
        let path = args.results_dir.join(format!("stats_{label}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

fn collect_docker_logs(args: &Args, container: &str, label: &str) {
    let remote_cmd = format!("docker logs {container} 2>&1");
    if let Ok(out) = ssh_server(args, &remote_cmd) {
        let path = args.results_dir.join(format!("logs_{label}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
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
    outfile: &Path,
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
        Ok(o) if o.status.success() => {
            let _ = fs::write(outfile, &o.stdout);
            let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
            if let Some(ref v) = val {
                let rps = val_str(v, "rps");
                let p99 = val_str(v, "latency_p99_ms");
                println!("    → rps={rps} p99={p99}ms");
            }
            val
        }
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

fn generate_report(results: &BTreeMap<String, Value>, args: &Args) {
    let now = chrono_lite_utc();
    let mut report = format!(
        "# Serde Benchmark Results\n\
         \n\
         Server: {}:{}\n\
         Duration: {}s | Warmup: {}s\n\
         Date: {now}\n",
        args.server_host, args.port, args.duration, args.warmup,
    );

    // Phase A
    report.push_str("\n## Phase A: Serialization Comparison (harrow bare vs axum bare)\n\n");
    report.push_str(
        "| Framework | Endpoint | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |\n",
    );
    report.push_str(
        "|-----------|----------|-------------|-----|----------|----------|----------|\n",
    );

    for fw in ["harrow", "axum"] {
        for &(path, label) in PHASE_A_ENDPOINTS {
            for &c in PHASE_A_CONCURRENCIES {
                let key = format!("a_{fw}_{label}_c{c}");
                let (rps, p50, p99, p999) = extract_latencies(results.get(&key));
                report.push_str(&format!(
                    "| {fw} | /{path} | {c} | {rps} | {p50} | {p99} | {p999} |\n"
                ));
            }
        }
    }

    // Phase B
    report.push_str("\n## Phase B: Per-Feature Middleware Overhead (harrow only)\n\n");
    report.push_str(
        "| Feature | Payload | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |\n",
    );
    report
        .push_str("|---------|---------|-------------|-----|----------|----------|----------|\n");

    for &prefix in PHASE_B_PREFIXES {
        for &(payload, label) in PHASE_B_PAYLOADS {
            for &c in PHASE_B_CONCURRENCIES {
                let key = format!("b_{prefix}_{label}_c{c}");
                let (rps, p50, p99, p999) = extract_latencies(results.get(&key));
                report.push_str(&format!(
                    "| {prefix} | /{payload} | {c} | {rps} | {p50} | {p99} | {p999} |\n"
                ));
            }
        }
    }

    // Phase C
    report.push_str("\n## Phase C: O11y Overhead (harrow only, with Vector)\n\n");
    report
        .push_str("| Endpoint | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |\n");
    report.push_str("|----------|-------------|-----|----------|----------|----------|\n");

    for &(path, label) in PHASE_C_ENDPOINTS {
        for &c in PHASE_C_CONCURRENCIES {
            let key = format!("c_o11y_{label}_c{c}");
            let (rps, p50, p99, p999) = extract_latencies(results.get(&key));
            report.push_str(&format!(
                "| /{path} | {c} | {rps} | {p50} | {p99} | {p999} |\n"
            ));
        }
    }

    let report_path = args.results_dir.join("summary.md");
    fs::write(&report_path, &report).unwrap();
    println!("Summary written to {}", report_path.display());
}

fn extract_latencies(v: Option<&Value>) -> (String, String, String, String) {
    match v {
        Some(v) => (
            val_str(v, "rps"),
            val_str(v, "latency_p50_ms"),
            val_str(v, "latency_p99_ms"),
            val_str(v, "latency_p999_ms"),
        ),
        None => ("-".into(), "-".into(), "-".into(), "-".into()),
    }
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
        None => "-".into(),
    }
}

/// Minimal UTC timestamp without pulling in chrono.
fn chrono_lite_utc() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S UTC"])
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
    fs::create_dir_all(&args.results_dir).unwrap();

    println!("============================================");
    println!(" Serde Benchmark Suite (3-phase)");
    println!(" Server: {}:{}", args.server_host, args.port);
    println!(" Duration: {}s  Warmup: {}s", args.duration, args.warmup);
    println!(" Bench binary: {}", args.bench_bin.display());
    println!(" Results: {}/", args.results_dir.display());
    println!("============================================");
    println!();

    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    // -----------------------------------------------------------------------
    // Phase A: Serialization comparison (harrow bare vs axum bare)
    // -----------------------------------------------------------------------
    println!("========== PHASE A: Serialization comparison ==========");

    // --- Harrow (bare group, no middleware) ---
    println!();
    println!("--- Harrow (bare) ---");
    start_container(&args, "serde-bench-server", "serde-bench-server", "");
    if let Err(e) = wait_for_server(&args.server_host, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_container(&args, "serde-bench-server");
        std::process::exit(1);
    }

    for &(path, label) in PHASE_A_ENDPOINTS {
        for &c in PHASE_A_CONCURRENCIES {
            let url = format!("http://{}:{}/{path}", args.server_host, args.port);
            let key = format!("a_harrow_{label}_c{c}");
            let outfile = args.results_dir.join(format!("{key}.json"));
            println!("  [{key}] c={c} → {url}");
            if let Some(v) =
                run_bench(&args.bench_bin, &url, c, args.duration, args.warmup, &outfile)
            {
                results.insert(key, v);
            }
            thread::sleep(SLEEP_BETWEEN);
        }
    }

    collect_docker_stats(&args, "harrow_bare");
    collect_docker_logs(&args, "serde-bench-server", "harrow_bare");
    stop_container(&args, "serde-bench-server");

    // --- Axum (bare group, no middleware) ---
    println!();
    println!("--- Axum (bare) ---");
    start_container(&args, "axum-serde-server", "axum-serde-server", "");
    if let Err(e) = wait_for_server(&args.server_host, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_container(&args, "axum-serde-server");
        std::process::exit(1);
    }

    for &(path, label) in PHASE_A_ENDPOINTS {
        for &c in PHASE_A_CONCURRENCIES {
            let url = format!("http://{}:{}/{path}", args.server_host, args.port);
            let key = format!("a_axum_{label}_c{c}");
            let outfile = args.results_dir.join(format!("{key}.json"));
            println!("  [{key}] c={c} → {url}");
            if let Some(v) =
                run_bench(&args.bench_bin, &url, c, args.duration, args.warmup, &outfile)
            {
                results.insert(key, v);
            }
            thread::sleep(SLEEP_BETWEEN);
        }
    }

    collect_docker_stats(&args, "axum_bare");
    collect_docker_logs(&args, "axum-serde-server", "axum_bare");
    stop_container(&args, "axum-serde-server");

    // -----------------------------------------------------------------------
    // Phase B: Per-feature middleware overhead (harrow only)
    // -----------------------------------------------------------------------
    println!();
    println!("========== PHASE B: Per-feature middleware overhead ==========");

    start_container(&args, "serde-bench-server", "serde-bench-server", "");
    if let Err(e) = wait_for_server(&args.server_host, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_container(&args, "serde-bench-server");
        std::process::exit(1);
    }

    for &prefix in PHASE_B_PREFIXES {
        println!();
        println!("--- {prefix} ---");
        for &(payload, label) in PHASE_B_PAYLOADS {
            for &c in PHASE_B_CONCURRENCIES {
                let url = format!(
                    "http://{}:{}/{prefix}/{payload}",
                    args.server_host, args.port
                );
                let key = format!("b_{prefix}_{label}_c{c}");
                let outfile = args.results_dir.join(format!("{key}.json"));
                println!("  [{key}] c={c} → {url}");
                if let Some(v) =
                    run_bench(&args.bench_bin, &url, c, args.duration, args.warmup, &outfile)
                {
                    results.insert(key, v);
                }
                thread::sleep(SLEEP_BETWEEN);
            }
        }
    }

    collect_docker_stats(&args, "harrow_middleware");
    collect_docker_logs(&args, "serde-bench-server", "harrow_middleware");
    stop_container(&args, "serde-bench-server");

    // -----------------------------------------------------------------------
    // Phase C: O11y overhead (harrow only, with Vector)
    // -----------------------------------------------------------------------
    println!();
    println!("========== PHASE C: O11y overhead (Harrow) ==========");

    start_vector();

    println!("  Waiting for Vector to be ready...");
    if let Err(e) = wait_for_server("127.0.0.1", 4318, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_vector();
        std::process::exit(1);
    }

    let o11y_extra = format!(
        "-e OTLP_ENDPOINT=http://{}:4318 -- /serde-bench-server --bind 0.0.0.0 --o11y",
        args.client_host,
    );
    start_container(&args, "serde-bench-o11y", "serde-bench-server", &o11y_extra);
    if let Err(e) = wait_for_server(&args.server_host, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_container(&args, "serde-bench-o11y");
        stop_vector();
        std::process::exit(1);
    }

    for &(path, label) in PHASE_C_ENDPOINTS {
        for &c in PHASE_C_CONCURRENCIES {
            let url = format!("http://{}:{}/{path}", args.server_host, args.port);
            let key = format!("c_o11y_{label}_c{c}");
            let outfile = args.results_dir.join(format!("{key}.json"));
            println!("  [{key}] c={c} → {url}");
            if let Some(v) =
                run_bench(&args.bench_bin, &url, c, args.duration, args.warmup, &outfile)
            {
                results.insert(key, v);
            }
            thread::sleep(SLEEP_BETWEEN);
        }
    }

    collect_docker_stats(&args, "harrow_o11y");
    collect_docker_logs(&args, "serde-bench-o11y", "harrow_o11y");
    stop_container(&args, "serde-bench-o11y");
    stop_vector();

    // -----------------------------------------------------------------------
    // Summary
    // -----------------------------------------------------------------------
    println!();
    println!("========== GENERATING SUMMARY ==========");
    generate_report(&results, &args);
    println!();
    println!("Done! Results in {}/", args.results_dir.display());
}
