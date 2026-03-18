//! Remote performance test orchestrator.
//!
//! Runs from the laptop and drives both EC2 nodes via SSH:
//! - server node runs `harrow-perf-server` or `axum-perf-server` in Docker
//! - client node runs `spinr` either in Docker or directly on the host
//! - optional host telemetry and `perf stat` are collected per run
//!
//! The runner is intentionally small and explicit: each invocation selects one
//! or more named test cases and runs Harrow/Axum back-to-back for each case.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_PORT: u16 = 3090;
const SSH_USER: &str = "alpine";
const DEFAULT_SPINR_BIN: &str = "/usr/local/bin/spinr";
const SLEEP_BETWEEN_RUNS: Duration = Duration::from_secs(2);
const MONITOR_MARGIN_SECS: u32 = 2;
const PERF_COUNTERS: &str = "cycles,instructions,branches,branch-misses,cache-references,cache-misses,context-switches,cpu-migrations,page-faults";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestCase {
    name: &'static str,
    path: &'static str,
    key_label: &'static str,
    concurrency: u32,
}

const TEST_CASES: &[TestCase] = &[
    TestCase {
        name: "text-c32",
        path: "text",
        key_label: "text",
        concurrency: 32,
    },
    TestCase {
        name: "text-c128",
        path: "text",
        key_label: "text",
        concurrency: 128,
    },
    TestCase {
        name: "json-1kb-c32",
        path: "json/1kb",
        key_label: "json_1kb",
        concurrency: 32,
    },
    TestCase {
        name: "json-1kb-c128",
        path: "json/1kb",
        key_label: "json_1kb",
        concurrency: 128,
    },
    TestCase {
        name: "json-10kb-c32",
        path: "json/10kb",
        key_label: "json_10kb",
        concurrency: 32,
    },
    TestCase {
        name: "json-10kb-c128",
        path: "json/10kb",
        key_label: "json_10kb",
        concurrency: 128,
    },
    TestCase {
        name: "msgpack-1kb-c32",
        path: "msgpack/1kb",
        key_label: "msgpack_1kb",
        concurrency: 32,
    },
    TestCase {
        name: "msgpack-1kb-c128",
        path: "msgpack/1kb",
        key_label: "msgpack_1kb",
        concurrency: 128,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpinrMode {
    Docker,
    Host,
}

impl SpinrMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "docker" => Some(Self::Docker),
            "host" => Some(Self::Host),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Host => "host",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteSide {
    Server,
    Client,
}

impl RemoteSide {
    fn label(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Framework {
    Harrow,
    Axum,
}

impl Framework {
    fn as_str(self) -> &'static str {
        match self {
            Self::Harrow => "harrow",
            Self::Axum => "axum",
        }
    }

    fn image(self) -> &'static str {
        match self {
            Self::Harrow => "harrow-perf-server",
            Self::Axum => "axum-perf-server",
        }
    }

    fn container_name(self) -> &'static str {
        match self {
            Self::Harrow => "harrow-perf-server",
            Self::Axum => "axum-perf-server",
        }
    }

    fn launch_cmd(self, port: u16) -> String {
        match self {
            Self::Harrow => format!("/harrow-perf-server --bind 0.0.0.0 --port {port}"),
            Self::Axum => format!("/axum-perf-server --bind 0.0.0.0 --port {port}"),
        }
    }
}

struct Args {
    server_ssh: String,
    client_ssh: String,
    server_private: String,
    ssh_user: String,
    instance_type: String,
    port: u16,
    duration: u32,
    warmup: u32,
    results_dir: std::path::PathBuf,
    test_cases: Vec<TestCase>,
    os_monitors: bool,
    perf_stat: bool,
    spinr_mode: SpinrMode,
}

fn client_perf_enabled(args: &Args) -> bool {
    args.perf_stat && args.spinr_mode == SpinrMode::Host
}

fn perf_scope_label(args: &Args) -> &'static str {
    if !args.perf_stat {
        "off"
    } else if client_perf_enabled(args) {
        "server + client"
    } else {
        "server only"
    }
}

