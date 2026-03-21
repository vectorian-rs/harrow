//! Remote performance test orchestrator.
//!
//! Runs from the laptop and drives both EC2 nodes via SSH:
//! - server node runs `harrow-perf-server` or `axum-perf-server` in Docker
//! - client node runs `spinr` either in Docker or directly on the host
//! - optional host telemetry plus `perf stat` / `perf record` are collected per run
//!
//! Each invocation takes one or more `--config <path>` arguments pointing to
//! spinr TOML templates.  The orchestrator renders `{{ server }}`, `{{ duration }}`
//! and `{{ warmup }}` placeholders, uploads the result to the client, and runs
//! `spinr bench <file> -j`.
//!
//! Session configs are auto-detected by filename prefix (`session-`).  These run
//! Harrow-only with `--session` passed to the perf server.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_PORT: u16 = 3090;
const SSH_USER: &str = "alpine";
const DEFAULT_SPINR_BIN: &str = "/usr/local/bin/spinr";
const SLEEP_BETWEEN_RUNS: Duration = Duration::from_secs(2);
const MONITOR_MARGIN_SECS: u32 = 2;
const PERF_COUNTERS: &str =
    "task-clock,cpu-clock,context-switches,cpu-migrations,page-faults,minor-faults,major-faults";
const PERF_RECORD_FREQ_HZ: u32 = 1000;
const PERF_RECORD_CALL_GRAPH: &str = "fp";

// ---------------------------------------------------------------------------
// Template rendering — the only "parsing" the orchestrator does
// ---------------------------------------------------------------------------

fn render_template(raw: &str, server: &str, duration: u32, warmup: u32) -> String {
    raw.replace("{{ server }}", server)
        .replace("{{ duration }}", &duration.to_string())
        .replace("{{ warmup }}", &warmup.to_string())
}

fn is_session_config(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("session-"))
}

fn config_name(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

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
enum PerfMode {
    Stat,
    Record,
    Both,
}

impl PerfMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "stat" => Some(Self::Stat),
            "record" => Some(Self::Record),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Stat => "stat",
            Self::Record => "record",
            Self::Both => "both",
        }
    }

    fn collects_stat(self) -> bool {
        matches!(self, Self::Stat | Self::Both)
    }

    fn collects_record(self) -> bool {
        matches!(self, Self::Record | Self::Both)
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

    fn launch_cmd(self, port: u16, extra_flags: &str) -> String {
        let base = match self {
            Self::Harrow => format!("/harrow-perf-server --bind 0.0.0.0 --port {port}"),
            Self::Axum => format!("/axum-perf-server --bind 0.0.0.0 --port {port}"),
        };
        if extra_flags.is_empty() {
            base
        } else {
            format!("{base} {extra_flags}")
        }
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::Harrow => "harrow-perf-server",
            Self::Axum => "axum-perf-server",
        }
    }

    fn binary_path(self) -> &'static str {
        match self {
            Self::Harrow => "/harrow-perf-server",
            Self::Axum => "/axum-perf-server",
        }
    }
}

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

struct Args {
    server_ssh: String,
    client_ssh: String,
    server_private: String,
    ssh_user: String,
    instance_type: String,
    port: u16,
    duration: u32,
    warmup: u32,
    results_dir: PathBuf,
    config_paths: Vec<PathBuf>,
    server_flags: Option<String>,
    os_monitors: bool,
    perf_enabled: bool,
    perf_mode: PerfMode,
    spinr_mode: SpinrMode,
}

fn client_perf_enabled(args: &Args) -> bool {
    args.perf_enabled && args.spinr_mode == SpinrMode::Host
}

fn perf_stat_enabled(args: &Args) -> bool {
    args.perf_enabled && args.perf_mode.collects_stat()
}

fn perf_record_enabled(args: &Args) -> bool {
    args.perf_enabled && args.perf_mode.collects_record()
}