fn find_test_case(name: &str) -> Option<TestCase> {
    TEST_CASES.iter().copied().find(|case| case.name == name)
}

fn supported_test_case_names() -> String {
    TEST_CASES
        .iter()
        .map(|case| case.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_supported_test_cases_and_exit() -> ! {
    println!("Supported test cases:");
    for case in TEST_CASES {
        println!(
            "  {:<18} /{} @ c={}",
            case.name, case.path, case.concurrency
        );
    }
    std::process::exit(0);
}

fn usage() -> ! {
    eprintln!(
        "Usage: harrow-remote-perf-test --server-ssh IP --client-ssh IP --server-private IP --instance-type TYPE --test-case NAME [OPTIONS]\n\
         \n\
         Runs matched Harrow/Axum perf-server comparisons on two EC2 nodes.\n\
         \n\
         Required:\n\
         \x20 --server-ssh IP        Server public IP (for SSH)\n\
         \x20 --client-ssh IP        Client public IP (for SSH)\n\
         \x20 --server-private IP    Server private IP (for bench URLs over VPC)\n\
         \x20 --instance-type TYPE   EC2 instance type (e.g. c8g.12xlarge)\n\
         \x20 --test-case NAME       Named test case (repeatable)\n\
         \n\
         Options:\n\
         \x20 --list-test-cases      Print supported test cases and exit\n\
         \x20 --ssh-user USER        SSH user for both nodes (default: alpine)\n\
         \x20 --port PORT            Server port (default: 3090)\n\
         \x20 --duration SECS        Test duration per run (default: 60)\n\
         \x20 --warmup SECS          Warmup duration per run (default: 5)\n\
         \x20 --results-dir DIR      Override output directory (default: docs/perf/<instance-type>/<timestamp>)\n\
         \x20 --os-monitors          Collect vmstat/sar/iostat/pidstat artifacts on both nodes\n\
         \x20 --perf                 Collect perf stat artifacts on the server; also on the client when --spinr-mode host\n\
         \x20 --spinr-mode MODE      Client load generator mode: docker|host (default: docker)\n\
         \n\
         Supported test cases:\n\
         \x20 {}\n",
        supported_test_case_names()
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut server_ssh: Option<String> = None;
    let mut client_ssh: Option<String> = None;
    let mut server_private: Option<String> = None;
    let mut instance_type: Option<String> = None;
    let mut ssh_user = SSH_USER.to_string();
    let mut port: u16 = DEFAULT_PORT;
    let mut duration: u32 = 60;
    let mut warmup: u32 = 5;
    let mut results_dir_override: Option<std::path::PathBuf> = None;
    let mut test_cases = Vec::new();
    let mut list_test_cases = false;
    let mut os_monitors = false;
    let mut perf_stat = false;
    let mut spinr_mode = SpinrMode::Docker;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--server-ssh" => {
                server_ssh = Some(args[i + 1].clone());
                i += 2;
            }
            "--client-ssh" => {
                client_ssh = Some(args[i + 1].clone());
                i += 2;
            }
            "--server-private" => {
                server_private = Some(args[i + 1].clone());
                i += 2;
            }
            "--instance-type" => {
                instance_type = Some(args[i + 1].clone());
                i += 2;
            }
            "--test-case" => {
                let value = &args[i + 1];
                if value == "all" {
                    test_cases.extend(TEST_CASES.iter().copied());
                } else if let Some(case) = find_test_case(value) {
                    test_cases.push(case);
                } else {
                    eprintln!("invalid --test-case: {value}");
                    usage();
                }
                i += 2;
            }
            "--list-test-cases" => {
                list_test_cases = true;
                i += 1;
            }
            "--ssh-user" => {
                ssh_user = args[i + 1].clone();
                i += 2;
            }
            "--port" => {
                port = args[i + 1].parse().expect("invalid --port");
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
                results_dir_override = Some(std::path::PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--os-monitors" => {
                os_monitors = true;
                i += 1;
            }
            "--perf" | "--perf-stat" => {
                perf_stat = true;
                i += 1;
            }
            "--spinr-mode" => {
                spinr_mode = SpinrMode::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --spinr-mode: {}", args[i + 1]);
                    usage();
                });
                i += 2;
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown option: {other}");
                usage();
            }
        }
    }

    if list_test_cases {
        print_supported_test_cases_and_exit();
    }

    if test_cases.is_empty() {
        eprintln!("error: at least one --test-case is required");
        usage();
    }

    let require = |opt: Option<String>, name: &str| -> String {
        opt.unwrap_or_else(|| {
            eprintln!("error: {name} is required");
            usage();
        })
    };

    let server_ssh = require(server_ssh, "--server-ssh");
    let client_ssh = require(client_ssh, "--client-ssh");
    let server_private = require(server_private, "--server-private");
    let instance_type = require(instance_type, "--instance-type");

    let results_dir = results_dir_override.unwrap_or_else(|| {
        let ts = Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H-%M-%SZ"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        std::path::PathBuf::from(format!("docs/perf/{instance_type}/{ts}"))
    });

    Args {
        server_ssh,
        client_ssh,
        server_private,
        ssh_user,
        instance_type,
        port,
        duration,
        warmup,
        results_dir,
        test_cases,
        os_monitors,
        perf_stat,
        spinr_mode,
    }
}