fn perf_scope_label(args: &Args) -> String {
    if !args.perf_enabled {
        "off".into()
    } else {
        let scope = if client_perf_enabled(args) {
            "server + client"
        } else {
            "server only"
        };
        format!("{} ({scope})", args.perf_mode.as_str())
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: harrow-remote-perf-test --server-ssh IP --client-ssh IP --server-private IP --instance-type TYPE --config PATH [OPTIONS]\n\
         \n\
         Runs matched Harrow/Axum perf-server comparisons on two EC2 nodes.\n\
         \n\
         Required:\n\
         \x20 --server-ssh IP        Server public IP (for SSH)\n\
         \x20 --client-ssh IP        Client public IP (for SSH)\n\
         \x20 --server-private IP    Server private IP (for bench URLs over VPC)\n\
         \x20 --instance-type TYPE   EC2 instance type (e.g. c8g.12xlarge)\n\
         \x20 --config PATH          Spinr TOML template (repeatable)\n\
         \n\
         Options:\n\
         \x20 --server-flags FLAGS   Extra flags for harrow-perf-server\n\
         \x20 --ssh-user USER        SSH user for both nodes (default: alpine)\n\
         \x20 --port PORT            Server port (default: 3090)\n\
         \x20 --duration SECS        Test duration (renders into TOML template, default: 60)\n\
         \x20 --warmup SECS          Warmup duration (renders into TOML template, default: 5)\n\
         \x20 --results-dir DIR      Override output directory\n\
         \x20 --os-monitors          Collect vmstat/sar/iostat/pidstat on both nodes\n\
         \x20 --perf                 Collect perf artifacts (default mode: stat)\n\
         \x20 --perf-mode MODE       Perf mode: stat|record|both (default: stat)\n\
         \x20 --spinr-mode MODE      Client mode: docker|host (default: docker)\n\
         \n\
         Session configs (filename starts with 'session-') automatically:\n\
         \x20 - pass --session to harrow-perf-server\n\
         \x20 - skip the Axum comparison run\n"
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
    let mut results_dir_override: Option<PathBuf> = None;
    let mut config_paths: Vec<PathBuf> = Vec::new();
    let mut server_flags: Option<String> = None;
    let mut os_monitors = false;
    let mut perf_enabled = false;
    let mut perf_mode = PerfMode::Stat;
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
            "--config" => {
                config_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--server-flags" => {
                server_flags = Some(args[i + 1].clone());
                i += 2;
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
                results_dir_override = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--os-monitors" => {
                os_monitors = true;
                i += 1;
            }
            "--perf" => {
                perf_enabled = true;
                i += 1;
            }
            "--perf-stat" => {
                perf_enabled = true;
                perf_mode = PerfMode::Stat;
                i += 1;
            }
            "--perf-record" => {
                perf_enabled = true;
                perf_mode = PerfMode::Record;
                i += 1;
            }
            "--perf-mode" => {
                perf_enabled = true;
                perf_mode = PerfMode::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --perf-mode: {}", args[i + 1]);
                    usage();
                });
                i += 2;
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

    if config_paths.is_empty() {
        eprintln!("error: at least one --config is required");
        usage();
    }

    // Verify all config files exist upfront.
    for p in &config_paths {
        if !p.exists() {
            eprintln!("error: config file not found: {}", p.display());
            std::process::exit(1);
        }
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
        PathBuf::from(format!("docs/perf/{instance_type}/{ts}"))
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
        config_paths,
        server_flags,
        os_monitors,
        perf_enabled,
        perf_mode,
        spinr_mode,
    }
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

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

/// SCP a local file to the client node.
fn scp_to_client(args: &Args, local_path: &Path, remote_path: &str) {
    let dest = format!("{}@{}:{}", args.ssh_user, args.client_ssh, remote_path);
    let out = Command::new("scp")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg(local_path)
        .arg(&dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("    warning: scp to {dest} failed: {}", stderr.trim());
        }
        Err(e) => eprintln!("    warning: scp to {dest} failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Server container management
// ---------------------------------------------------------------------------

fn start_server_container(args: &Args, framework: Framework, server_flags: &str) {
    let name = framework.container_name();
    let image = framework.image();
    let command = framework.launch_cmd(args.port, server_flags);
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

// ---------------------------------------------------------------------------
// Keys and paths
// ---------------------------------------------------------------------------

fn run_key(framework: Framework, cfg_name: &str) -> String {
    format!("{}_{}", framework.as_str(), cfg_name)
}

fn monitor_window_secs(args: &Args) -> u32 {
    args.warmup + args.duration + (MONITOR_MARGIN_SECS * 2)
}

fn remote_artifact_path(key: &str, side: RemoteSide, suffix: &str) -> String {
    format!("/tmp/{key}.{}.{}", side.label(), suffix)
}

fn remote_perf_symfs_dir(key: &str, side: RemoteSide) -> String {
    format!("/tmp/{key}.{}.perf-symfs", side.label())
}

// ---------------------------------------------------------------------------
// File transfer and cleanup helpers
// ---------------------------------------------------------------------------

fn pull_remote_file(args: &Args, side: RemoteSide, remote_path: &str, local_path: &Path) {
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

fn remote_file_exists(args: &Args, side: RemoteSide, remote_path: &str) -> bool {
    matches!(
        ssh_side(args, side, &format!("test -f {remote_path} && echo ok")),
        Ok(o) if o.status.success()
    )
}

fn cleanup_remote_file(args: &Args, side: RemoteSide, remote_path: &str) {
    let _ = ssh_side(args, side, &format!("rm -f {remote_path}"));
}

fn cleanup_remote_dir(args: &Args, side: RemoteSide, remote_path: &str) {
    let _ = ssh_side(args, side, &format!("rm -rf {remote_path}"));
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

fn inferno_available(args: &Args, side: RemoteSide) -> bool {
    matches!(
        ssh_side(
            args,
            side,
            "command -v inferno-collapse-perf >/dev/null && command -v inferno-flamegraph >/dev/null && echo ok",
        ),
        Ok(o) if o.status.success()
    )
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

// ---------------------------------------------------------------------------
// OS monitors
// ---------------------------------------------------------------------------

fn start_host_monitors(args: &Args, key: &str, server_pid: Option<u32>) {
    let samples = monitor_window_secs(args);

    for cmd in [
        format!(
            "vmstat 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "vmstat.txt")
        ),
        format!(
            "sar -u 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "sar-u.txt")
        ),
        format!(
            "sar -q 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "sar-q.txt")
        ),
        format!(
            "sar -n DEV,TCP,ETCP 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "sar-net.txt")
        ),
        format!(
            "iostat -xz 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "iostat.txt")
        ),
    ] {
        start_remote_capture(args, RemoteSide::Server, &cmd);
    }

    if let Some(pid) = server_pid {
        let cmd = format!(
            "pidstat -durwt -p {pid} 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Server, "pidstat.txt")
        );
        start_remote_capture(args, RemoteSide::Server, &cmd);
    }

    for cmd in [
        format!(
            "vmstat 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "vmstat.txt")
        ),
        format!(
            "sar -u 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "sar-u.txt")
        ),
        format!(
            "sar -q 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "sar-q.txt")
        ),
        format!(
            "sar -n DEV,TCP,ETCP 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "sar-net.txt")
        ),
        format!(
            "iostat -xz 1 {samples} > {}",
            remote_artifact_path(key, RemoteSide::Client, "iostat.txt")
        ),
    ] {
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

// ---------------------------------------------------------------------------
// Perf helpers
// ---------------------------------------------------------------------------

enum PerfTarget {
    AttachPid { pid: u32, sleep_secs: u32 },
}

fn perf_stat_cmd(output_path: &str, target: PerfTarget) -> String {
    let target_args = match target {
        PerfTarget::AttachPid { pid, sleep_secs } => {
            format!("-p {pid} -- sleep {sleep_secs}")
        }
    };
    format!(
        "sh -lc 'doas perf stat -x, -e {PERF_COUNTERS} -o {output_path} {target_args}; \
         status=$?; if test -f {output_path}; then doas chmod 0644 {output_path} >/dev/null 2>&1 || true; fi; exit $status'"
    )
}

fn perf_record_cmd(output_path: &str, target: PerfTarget) -> String {
    let target_args = match target {
        PerfTarget::AttachPid { pid, sleep_secs } => {
            format!("-p {pid} -- sleep {sleep_secs}")
        }
    };
    format!(
        "sh -lc 'doas perf record -m 1M -s -e cpu-clock --call-graph {PERF_RECORD_CALL_GRAPH} -F {PERF_RECORD_FREQ_HZ} \
         -o {output_path} {target_args}; status=$?; \
         if test -f {output_path}; then doas chmod 0644 {output_path} >/dev/null 2>&1 || true; fi; \
         exit $status'"
    )
}

fn start_server_perf_stat(args: &Args, key: &str, server_pid: u32) {
    let perf_path = remote_artifact_path(key, RemoteSide::Server, "perf-stat.txt");
    let cmd = perf_stat_cmd(
        &perf_path,
        PerfTarget::AttachPid {
            pid: server_pid,
            sleep_secs: args.warmup + args.duration,
        },
    );
    start_remote_capture(args, RemoteSide::Server, &cmd);
}

fn start_server_perf_record(args: &Args, key: &str, server_pid: u32) {
    let perf_path = remote_artifact_path(key, RemoteSide::Server, "perf.data");
    let cmd = perf_record_cmd(
        &perf_path,
        PerfTarget::AttachPid {
            pid: server_pid,
            sleep_secs: args.warmup + args.duration,
        },
    );
    start_remote_capture(args, RemoteSide::Server, &cmd);
}

fn prepare_remote_perf_symfs(
    args: &Args,
    side: RemoteSide,
    key: &str,
    framework: Option<Framework>,
) -> Option<String> {
    let symfs_dir = remote_perf_symfs_dir(key, side);
    let remote_cmd = match side {
        RemoteSide::Server => {
            let framework = framework?;
            format!(
                "sh -lc 'rm -rf {symfs_dir}; mkdir -p {symfs_dir}; \
                 docker cp {}:{} {symfs_dir}/{} >/dev/null'",
                framework.container_name(),
                framework.binary_path(),
                framework.binary_name()
            )
        }
        RemoteSide::Client => format!(
            "sh -lc 'rm -rf {symfs_dir}; mkdir -p {symfs_dir}; cp {DEFAULT_SPINR_BIN} {symfs_dir}/spinr'"
        ),
    };

    match ssh_side(args, side, &remote_cmd) {
        Ok(o) if o.status.success() => Some(symfs_dir),
        Ok(o) => {
            eprintln!(
                "    warning: failed to prepare perf symfs on {}: {}",
                side.label(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "    warning: failed to prepare perf symfs on {}: {e}",
                side.label()
            );
            None
        }
    }
}

fn postprocess_remote_perf_record(args: &Args, side: RemoteSide, key: &str, symfs_dir: &str) {
    let perf_path = remote_artifact_path(key, side, "perf.data");
    let report_path = remote_artifact_path(key, side, "perf-report.txt");
    let report_stderr_path = remote_artifact_path(key, side, "perf-report.stderr.txt");
    let report_cmd = format!(
        "sh -lc 'doas perf report --stdio --no-children --percent-limit 1 \
         -i {perf_path} --symfs {symfs_dir} 2>{report_stderr_path} > {report_path}; \
         status=$?; if test -f {report_path}; then chmod 0644 {report_path}; fi; exit $status'"
    );

    match ssh_side(args, side, &report_cmd) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "    warning: failed to generate perf report on {}: {}",
                side.label(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => eprintln!(
            "    warning: failed to generate perf report on {}: {e}",
            side.label()
        ),
    }

    let script_path = remote_artifact_path(key, side, "perf.script");
    let script_cmd = format!(
        "sh -lc 'doas perf script -i {perf_path} --symfs {symfs_dir} 2>/dev/null > {script_path}; \
         status=$?; if test -f {script_path}; then chmod 0644 {script_path}; fi; exit $status'"
    );

    match ssh_side(args, side, &script_cmd) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "    warning: failed to generate perf script on {}: {}",
                side.label(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => eprintln!(
            "    warning: failed to generate perf script on {}: {e}",
            side.label()
        ),
    }

    if !inferno_available(args, side) {
        eprintln!(
            "    warning: inferno tools missing on {}; skipping flamegraph generation",
            side.label()
        );
        return;
    }

    let folded_path = remote_artifact_path(key, side, "perf.folded");
    let svg_path = remote_artifact_path(key, side, "perf.svg");
    let flamegraph_cmd = format!(
        "sh -lc 'set -e; \
         doas perf script -i {perf_path} --symfs {symfs_dir} | inferno-collapse-perf > {folded_path}; \
         inferno-flamegraph < {folded_path} > {svg_path}; \
         chmod 0644 {folded_path} {svg_path}'"
    );

    match ssh_side(args, side, &flamegraph_cmd) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "    warning: failed to generate flamegraph on {}: {}",
                side.label(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => eprintln!(
            "    warning: failed to generate flamegraph on {}: {e}",
            side.label()
        ),
    }
}

fn host_spinr_perf_cmd(args: &Args, key: &str, spinr_cmd: &str) -> String {
    let spinr_stdout_path = remote_artifact_path(key, RemoteSide::Client, "spinr-stdout.json");
    let spinr_stderr_path = remote_artifact_path(key, RemoteSide::Client, "spinr-stderr.txt");
    let mut script = format!(
        "sh -lc 'out={spinr_stdout_path}; err={spinr_stderr_path}; \
         rm -f $out $err; \
         {spinr_cmd} >$out 2>$err & spinr_pid=$!; "
    );

    if perf_stat_enabled(args) {
        let stat_path = remote_artifact_path(key, RemoteSide::Client, "perf-stat.txt");
        script.push_str(&format!(
            "doas perf stat -x, -e {PERF_COUNTERS} -o {stat_path} -p $spinr_pid -- sleep {} \
             >/dev/null 2>&1 & perf_stat_pid=$!; ",
            args.warmup + args.duration
        ));
    }

    if perf_record_enabled(args) {
        let record_path = remote_artifact_path(key, RemoteSide::Client, "perf.data");
        script.push_str(&format!(
            "doas perf record -m 1M -s -e cpu-clock --call-graph {PERF_RECORD_CALL_GRAPH} -F {PERF_RECORD_FREQ_HZ} \
             -o {record_path} -p $spinr_pid -- sleep {} >/dev/null 2>&1 & perf_record_pid=$!; ",
            args.warmup + args.duration
        ));
    }

    script.push_str("wait $spinr_pid; status=$?; ");

    if perf_stat_enabled(args) {
        let stat_path = remote_artifact_path(key, RemoteSide::Client, "perf-stat.txt");
        script.push_str(&format!(
            "wait $perf_stat_pid || true; \
             if test -f {stat_path}; then doas chmod 0644 {stat_path} >/dev/null 2>&1 || true; fi; "
        ));
    }

    if perf_record_enabled(args) {
        let record_path = remote_artifact_path(key, RemoteSide::Client, "perf.data");
        script.push_str(&format!(
            "wait $perf_record_pid || true; \
             if test -f {record_path}; then doas chmod 0644 {record_path} >/dev/null 2>&1 || true; fi; "
        ));
    }

    script.push_str(
        "cat $out; if test -s $err; then cat $err >&2; fi; rm -f $out $err; exit $status'",
    );
    script
}

// ---------------------------------------------------------------------------
// Artifact collection
// ---------------------------------------------------------------------------

fn collect_run_artifacts(args: &Args, framework: Framework, key: &str) {
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

    if args.perf_enabled {
        let mut sides = vec![RemoteSide::Server];
        if client_perf_enabled(args) {
            sides.push(RemoteSide::Client);
        }

        for side in sides {
            if perf_stat_enabled(args) {
                let remote_path = remote_artifact_path(key, side, "perf-stat.txt");
                let local_path = args
                    .results_dir
                    .join(format!("{key}.{}.perf-stat.txt", side.label()));
                pull_remote_file(args, side, &remote_path, &local_path);
                cleanup_remote_file(args, side, &remote_path);
            }

            if perf_record_enabled(args) {
                let symfs_dir = prepare_remote_perf_symfs(
                    args,
                    side,
                    key,
                    if side == RemoteSide::Server {
                        Some(framework)
                    } else {
                        None
                    },
                );

                if let Some(symfs_dir) = symfs_dir.as_deref() {
                    postprocess_remote_perf_record(args, side, key, symfs_dir);
                }

                for suffix in [
                    "perf-report.txt",
                    "perf-report.stderr.txt",
                    "perf.script",
                    "perf.folded",
                    "perf.svg",
                ] {
                    let remote = remote_artifact_path(key, side, suffix);
                    if remote_file_exists(args, side, &remote) {
                        let local =
                            args.results_dir
                                .join(format!("{key}.{}.{}", side.label(), suffix));
                        pull_remote_file(args, side, &remote, &local);
                        cleanup_remote_file(args, side, &remote);
                    }
                }

                if let Some(symfs_dir) = symfs_dir.as_deref() {
                    cleanup_remote_dir(args, side, symfs_dir);
                }
            }
        }
    }
}

/// Parse the `connections` value from a spinr TOML template.
fn parse_connections_from_toml(config_path: &Path) -> u32 {
    fs::read_to_string(config_path)
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.trim().starts_with("connections"))
                .and_then(|l| l.split('=').nth(1))
                .and_then(|v| v.trim().parse::<u32>().ok())
        })
        .unwrap_or(0)
}

fn write_run_meta(
    args: &Args,
    key: &str,
    framework: Framework,
    cfg_name: &str,
    config_path: &Path,
    started_at_utc: &str,
    completed_at_utc: &str,
) {
    let meta = serde_json::json!({
        "key": key,
        "framework": framework.as_str(),
        "test_case": cfg_name,
        "path": config_path.display().to_string(),
        "concurrency": parse_connections_from_toml(config_path),
        "warmup_secs": args.warmup,
        "duration_secs": args.duration,
        "server_host": args.server_ssh,
        "client_host": args.client_ssh,
        "spinr_mode": args.spinr_mode.as_str(),
        "os_monitors": args.os_monitors,
        "perf_enabled": args.perf_enabled,
        "perf_mode": args.perf_mode.as_str(),
        "perf_stat": perf_stat_enabled(args),
        "perf_record": perf_record_enabled(args),
        "perf_scope": perf_scope_label(args),
        "started_at_utc": started_at_utc,
        "completed_at_utc": completed_at_utc,
    });
    let path = args.results_dir.join(format!("{key}.meta.json"));
    let _ = fs::write(path, serde_json::to_vec_pretty(&meta).unwrap());
}

// ---------------------------------------------------------------------------
// Bench runner
// ---------------------------------------------------------------------------

fn run_bench(args: &Args, key: &str, remote_config_path: &str, outfile: &Path) -> Option<Value> {
    let spinr_cmd = format!("{DEFAULT_SPINR_BIN} bench {remote_config_path} -j");

    let remote_cmd = match args.spinr_mode {
        SpinrMode::Docker => format!(
            "docker run --rm --network host --ulimit nofile=65535:65535 \
             -v {remote_config_path}:/bench.toml spinr bench /bench.toml -j"
        ),
        SpinrMode::Host if client_perf_enabled(args) => host_spinr_perf_cmd(args, key, &spinr_cmd),
        SpinrMode::Host => spinr_cmd.clone(),
    };

    match ssh_client(args, &remote_cmd) {
        Ok(o) if o.status.success() => {
            let _ = fs::write(outfile, &o.stdout);
            let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
            if let Some(ref v) = val {
                // spinr bench JSON nests metrics under scenarios[0].metrics
                let metrics = v.pointer("/scenarios/0/metrics").unwrap_or(v);
                println!(
                    "    -> rps={} p99={}ms",
                    val_str(metrics, "rps"),
                    val_str(metrics, "latency_p99_ms")
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

// ---------------------------------------------------------------------------
// Per-config run
// ---------------------------------------------------------------------------

fn run_config(
    args: &Args,
    config_path: &Path,
    framework: Framework,
    results: &mut BTreeMap<String, Value>,
) {
    let cfg_name = config_name(config_path);
    let key = run_key(framework, &cfg_name);
    let is_session = is_session_config(config_path);

    // Server flags: only Harrow receives extra flags; Axum rejects unknown args.
    let server_flags = if framework == Framework::Harrow {
        let mut parts = Vec::new();
        if is_session {
            parts.push("--session".to_string());
        }
        if let Some(ref flags) = args.server_flags {
            parts.push(flags.clone());
        }
        parts.join(" ")
    } else {
        String::new()
    };

    println!();
    println!("--- {} / {} ---", framework.as_str(), cfg_name);

    // 1. Start server.
    start_server_container(args, framework, &server_flags);
    if let Err(e) = wait_for_port(&args.server_ssh, args.port, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_server_container(args, framework);
        std::process::exit(1);
    }

    // 2. Render template and SCP to client.
    let raw = fs::read_to_string(config_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", config_path.display()));
    let server_addr = format!("{}:{}", args.server_private, args.port);
    let rendered = render_template(&raw, &server_addr, args.duration, args.warmup);

    let local_tmp = std::env::temp_dir().join(format!("{cfg_name}.toml"));
    fs::write(&local_tmp, &rendered).expect("failed to write temp TOML");
    let remote_config = format!("/tmp/{cfg_name}.toml");
    scp_to_client(args, &local_tmp, &remote_config);
    let _ = fs::remove_file(&local_tmp);

    // 3. Start perf/monitors.
    let server_pid = container_pid(args, framework);
    if args.os_monitors {
        start_host_monitors(args, &key, server_pid);
        thread::sleep(Duration::from_secs(MONITOR_MARGIN_SECS as u64));
    }

    if args.perf_enabled {
        match server_pid {
            Some(pid) => {
                if perf_stat_enabled(args) {
                    start_server_perf_stat(args, &key, pid);
                }
                if perf_record_enabled(args) {
                    start_server_perf_record(args, &key, pid);
                }
            }
            None => eprintln!("    warning: failed to determine server PID for perf capture"),
        }
    }

    // 4. Run spinr bench.
    let outfile = args.results_dir.join(format!("{key}.json"));
    println!("  [{key}] -> spinr bench {remote_config}");
    let started_at_utc = chrono_lite_utc();
    if let Some(v) = run_bench(args, &key, &remote_config, &outfile) {
        results.insert(key.clone(), v);
    }
    let completed_at_utc = chrono_lite_utc();

    if args.os_monitors {
        thread::sleep(Duration::from_secs(MONITOR_MARGIN_SECS as u64));
    }

    // 5. Collect artifacts and stop.
    collect_run_artifacts(args, framework, &key);
    write_run_meta(
        args,
        &key,
        framework,
        &cfg_name,
        config_path,
        &started_at_utc,
        &completed_at_utc,
    );
    collect_docker_stats(args, &key);
    collect_docker_logs(args, framework, &key);
    stop_server_container(args, framework);
    cleanup_remote_file(args, RemoteSide::Client, &remote_config);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn generate_report(args: &Args) {
    harrow_bench::perf_summary::render_results_dir(&args.results_dir).unwrap();
    let report_path = args.results_dir.join("summary.md");
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

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

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

    if args.perf_enabled {
        let out = ssh_run(
            &args.ssh_user,
            &args.server_ssh,
            "command -v perf >/dev/null && command -v doas >/dev/null && doas -n true >/dev/null 2>&1 && echo ok",
        );
        match out {
            Ok(o) if o.status.success() => println!("  perf on server via doas: ok"),
            _ => {
                eprintln!(
                    "  perf on server ({}): MISSING or doas not usable",
                    args.server_ssh
                );
                std::process::exit(1);
            }
        }

        if client_perf_enabled(args) {
            let out = ssh_run(
                &args.ssh_user,
                &args.client_ssh,
                "command -v perf >/dev/null && command -v doas >/dev/null && doas -n true >/dev/null 2>&1 && echo ok",
            );
            match out {
                Ok(o) if o.status.success() => println!("  perf on client via doas: ok"),
                _ => {
                    eprintln!(
                        "  perf on client ({}): MISSING or doas not usable",
                        args.client_ssh
                    );
                    std::process::exit(1);
                }
            }
        } else {
            println!("  perf on client: skipped (spinr-mode=docker)");
        }

        if perf_record_enabled(args) {
            for side in [RemoteSide::Server, RemoteSide::Client] {
                if side == RemoteSide::Client && !client_perf_enabled(args) {
                    continue;
                }
                if inferno_available(args, side) {
                    println!("  inferno tools on {}: ok", side.label());
                } else {
                    eprintln!(
                        "  WARNING: inferno tools on {}: MISSING — runner will keep raw perf.data on the node and only collect perf-report.txt",
                        side.label()
                    );
                }
            }
        }
    }

    println!("--- Preflight checks passed ---");
    println!();
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args = parse_args();
    fs::create_dir_all(&args.results_dir).unwrap();

    preflight_checks(&args);

    let config_names: Vec<String> = args.config_paths.iter().map(|p| config_name(p)).collect();

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
    println!(" Perf: {}", perf_scope_label(&args));
    println!(" Configs: {}", config_names.join(", "));
    println!(" Results: {}/", args.results_dir.display());
    println!("============================================");
    println!();

    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    for config_path in &args.config_paths {
        let cfg_name = config_name(config_path);
        let is_session = is_session_config(config_path);

        println!(
            "========== CONFIG: {}{} ==========",
            cfg_name,
            if is_session { " (session)" } else { "" }
        );

        let frameworks: Vec<Framework> = if is_session {
            vec![Framework::Harrow]
        } else {
            vec![Framework::Harrow, Framework::Axum]
        };

        for framework in &frameworks {
            run_config(&args, config_path, *framework, &mut results);
            thread::sleep(SLEEP_BETWEEN_RUNS);
        }
    }

    println!();
    println!("========== GENERATING SUMMARY ==========");
    generate_report(&args);
    println!();
    println!("Done! Results in {}/", args.results_dir.display());
}