fn ssh_run(user: &str, host: &str, remote_cmd: &str) -> std::io::Result<std::process::Output> {
    Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(format!("{user}@{host}"))
        .arg(remote_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn ssh_server(args: &Args, remote_cmd: &str) -> std::io::Result<std::process::Output> {
    ssh_run(&args.ssh_user, &args.server_ssh, remote_cmd)
}

fn ssh_client(args: &Args, remote_cmd: &str) -> std::io::Result<std::process::Output> {
    ssh_run(&args.ssh_user, &args.client_ssh, remote_cmd)
}

fn ssh_side(
    args: &Args,
    side: RemoteSide,
    remote_cmd: &str,
) -> std::io::Result<std::process::Output> {
    match side {
        RemoteSide::Server => ssh_server(args, remote_cmd),
        RemoteSide::Client => ssh_client(args, remote_cmd),
    }
}

fn start_server_container(args: &Args, framework: Framework) {
    let name = framework.container_name();
    let image = framework.image();
    let command = framework.launch_cmd(args.port);
    println!(">>> Starting {} on server", framework.as_str());
    let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
    let docker_cmd = format!(
        "docker run -d --name {name} --network host --ulimit nofile=65535:65535 {image} {command}"
    );
    match ssh_server(args, &docker_cmd) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!(
                "  warning: docker run {} stderr: {}",
                framework.as_str(),
                stderr.trim()
            );
        }
        Err(e) => eprintln!("  failed to start {}: {e}", framework.as_str()),
    }
    thread::sleep(Duration::from_secs(2));
}

fn stop_server_container(args: &Args, framework: Framework) {
    let name = framework.container_name();
    println!(">>> Stopping {} on server", framework.as_str());
    let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
}

fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    println!("    Waiting for {addr}...");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(500)).is_ok() {
            println!("    Health check passed");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("server on {addr} did not start within {timeout:?}"))
}

fn run_key(framework: Framework, test_case: TestCase) -> String {
    format!(
        "{}_{}_c{}",
        framework.as_str(),
        test_case.key_label,
        test_case.concurrency
    )
}

fn monitor_window_secs(args: &Args) -> u32 {
    args.warmup + args.duration + (MONITOR_MARGIN_SECS * 2)
}

fn remote_artifact_path(key: &str, side: RemoteSide, suffix: &str) -> String {
    format!("/tmp/{key}.{}.{}", side.label(), suffix)
}

fn pull_remote_file(
    args: &Args,
    side: RemoteSide,
    remote_path: &str,
    local_path: &std::path::Path,
) {
    let remote_cmd = format!("test -f {remote_path} && cat {remote_path}");
    match ssh_side(args, side, &remote_cmd) {
        Ok(o) if o.status.success() => {
            let _ = fs::write(local_path, &o.stdout);
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!(
                "    warning: failed to collect {} from {}: {}",
                remote_path,
                side.label(),
                stderr.trim()
            );
        }
        Err(e) => eprintln!(
            "    warning: failed to collect {} from {}: {e}",
            remote_path,
            side.label()
        ),
    }
}

fn cleanup_remote_file(args: &Args, side: RemoteSide, remote_path: &str) {
    let _ = ssh_side(args, side, &format!("rm -f {remote_path}"));
}

fn start_remote_capture(args: &Args, side: RemoteSide, shell_cmd: &str) {
    let remote_cmd = format!("nohup sh -lc \"{shell_cmd}\" >/dev/null 2>&1 &");
    if let Err(e) = ssh_side(args, side, &remote_cmd) {
        eprintln!(
            "    warning: failed to start {} capture on {}: {e}",
            shell_cmd,
            side.label()
        );
    }
}

fn container_pid(args: &Args, framework: Framework) -> Option<u32> {
    let remote_cmd = format!(
        "docker inspect -f '{{{{.State.Pid}}}}' {}",
        framework.container_name()
    );
    let out = ssh_server(args, &remote_cmd).ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn start_host_monitors(args: &Args, key: &str, server_pid: Option<u32>) {
    let samples = monitor_window_secs(args);

    for (suffix, cmd) in [
        (
            "vmstat.txt",
            format!(
                "vmstat 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Server, "vmstat.txt")
            ),
        ),
        (
            "sar-u.txt",
            format!(
                "sar -u 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Server, "sar-u.txt")
            ),
        ),
        (
            "sar-q.txt",
            format!(
                "sar -q 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Server, "sar-q.txt")
            ),
        ),
        (
            "sar-net.txt",
            format!(
                "sar -n DEV,TCP,ETCP 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Server, "sar-net.txt")
            ),
        ),
        (
            "iostat.txt",
            format!(
                "iostat -xz 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Server, "iostat.txt")
            ),
        ),
    ] {
        let _ = suffix;
        start_remote_capture(args, RemoteSide::Server, &cmd);
    }

    if let Some(pid) = server_pid {
        let cmd = format!(
            "pidstat -durwt -p {pid} 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "pidstat.txt")
        );
        start_remote_capture(args, RemoteSide::Server, &cmd);
    }

    for (suffix, cmd) in [
        (
            "vmstat.txt",
            format!(
                "vmstat 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Client, "vmstat.txt")
            ),
        ),
        (
            "sar-u.txt",
            format!(
                "sar -u 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Client, "sar-u.txt")
            ),
        ),
        (
            "sar-q.txt",
            format!(
                "sar -q 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Client, "sar-q.txt")
            ),
        ),
        (
            "sar-net.txt",
            format!(
                "sar -n DEV,TCP,ETCP 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Client, "sar-net.txt")
            ),
        ),
        (
            "iostat.txt",
            format!(
                "iostat -xz 1 {samples} > {}",
                remote_artifact_path(key, RemoteSide::Client, "iostat.txt")
            ),
        ),
    ] {
        let _ = suffix;
        start_remote_capture(args, RemoteSide::Client, &cmd);
    }

    if args.spinr_mode == SpinrMode::Host {
        let cmd = format!(
            "pidstat -durwt -C spinr 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "pidstat.txt")
        );
        start_remote_capture(args, RemoteSide::Client, &cmd);
    }
}

fn start_server_perf_stat(args: &Args, key: &str, server_pid: u32) {
    let perf_path = remote_artifact_path(key, RemoteSide::Server, "perf-stat.txt");
    let load_secs = args.warmup + args.duration;
    let cmd = format!(
        "perf stat -x, -e {PERF_COUNTERS} -o {perf_path} -p {server_pid} -- sleep {load_secs}"
    );
    start_remote_capture(args, RemoteSide::Server, &cmd);
}

fn collect_run_artifacts(args: &Args, key: &str) {
    if args.os_monitors {
        for suffix in [
            "vmstat.txt",
            "sar-u.txt",
            "sar-q.txt",
            "sar-net.txt",
            "iostat.txt",
            "pidstat.txt",
        ] {
            let remote_path = remote_artifact_path(key, RemoteSide::Server, suffix);
            let local_path =
                args.results_dir
                    .join(format!("{key}.{}.{}", RemoteSide::Server.label(), suffix));
            pull_remote_file(args, RemoteSide::Server, &remote_path, &local_path);
            cleanup_remote_file(args, RemoteSide::Server, &remote_path);
        }

        for suffix in [
            "vmstat.txt",
            "sar-u.txt",
            "sar-q.txt",
            "sar-net.txt",
            "iostat.txt",
        ] {
            let remote_path = remote_artifact_path(key, RemoteSide::Client, suffix);
            let local_path =
                args.results_dir
                    .join(format!("{key}.{}.{}", RemoteSide::Client.label(), suffix));
            pull_remote_file(args, RemoteSide::Client, &remote_path, &local_path);
            cleanup_remote_file(args, RemoteSide::Client, &remote_path);
        }

        if args.spinr_mode == SpinrMode::Host {
            let remote_path = remote_artifact_path(key, RemoteSide::Client, "pidstat.txt");
            let local_path = args
                .results_dir
                .join(format!("{key}.{}.pidstat.txt", RemoteSide::Client.label()));
            pull_remote_file(args, RemoteSide::Client, &remote_path, &local_path);
            cleanup_remote_file(args, RemoteSide::Client, &remote_path);
        }
    }

    if args.perf_stat {
        let mut sides = vec![RemoteSide::Server];
        if client_perf_enabled(args) {
            sides.push(RemoteSide::Client);
        }

        for side in sides {
            let remote_path = remote_artifact_path(key, side, "perf-stat.txt");
            let local_path = args
                .results_dir
                .join(format!("{key}.{}.perf-stat.txt", side.label()));
            pull_remote_file(args, side, &remote_path, &local_path);
            cleanup_remote_file(args, side, &remote_path);
        }
    }
}

fn write_run_meta(
    args: &Args,
    key: &str,
    framework: Framework,
    test_case: TestCase,
    started_at_utc: &str,
    completed_at_utc: &str,
) {
    let meta = serde_json::json!({
        "key": key,
        "framework": framework.as_str(),
        "test_case": test_case.name,
        "path": format!("/{}", test_case.path),
        "concurrency": test_case.concurrency,
        "warmup_secs": args.warmup,
        "duration_secs": args.duration,
        "server_host": args.server_ssh,
        "client_host": args.client_ssh,
        "spinr_mode": args.spinr_mode.as_str(),
        "os_monitors": args.os_monitors,
        "perf_stat": args.perf_stat,
        "server_perf_stat": args.perf_stat,
        "client_perf_stat": client_perf_enabled(args),
        "perf_scope": perf_scope_label(args),
        "started_at_utc": started_at_utc,
        "completed_at_utc": completed_at_utc,
    });
    let path = args.results_dir.join(format!("{key}.meta.json"));
    let _ = fs::write(path, serde_json::to_vec_pretty(&meta).unwrap());
}

fn run_bench(
    args: &Args,
    key: &str,
    url: &str,
    concurrency: u32,
    outfile: &std::path::Path,
) -> Option<Value> {
    let spinr_cmd = format!(
        "{DEFAULT_SPINR_BIN} load-test --max-throughput -c {concurrency} -d {} -w {} -j {url}",
        args.duration, args.warmup
    );

    let remote_cmd = match args.spinr_mode {
        SpinrMode::Docker => format!(
            "docker run --rm --network host --ulimit nofile=65535:65535 spinr load-test \
             --max-throughput -c {concurrency} -d {} -w {} -j {url}",
            args.duration, args.warmup
        ),
        SpinrMode::Host if args.perf_stat => {
            let perf_path = remote_artifact_path(key, RemoteSide::Client, "perf-stat.txt");
            format!("perf stat -x, -e {PERF_COUNTERS} -o {perf_path} -- {spinr_cmd}")
        }
        SpinrMode::Host => spinr_cmd.clone(),
    };

    match ssh_client(args, &remote_cmd) {
        Ok(o) if o.status.success() => {
            let _ = fs::write(outfile, &o.stdout);
            let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
            if let Some(ref v) = val {
                println!(
                    "    → rps={} p99={}ms",
                    val_str(v, "rps"),
                    val_str(v, "latency_p99_ms")
                );
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

fn collect_docker_stats(args: &Args, key: &str) {
    let remote_cmd =
        "docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}'";
    if let Ok(out) = ssh_server(args, remote_cmd) {
        let path = args.results_dir.join(format!("stats_{key}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

fn collect_docker_logs(args: &Args, framework: Framework, key: &str) {
    let remote_cmd = format!("docker logs {} 2>&1", framework.container_name());
    if let Ok(out) = ssh_server(args, &remote_cmd) {
        let path = args.results_dir.join(format!("logs_{key}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

fn run_test_case(
    args: &Args,
    framework: Framework,
    test_case: TestCase,
    results: &mut BTreeMap<String, Value>,
) {
    let key = run_key(framework, test_case);
    let url = format!(
        "http://{}:{}/{}",
        args.server_private, args.port, test_case.path
    );
    let outfile = args.results_dir.join(format!("{key}.json"));

    println!();
    println!(
        "--- {} /{} c={} ---",
        framework.as_str(),
        test_case.path,
        test_case.concurrency
    );
    start_server_container(args, framework);
    if let Err(e) = wait_for_port(&args.server_ssh, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_server_container(args, framework);
        std::process::exit(1);
    }

    let server_pid = container_pid(args, framework);
    if args.os_monitors {
        start_host_monitors(args, &key, server_pid);
        thread::sleep(Duration::from_secs(MONITOR_MARGIN_SECS as u64));
    }

    if args.perf_stat {
        match server_pid {
            Some(pid) => start_server_perf_stat(args, &key, pid),
            None => eprintln!("    warning: failed to determine server PID for perf stat"),
        }
    }

    println!("  [{key}] → {url}");
    let started_at_utc = chrono_lite_utc();
    if let Some(v) = run_bench(args, &key, &url, test_case.concurrency, &outfile) {
        results.insert(key.clone(), v);
    }
    let completed_at_utc = chrono_lite_utc();

    if args.os_monitors {
        thread::sleep(Duration::from_secs(MONITOR_MARGIN_SECS as u64));
    }

    collect_run_artifacts(args, &key);
    write_run_meta(
        args,
        &key,
        framework,
        test_case,
        &started_at_utc,
        &completed_at_utc,
    );
    collect_docker_stats(args, &key);
    collect_docker_logs(args, framework, &key);
    stop_server_container(args, framework);
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

fn value_as_f64(v: Option<&Value>, key: &str) -> Option<f64> {
    v.and_then(|val| val.get(key)).and_then(Value::as_f64)
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

fn generate_report(results: &BTreeMap<String, Value>, args: &Args) {
    let now = chrono_lite_utc();
    let mut report = format!(
        "# Performance Test Results\n\
         \n\
         Instance: {}\n\
         Server: {} (private: {}:{})\n\
         Client: {}\n\
         Duration: {}s | Warmup: {}s\n\
         Spinr mode: {}\n\
         OS monitors: {}\n\
         Perf stat: {}\n\
         Date: {now}\n\
         \n\
         ## Runs\n\
         \n\
         | Test case | Framework | Path | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |\n\
         |-----------|-----------|------|-------------|-----|----------|----------|-----------|\n",
        args.instance_type,
        args.server_ssh,
        args.server_private,
        args.port,
        args.client_ssh,
        args.duration,
        args.warmup,
        args.spinr_mode.as_str(),
        args.os_monitors,
        perf_scope_label(args),
    );

    for test_case in &args.test_cases {
        for framework in [Framework::Harrow, Framework::Axum] {
            let key = run_key(framework, *test_case);
            let (rps, p50, p99, p999) = extract_latencies(results.get(&key));
            report.push_str(&format!(
                "| {} | {} | /{} | {} | {} | {} | {} | {} |\n",
                test_case.name,
                framework.as_str(),
                test_case.path,
                test_case.concurrency,
                rps,
                p50,
                p99,
                p999
            ));
        }
    }

    report.push_str(
        "\n## Comparison\n\n| Test case | Harrow RPS | Axum RPS | Delta % |\n|-----------|------------|----------|---------|\n",
    );

    for test_case in &args.test_cases {
        let harrow = results.get(&run_key(Framework::Harrow, *test_case));
        let axum = results.get(&run_key(Framework::Axum, *test_case));
        let harrow_rps = value_as_f64(harrow, "rps");
        let axum_rps = value_as_f64(axum, "rps");
        let delta = match (harrow_rps, axum_rps) {
            (Some(h), Some(a)) if a != 0.0 => format!("{:+.2}%", ((h - a) / a) * 100.0),
            _ => "-".into(),
        };
        report.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            test_case.name,
            harrow_rps
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "-".into()),
            axum_rps
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "-".into()),
            delta
        ));
    }

    let report_path = args.results_dir.join("summary.md");
    fs::write(&report_path, &report).unwrap();
    println!("Summary written to {}", report_path.display());
}

fn chrono_lite_utc() -> String {
    match Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S UTC"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    }
}

fn preflight_checks(args: &Args) {
    println!("--- Preflight checks ---");

    for (label, host) in [("server", &args.server_ssh), ("client", &args.client_ssh)] {
        let out = ssh_run(&args.ssh_user, host, "echo ok");
        match out {
            Ok(o) if o.status.success() => println!("  SSH to {label} ({host}): ok"),
            _ => {
                eprintln!("  SSH to {label} ({host}): FAILED");
                std::process::exit(1);
            }
        }
    }

    let out = ssh_server(args, "docker info >/dev/null 2>&1 && echo ok");
    match out {
        Ok(o) if o.status.success() => println!("  Docker on server: ok"),
        _ => {
            eprintln!(
                "  Docker on server ({}): FAILED — is Docker running?",
                args.server_ssh
            );
            std::process::exit(1);
        }
    }

    if args.spinr_mode == SpinrMode::Docker {
        let out = ssh_client(args, "docker info >/dev/null 2>&1 && echo ok");
        match out {
            Ok(o) if o.status.success() => println!("  Docker on client: ok"),
            _ => {
                eprintln!(
                    "  Docker on client ({}): FAILED — is Docker running?",
                    args.client_ssh
                );
                std::process::exit(1);
            }
        }
    }

    let out = ssh_server(
        args,
        "docker run --rm --ulimit nofile=65535:65535 alpine sh -c 'ulimit -n'",
    );
    match out {
        Ok(o) if o.status.success() => {
            let val = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if val == "65535" {
                println!("  Container ulimit on server: {val} (ok)");
            } else {
                eprintln!("  WARNING: Container ulimit on server: {val} (expected 65535)");
            }
        }
        _ => eprintln!("  WARNING: Could not verify container ulimit on server"),
    }

    if args.spinr_mode == SpinrMode::Docker {
        let out = ssh_client(
            args,
            "docker run --rm --ulimit nofile=65535:65535 alpine sh -c 'ulimit -n'",
        );
        match out {
            Ok(o) if o.status.success() => {
                let val = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if val == "65535" {
                    println!("  Container ulimit on client: {val} (ok)");
                } else {
                    eprintln!("  WARNING: Container ulimit on client: {val} (expected 65535)");
                }
            }
            _ => eprintln!("  WARNING: Could not verify container ulimit on client"),
        }
    }

    for image in ["harrow-perf-server", "axum-perf-server"] {
        let out = ssh_server(
            args,
            &format!("docker image inspect {image} >/dev/null 2>&1 && echo ok"),
        );
        match out {
            Ok(o) if o.status.success() => println!("  Image {image} on server: ok"),
            _ => {
                eprintln!("  Image {image} on server: MISSING");
                std::process::exit(1);
            }
        }
    }

    if args.spinr_mode == SpinrMode::Docker {
        let out = ssh_client(
            args,
            "docker image inspect spinr >/dev/null 2>&1 && echo ok",
        );
        match out {
            Ok(o) if o.status.success() => println!("  Image spinr on client: ok"),
            _ => {
                eprintln!("  Image spinr on client: MISSING");
                std::process::exit(1);
            }
        }
    }

    if args.spinr_mode == SpinrMode::Host {
        let out = ssh_client(args, &format!("test -x {DEFAULT_SPINR_BIN} && echo ok"));
        match out {
            Ok(o) if o.status.success() => {
                println!("  Host spinr on client: {DEFAULT_SPINR_BIN} (ok)");
            }
            _ => {
                eprintln!("  Host spinr on client: MISSING ({DEFAULT_SPINR_BIN})");
                std::process::exit(1);
            }
        }
    }

    if args.os_monitors {
        for side in [RemoteSide::Server, RemoteSide::Client] {
            let (host, cmd) = match side {
                RemoteSide::Server => (
                    &args.server_ssh,
                    "command -v vmstat >/dev/null && command -v sar >/dev/null && command -v iostat >/dev/null && command -v pidstat >/dev/null && echo ok",
                ),
                RemoteSide::Client if args.spinr_mode == SpinrMode::Host => (
                    &args.client_ssh,
                    "command -v vmstat >/dev/null && command -v sar >/dev/null && command -v iostat >/dev/null && command -v pidstat >/dev/null && echo ok",
                ),
                RemoteSide::Client => (
                    &args.client_ssh,
                    "command -v vmstat >/dev/null && command -v sar >/dev/null && command -v iostat >/dev/null && echo ok",
                ),
            };
            let out = ssh_run(&args.ssh_user, host, cmd);
            match out {
                Ok(o) if o.status.success() => {
                    println!("  OS monitor tools on {}: ok", side.label())
                }
                _ => {
                    eprintln!("  OS monitor tools on {} ({}): MISSING", side.label(), host);
                    std::process::exit(1);
                }
            }
        }
    }

    if args.perf_stat {
        let out = ssh_run(
            &args.ssh_user,
            &args.server_ssh,
            "command -v perf >/dev/null && echo ok",
        );
        match out {
            Ok(o) if o.status.success() => println!("  perf on server: ok"),
            _ => {
                eprintln!("  perf on server ({}): MISSING", args.server_ssh);
                std::process::exit(1);
            }
        }

        if client_perf_enabled(args) {
            let out = ssh_run(
                &args.ssh_user,
                &args.client_ssh,
                "command -v perf >/dev/null && echo ok",
            );
            match out {
                Ok(o) if o.status.success() => println!("  perf on client: ok"),
                _ => {
                    eprintln!("  perf on client ({}): MISSING", args.client_ssh);
                    std::process::exit(1);
                }
            }
        } else {
            println!("  perf on client: skipped (spinr-mode=docker)");
        }
    }

    println!("--- Preflight checks passed ---");
    println!();
}

fn main() {
    let args = parse_args();
    fs::create_dir_all(&args.results_dir).unwrap();

    preflight_checks(&args);

    println!("============================================");
    println!(" Matched Harrow/Axum Performance Test");
    println!(" Instance: {}", args.instance_type);
    println!(
        " Server: {} (private: {}:{})",
        args.server_ssh, args.server_private, args.port
    );
    println!(" Client: {}", args.client_ssh);
    println!(" Duration: {}s  Warmup: {}s", args.duration, args.warmup);
    println!(" Spinr mode: {}", args.spinr_mode.as_str());
    println!(" OS monitors: {}", args.os_monitors);
    println!(" Perf stat: {}", perf_scope_label(&args));
    println!(
        " Test cases: {}",
        args.test_cases
            .iter()
            .map(|case| case.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(" Results: {}/", args.results_dir.display());
    println!("============================================");
    println!();

    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    for test_case in &args.test_cases {
        println!("========== TEST CASE: {} ==========", test_case.name);
        run_test_case(&args, Framework::Harrow, *test_case, &mut results);
        thread::sleep(SLEEP_BETWEEN_RUNS);
        run_test_case(&args, Framework::Axum, *test_case, &mut results);
        thread::sleep(SLEEP_BETWEEN_RUNS);
    }

    println!();
    println!("========== GENERATING SUMMARY ==========");
    generate_report(&results, &args);
    println!();
    println!("Done! Results in {}/", args.results_dir.display());
}
