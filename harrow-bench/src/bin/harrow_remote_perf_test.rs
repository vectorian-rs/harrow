//! Unified direct performance test orchestrator.
//!
//! This is the one-shot runner used by the `bench:baseline:*` and
//! `bench:perf:*` mise tasks. It is intentionally narrower than the
//! registry/suite-driven `bench-single` and `bench-compare` binaries.
//!
//! Supports both local (single-node) and remote (multi-node) deployments,
//! using the spinr load generator.
//!
//! # Modes
//!
//! ## Local Mode (Single Node)
//! Server and load generator run on the same machine (localhost).
//! Useful for quick local testing without SSH or cloud resources.
//!
//! ## Remote Mode (Multi Node)
//! Server runs on one node, load generator on another, orchestrated via SSH.
//! Designed for EC2 benchmarking with isolated client/server resources.
//!
//! # Load Generator
//!
//! ## Spinr
//! Custom Rust load generator with advanced features.
//! Requires TOML config files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use harrow_bench::harness::ops;
use harrow_bench::harness::spec::DeploymentMode;
use serde_json::Value;

const DEFAULT_PORT: u16 = 3090;
const SSH_USER: &str = "alpine";
const DEFAULT_SPINR_BIN: &str = "/usr/local/bin/spinr";
const SLEEP_BETWEEN_RUNS: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Test Target Definition
// ---------------------------------------------------------------------------

/// A test target — a spinr TOML config
#[derive(Clone, Debug)]
struct TestTarget {
    path: PathBuf,
}

impl TestTarget {
    fn name(&self) -> String {
        config_name(&self.path)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn is_session_config(&self) -> bool {
        is_session_config_path(&self.path)
    }

    fn is_compression_config(&self) -> bool {
        is_compression_config_path(&self.path)
    }
}

struct BenchRunResult {
    error: Option<String>,
}

impl BenchRunResult {
    fn success() -> Self {
        Self { error: None }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Existing Enums (preserved for backward compatibility)
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Allocator {
    Mimalloc,
    System,
}

impl Allocator {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "mimalloc" => Some(Self::Mimalloc),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Mimalloc => "mimalloc",
            Self::System => "system",
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Mimalloc => "",
            Self::System => "-sysalloc",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Framework {
    Harrow,
    Axum,
    Ntex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    /// Tokio - standard async runtime (cross-platform)
    Tokio,
    /// Monoio - io_uring-based runtime (Linux 6.1+ only)
    Monoio,
}

impl Backend {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "tokio" => Some(Self::Tokio),
            "monoio" => Some(Self::Monoio),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Tokio => "tokio",
            Self::Monoio => "monoio",
        }
    }
}

impl Framework {
    fn as_str(self) -> &'static str {
        match self {
            Self::Harrow => "harrow",
            Self::Axum => "axum",
            Self::Ntex => "ntex",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "harrow" => Some(Self::Harrow),
            "axum" => Some(Self::Axum),
            "ntex" => Some(Self::Ntex),
            _ => None,
        }
    }

    fn image(self, backend: Backend, alloc: Allocator) -> String {
        match (self, backend) {
            (Self::Harrow, Backend::Monoio) => "harrow-server-monoio".to_string(),
            (Self::Harrow, Backend::Tokio) => {
                let suffix = alloc.suffix();
                format!("harrow-perf-server{suffix}")
            }
            (Self::Axum, _) => {
                let suffix = alloc.suffix();
                format!("axum-perf-server{suffix}")
            }
            (Self::Ntex, _) => {
                let suffix = alloc.suffix();
                format!("ntex-perf-server{suffix}")
            }
        }
    }

    fn container_name(self, backend: Backend, alloc: Allocator) -> String {
        match (self, backend) {
            (Self::Harrow, Backend::Monoio) => "harrow-server-monoio".to_string(),
            (Self::Harrow, Backend::Tokio) => {
                let suffix = alloc.suffix();
                format!("harrow-perf-server{suffix}")
            }
            (Self::Axum, _) => {
                let suffix = alloc.suffix();
                format!("axum-perf-server{suffix}")
            }
            (Self::Ntex, _) => {
                let suffix = alloc.suffix();
                format!("ntex-perf-server{suffix}")
            }
        }
    }

    fn launch_cmd(self, backend: Backend, port: u16, extra_flags: &str) -> String {
        let base = match (self, backend) {
            (Self::Harrow, Backend::Monoio) => {
                format!("/harrow-server-monoio --bind 0.0.0.0 --port {port}")
            }
            (Self::Harrow, Backend::Tokio) => {
                format!("/harrow-perf-server --bind 0.0.0.0 --port {port}")
            }
            (Self::Axum, _) => format!("/axum-perf-server --bind 0.0.0.0 --port {port}"),
            (Self::Ntex, _) => format!("/ntex-perf-server --bind 0.0.0.0 --port {port}"),
        };
        if extra_flags.is_empty() {
            base
        } else {
            format!("{base} {extra_flags}")
        }
    }

    fn supports_backend(self, backend: Backend) -> bool {
        match (self, backend) {
            (Self::Harrow, _) => true,
            (Self::Axum, Backend::Monoio) => false,
            (Self::Axum, Backend::Tokio) => true,
            (Self::Ntex, Backend::Monoio) => false,
            (Self::Ntex, Backend::Tokio) => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompareMode {
    Framework,
    Allocator,
}

impl CompareMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "framework" => Some(Self::Framework),
            "allocator" => Some(Self::Allocator),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Framework => "framework",
            Self::Allocator => "allocator",
        }
    }
}

struct Variant {
    label: String,
    framework: Framework,
    allocator: Allocator,
}

// ---------------------------------------------------------------------------
// Unified Args Structure
// ---------------------------------------------------------------------------

struct Args {
    // Mode selection
    deployment_mode: DeploymentMode,

    // Connection settings (remote mode requires SSH, local mode uses server_url)
    server_ssh: Option<String>,
    client_ssh: Option<String>,
    server_private: Option<String>,
    server_url: Option<String>, // For local mode
    ssh_user: String,

    // Instance and test configuration
    instance_type: Option<String>, // Required for remote, optional for local
    port: u16,
    duration: u32,
    warmup: u32,
    results_dir: PathBuf,

    // Test targets
    config_paths: Vec<PathBuf>,

    // Server configuration
    server_flags: Option<String>,

    // Telemetry options
    os_monitors: bool,
    perf_enabled: bool,
    perf_mode: PerfMode,
    spinr_mode: SpinrMode,

    // Comparison options
    allocator: Allocator,
    compare: CompareMode,
    framework: Framework,
    backend: Backend,
}

fn client_perf_enabled(args: &Args) -> bool {
    args.perf_enabled && args.spinr_mode == SpinrMode::Host
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
        "Usage: harrow-remote-perf-test [MODE] [OPTIONS]\n\
         \n\
         MODE (required):\n\
         \x20 --mode MODE            Deployment mode: local|remote\n\
         \n\
         LOCAL MODE OPTIONS:\n\
         \x20 --server-url URL       Server URL (default: http://localhost:PORT)\n\
         \n\
         REMOTE MODE OPTIONS:\n\
         \x20 --server-ssh IP        Server public IP (for SSH)\n\
         \x20 --client-ssh IP        Client public IP (for SSH)\n\
         \x20 --server-private IP    Server private IP (for bench URLs over VPC)\n\
         \x20 --instance-type TYPE   EC2 instance type (e.g. c8g.12xlarge)\n\
         \x20 --ssh-user USER        SSH user (default: alpine)\n\
         \n\
         SPINR OPTIONS:\n\
         \x20 --config PATH          Spinr TOML template (repeatable)\n\
         \x20 --spinr-mode MODE      Client mode: docker|host (default: docker)\n\
         \n\
         COMMON OPTIONS:\n\
         \x20 --port PORT            Server port (default: 3090)\n\
         \x20 --duration SECS        Test duration in seconds (default: 60)\n\
         \x20 --warmup SECS          Warmup duration in seconds (default: 5)\n\
         \x20 --results-dir DIR      Override output directory\n\
         \x20 --server-flags FLAGS   Extra flags for harrow-perf-server\n\
         \x20 --os-monitors          Collect vmstat/sar/iostat/pidstat\n\
         \x20 --perf                 Collect perf artifacts (default mode: stat)\n\
         \x20 --perf-mode MODE       Perf mode: stat|record|both\n\
         \x20 --allocator ALLOC      Allocator: mimalloc|system (default: mimalloc)\n\
         \x20 --compare MODE         Comparison mode: framework|allocator\n\
         \x20 --framework FW         Framework: harrow|axum|ntex (default: harrow)\n\
         \x20 --backend BACKEND      Runtime backend: tokio|monoio (default: tokio, harrow only)\n\
         \n\
         EXAMPLES:\n\
         \n\
         # Local Spinr test\n\
         harrow-remote-perf-test --mode local \\\\n\
             --server-url http://localhost:3090 --config spinr/text-c128.toml\n\
         \n\
         # Remote Spinr test\n\
         harrow-remote-perf-test --mode remote \\\\n\
             --server-ssh 10.0.1.10 --client-ssh 10.0.1.20 --server-private 10.0.1.10 \\\\n\
             --instance-type c8g.12xlarge --config spinr/text-c128.toml\n"
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();

    // Mode (required)
    let mut deployment_mode: Option<DeploymentMode> = None;

    // Connection settings
    let mut server_ssh: Option<String> = None;
    let mut client_ssh: Option<String> = None;
    let mut server_private: Option<String> = None;
    let mut server_url: Option<String> = None;
    let mut ssh_user = SSH_USER.to_string();

    // Instance and test configuration
    let mut instance_type: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut duration: u32 = 60;
    let mut warmup: u32 = 5;
    let mut results_dir_override: Option<PathBuf> = None;

    // Test targets
    let mut config_paths: Vec<PathBuf> = Vec::new();

    // Server configuration
    let mut server_flags: Option<String> = None;

    // Telemetry options
    let mut os_monitors = false;
    let mut perf_enabled = false;
    let mut perf_mode = PerfMode::Stat;
    let mut spinr_mode = SpinrMode::Docker;

    // Comparison options
    let mut allocator = Allocator::Mimalloc;
    let mut compare = CompareMode::Framework;
    let mut framework = Framework::Harrow;
    let mut backend = Backend::Tokio;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            // Mode
            "--mode" => {
                deployment_mode = Some(DeploymentMode::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --mode: {}", args[i + 1]);
                    usage();
                }));
                i += 2;
            }
            // Connection settings
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
            "--server-url" => {
                server_url = Some(args[i + 1].clone());
                i += 2;
            }
            "--ssh-user" => {
                ssh_user = args[i + 1].clone();
                i += 2;
            }
            // Instance and test configuration
            "--instance-type" => {
                instance_type = Some(args[i + 1].clone());
                i += 2;
            }
            "--port" => {
                port = Some(args[i + 1].parse().expect("invalid --port"));
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
            // Test targets
            "--config" => {
                config_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            // Server configuration
            "--server-flags" => {
                server_flags = Some(args[i + 1].clone());
                i += 2;
            }
            // Telemetry options
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
            // Comparison options
            "--allocator" => {
                allocator = Allocator::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --allocator: {}", args[i + 1]);
                    usage();
                });
                i += 2;
            }
            "--compare" => {
                compare = CompareMode::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --compare: {}", args[i + 1]);
                    usage();
                });
                i += 2;
            }
            "--framework" => {
                framework = Framework::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --framework: {}", args[i + 1]);
                    usage();
                });
                i += 2;
            }
            "--backend" => {
                backend = Backend::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --backend: {}", args[i + 1]);
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

    // Validate required mode
    let deployment_mode = deployment_mode.unwrap_or_else(|| {
        eprintln!("error: --mode is required (local|remote)");
        usage();
    });

    // Validate test targets
    if config_paths.is_empty() {
        eprintln!("error: at least one --config is required");
        usage();
    }

    // Validate mode-specific requirements
    match deployment_mode {
        DeploymentMode::Local => {
            if server_ssh.is_some() || client_ssh.is_some() || server_private.is_some() {
                eprintln!("warning: SSH options are ignored in local mode");
            }
            if instance_type.is_none() {
                instance_type = Some("local".to_string());
            }
        }
        DeploymentMode::Remote => {
            if server_ssh.is_none() || client_ssh.is_none() || server_private.is_none() {
                eprintln!(
                    "error: --server-ssh, --client-ssh, and --server-private are required for remote mode"
                );
                usage();
            }
            if instance_type.is_none() {
                eprintln!("error: --instance-type is required for remote mode");
                usage();
            }
        }
    }

    // Validate backend/framework compatibility
    if !framework.supports_backend(backend) {
        eprintln!(
            "error: framework '{}' does not support backend '{}'",
            framework.as_str(),
            backend.as_str()
        );
        eprintln!(
            "note: Axum only supports Tokio backend; use --backend tokio or --framework harrow"
        );
        usage();
    }

    // Verify all config files exist
    for p in &config_paths {
        if !p.exists() {
            eprintln!("error: file not found: {}", p.display());
            std::process::exit(1);
        }
    }

    // Set default port
    let port = port.unwrap_or(DEFAULT_PORT);

    // Set default server URL for local mode
    let server_url = server_url.unwrap_or_else(|| format!("http://localhost:{port}"));

    let results_dir = results_dir_override.unwrap_or_else(|| {
        let ts = ops::timestamp_slug();
        let instance = instance_type.as_deref().unwrap_or("unknown");
        PathBuf::from(format!("perf/{instance}/{ts}"))
    });

    Args {
        deployment_mode,
        server_ssh,
        client_ssh,
        server_private,
        server_url: Some(server_url),
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
        allocator,
        compare,
        framework,
        backend,
    }
}

// ---------------------------------------------------------------------------
// Helper Functions
// ---------------------------------------------------------------------------

fn render_template(raw: &str, server: &str, duration: u32, warmup: u32) -> String {
    let base_url = if server.starts_with("http") {
        server.to_string()
    } else {
        format!("http://{server}")
    };
    raw.replace("{{ base_url }}", &base_url)
        .replace("{{ server }}", server)
        .replace("{{ duration_secs }}", &duration.to_string())
        .replace("{{ duration }}", &duration.to_string())
        .replace("{{ warmup_secs }}", &warmup.to_string())
        .replace("{{ warmup }}", &warmup.to_string())
}

fn is_session_config_path(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("session-"))
}

fn is_compression_config_path(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("compression-"))
}

fn config_name(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

fn comparison_variants(args: &Args) -> Vec<Variant> {
    match args.compare {
        CompareMode::Framework => vec![
            Variant {
                label: "harrow".into(),
                framework: Framework::Harrow,
                allocator: args.allocator,
            },
            Variant {
                label: "ntex".into(),
                framework: Framework::Ntex,
                allocator: args.allocator,
            },
        ],
        CompareMode::Allocator => vec![
            Variant {
                label: "mimalloc".into(),
                framework: args.framework,
                allocator: Allocator::Mimalloc,
            },
            Variant {
                label: "system".into(),
                framework: args.framework,
                allocator: Allocator::System,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// SSH Helpers (for Remote Mode)
// ---------------------------------------------------------------------------

fn ssh_server(args: &Args, remote_cmd: &str) -> std::io::Result<Output> {
    ops::ssh_run(
        &args.ssh_user,
        args.server_ssh.as_deref().unwrap(),
        remote_cmd,
    )
}

fn ssh_client(args: &Args, remote_cmd: &str) -> std::io::Result<Output> {
    ops::ssh_run(
        &args.ssh_user,
        args.client_ssh.as_deref().unwrap(),
        remote_cmd,
    )
}

fn scp_to_client(args: &Args, local_path: &Path, remote_path: &str) {
    ops::scp_to_remote(
        &args.ssh_user,
        args.client_ssh.as_deref().unwrap(),
        local_path,
        remote_path,
    );
}

// ---------------------------------------------------------------------------
// Server Container Management
// ---------------------------------------------------------------------------

fn start_server_container(
    args: &Args,
    framework: Framework,
    allocator: Allocator,
    server_flags: &str,
) {
    let name = framework.container_name(args.backend, allocator);
    let image = framework.image(args.backend, allocator);
    let command = framework.launch_cmd(args.backend, args.port, server_flags);

    println!(
        ">>> Starting {} server on {}",
        framework.as_str(),
        args.deployment_mode.as_str()
    );

    // io_uring requires unconfined seccomp (Docker blocks io_uring syscalls by default)
    let seccomp = if args.backend == Backend::Monoio {
        " --security-opt seccomp=unconfined"
    } else {
        ""
    };

    match args.deployment_mode {
        DeploymentMode::Local => {
            // Stop any existing container
            let _ = ops::run_local(&format!("docker rm -f {name} 2>/dev/null || true"));
            let docker_cmd = format!(
                "docker run -d --name {name} -p {0}:{0} --ulimit nofile=65535:65535{seccomp} {image} {command}",
                args.port
            );
            match ops::run_local(&docker_cmd) {
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
        }
        DeploymentMode::Remote => {
            let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
            let docker_cmd = format!(
                "docker run -d --name {name} --network host --ulimit nofile=65535:65535{seccomp} {image} {command}"
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
        }
    }
    thread::sleep(Duration::from_secs(2));
}

fn stop_server_container(args: &Args, framework: Framework, allocator: Allocator) {
    let name = framework.container_name(args.backend, allocator);
    println!(">>> Stopping {} server", framework.as_str());

    match args.deployment_mode {
        DeploymentMode::Local => {
            let _ = ops::run_local(&format!("docker rm -f {name} 2>/dev/null || true"));
        }
        DeploymentMode::Remote => {
            let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
        }
    }
}

fn wait_for_server(args: &Args, timeout: Duration) -> Result<(), String> {
    let (host, port) = match args.deployment_mode {
        DeploymentMode::Local => ("localhost", args.port),
        DeploymentMode::Remote => (args.server_ssh.as_deref().unwrap(), args.port),
    };

    let deadline = Instant::now() + timeout;
    let addr = format!("{host}:{port}");
    println!("    Waiting for {addr}...");

    while Instant::now() < deadline {
        if ops::http_health_check(host, port, "/health") {
            println!("    Health endpoint passed");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "server on {addr} did not pass GET /health within {timeout:?}"
    ))
}

// ---------------------------------------------------------------------------
// Load Generator Implementations
// ---------------------------------------------------------------------------

/// Run a spinr benchmark
fn run_spinr_bench(
    args: &Args,
    key: &str,
    config_path: &Path,
    rendered_config: &str,
    outfile: &Path,
) -> BenchRunResult {
    let spinr_cmd = format!("{DEFAULT_SPINR_BIN} bench {} -j", config_path.display());

    match args.deployment_mode {
        DeploymentMode::Local => {
            // Write config to temp file locally
            let local_tmp = std::env::temp_dir().join(format!("{key}.toml"));
            let _ = fs::write(&local_tmp, rendered_config);

            let cmd = match args.spinr_mode {
                SpinrMode::Docker => format!(
                    "docker run --rm --network host --ulimit nofile=65535:65535 \\
                     -v {}:/bench.toml spinr bench /bench.toml -j",
                    local_tmp.display()
                ),
                SpinrMode::Host => spinr_cmd.replace(
                    &*config_path.to_string_lossy(),
                    &local_tmp.to_string_lossy(),
                ),
            };

            let result = ops::run_local(&cmd);
            let _ = fs::remove_file(&local_tmp);

            match result {
                Ok(o) if o.status.success() => {
                    let _ = fs::write(outfile, &o.stdout);
                    let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
                    if let Some(ref v) = val {
                        let metrics = ops::spinr_metrics(v);
                        let validation = ops::validate_spinr_metrics(metrics);
                        let success_rate = ops::validation_success_rate(metrics);
                        println!(
                            "    -> rps={} p99={}ms success={:.1}%",
                            ops::val_str(metrics, "rps"),
                            ops::val_str(metrics, "latency_p99_ms"),
                            success_rate * 100.0
                        );
                        return match validation {
                            Ok(()) => BenchRunResult::success(),
                            Err(error) => BenchRunResult::failure(error),
                        };
                    }
                    BenchRunResult::failure("spinr returned non-JSON output")
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    eprintln!("    bench failed (exit {}): {}", o.status, stderr.trim());
                    BenchRunResult::failure("spinr benchmark command failed")
                }
                Err(e) => {
                    eprintln!("    failed to run bench: {e}");
                    BenchRunResult::failure(format!("failed to run spinr benchmark: {e}"))
                }
            }
        }
        DeploymentMode::Remote => {
            // Remote mode - upload config and run via SSH
            let local_tmp = std::env::temp_dir().join(format!("{key}.toml"));
            let _ = fs::write(&local_tmp, rendered_config);
            let remote_config = format!("/tmp/{key}.toml");
            scp_to_client(args, &local_tmp, &remote_config);
            let _ = fs::remove_file(&local_tmp);

            let remote_cmd = match args.spinr_mode {
                SpinrMode::Docker => format!(
                    "docker run --rm --network host --ulimit nofile=65535:65535 \\
                     -v {remote_config}:/bench.toml spinr bench /bench.toml -j"
                ),
                SpinrMode::Host => format!("{DEFAULT_SPINR_BIN} bench {remote_config} -j"),
            };

            match ssh_client(args, &remote_cmd) {
                Ok(o) if o.status.success() => {
                    let _ = fs::write(outfile, &o.stdout);
                    let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
                    if let Some(ref v) = val {
                        let metrics = ops::spinr_metrics(v);
                        let validation = ops::validate_spinr_metrics(metrics);
                        let success_rate = ops::validation_success_rate(metrics);
                        println!(
                            "    -> rps={} p99={}ms success={:.1}%",
                            ops::val_str(metrics, "rps"),
                            ops::val_str(metrics, "latency_p99_ms"),
                            success_rate * 100.0
                        );
                        return match validation {
                            Ok(()) => BenchRunResult::success(),
                            Err(error) => BenchRunResult::failure(error),
                        };
                    }
                    BenchRunResult::failure("spinr returned non-JSON output")
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    eprintln!("    bench failed (exit {}): {}", o.status, stderr.trim());
                    BenchRunResult::failure("spinr benchmark command failed")
                }
                Err(e) => {
                    eprintln!("    failed to run bench: {e}");
                    BenchRunResult::failure(format!("failed to run spinr benchmark: {e}"))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Artifact Collection
// ---------------------------------------------------------------------------

fn run_key(variant_label: &str, cfg_name: &str) -> String {
    format!("{variant_label}_{cfg_name}")
}

fn cleanup_remote_file(args: &Args, remote_path: &str) {
    let cmd = format!("rm -f {remote_path}");
    match args.deployment_mode {
        DeploymentMode::Local => {
            let _ = ops::run_local(&cmd);
        }
        DeploymentMode::Remote => {
            let _ = ssh_client(args, &cmd);
        }
    }
}

fn write_run_meta(
    args: &Args,
    key: &str,
    variant: &Variant,
    cfg_name: &str,
    config_path: &Path,
    started_at_utc: &str,
    completed_at_utc: &str,
) {
    let meta = serde_json::json!({
        "key": key,
        "variant": variant.label,
        "compare_mode": args.compare.as_str(),
        "framework": variant.framework.as_str(),
        "allocator": variant.allocator.as_str(),
        "test_case": cfg_name,
        "path": config_path.display().to_string(),
        "warmup_secs": args.warmup,
        "duration_secs": args.duration,
        "server_host": args.server_ssh.as_deref().unwrap_or("localhost"),
        "client_host": args.client_ssh.as_deref().unwrap_or("localhost"),
        "deployment_mode": args.deployment_mode.as_str(),
        "load_generator": "spinr",
        "spinr_mode": args.spinr_mode.as_str(),
        "os_monitors": args.os_monitors,
        "perf_enabled": args.perf_enabled,
        "perf_mode": args.perf_mode.as_str(),
        "perf_scope": perf_scope_label(args),
        "started_at_utc": started_at_utc,
        "completed_at_utc": completed_at_utc,
    });
    let path = args.results_dir.join(format!("{key}.meta.json"));
    let _ = fs::write(path, serde_json::to_vec_pretty(&meta).unwrap());
}

fn collect_docker_stats(args: &Args, key: &str) {
    let cmd =
        "docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}'";

    let result = match args.deployment_mode {
        DeploymentMode::Local => ops::run_local(cmd),
        DeploymentMode::Remote => ssh_server(args, cmd),
    };

    if let Ok(out) = result {
        let path = args.results_dir.join(format!("stats_{key}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

fn collect_docker_logs(args: &Args, framework: Framework, allocator: Allocator, key: &str) {
    let name = framework.container_name(args.backend, allocator);
    let cmd = format!("docker logs {name} 2>&1");

    let result = match args.deployment_mode {
        DeploymentMode::Local => ops::run_local(&cmd),
        DeploymentMode::Remote => ssh_server(args, &cmd),
    };

    if let Ok(out) = result {
        let path = args.results_dir.join(format!("logs_{key}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

// ---------------------------------------------------------------------------
// Perf / Strace Capture
// ---------------------------------------------------------------------------

/// Get the host-namespace PID of the server container's main process.
fn get_container_pid(args: &Args, container_name: &str) -> Option<u32> {
    let cmd = format!("docker inspect --format '{{{{.State.Pid}}}}' {container_name}");
    let result = match args.deployment_mode {
        DeploymentMode::Local => ops::run_local(&cmd),
        DeploymentMode::Remote => ssh_server(args, &cmd),
    };
    result
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
}

/// Start perf record, perf stat, and strace on the server.
///
/// Each tool runs as a background process. PID files are written to /tmp/
/// so we can reliably stop them later. Returns whether at least perf record
/// started successfully.
fn start_profilers(args: &Args, server_pid: u32, perf_mode: PerfMode) -> bool {
    if args.deployment_mode == DeploymentMode::Local {
        eprintln!("  perf capture not supported in local mode");
        return false;
    }

    println!("  Starting profilers on server (target PID {server_pid})...");

    // Clean up any leftover artifacts/PIDs from previous runs.
    let _ = ssh_server(
        args,
        "doas rm -f /tmp/perf.data /tmp/perf-stat.txt /tmp/strace.txt \
         /tmp/perf-record.pid /tmp/perf-stat.pid /tmp/strace.pid",
    );

    let mut ok = true;

    // perf record — CPU call stacks for flamegraph generation.
    if matches!(perf_mode, PerfMode::Record | PerfMode::Both) {
        let cmd = format!(
            "doas sh -c 'perf record -g -F 99 -p {server_pid} -o /tmp/perf.data \
             </dev/null >/dev/null 2>&1 & echo $! > /tmp/perf-record.pid'"
        );
        match ssh_server(args, &cmd) {
            Ok(o) if o.status.success() => println!("    perf record: started"),
            _ => {
                eprintln!("    perf record: FAILED to start");
                ok = false;
            }
        }
    }

    // perf stat — software event counters.
    if matches!(perf_mode, PerfMode::Stat | PerfMode::Both) {
        let cmd = format!(
            "doas sh -c 'perf stat -e task-clock,context-switches,cpu-migrations,page-faults \
             -p {server_pid} -o /tmp/perf-stat.txt \
             </dev/null >/dev/null 2>&1 & echo $! > /tmp/perf-stat.pid'"
        );
        match ssh_server(args, &cmd) {
            Ok(o) if o.status.success() => println!("    perf stat:   started"),
            _ => eprintln!("    perf stat:   FAILED to start"),
        }
    }

    // strace — syscall breakdown.
    let cmd = format!(
        "doas sh -c 'strace -c -f -p {server_pid} \
         </dev/null >/dev/null 2>/tmp/strace.txt & echo $! > /tmp/strace.pid'"
    );
    match ssh_server(args, &cmd) {
        Ok(o) if o.status.success() => println!("    strace:      started"),
        _ => eprintln!("    strace:      FAILED to start"),
    }

    // Verify at least one profiler is running.
    thread::sleep(Duration::from_millis(500));
    let verify = ssh_server(
        args,
        "for f in /tmp/perf-record.pid /tmp/perf-stat.pid /tmp/strace.pid; do \
         [ -f $f ] && kill -0 $(cat $f) 2>/dev/null && echo \"$(basename $f .pid): running\"; \
         done",
    );
    if let Ok(o) = verify {
        let out = String::from_utf8_lossy(&o.stdout);
        for line in out.lines() {
            println!("    {line}");
        }
    }

    ok
}

/// Stop profilers, collapse perf data on server, and download only
/// the minimal artifacts needed for analysis:
///
/// - **collapsed stacks** (~100KB) instead of raw perf.data (100s MB)
/// - **perf-stat.txt** (software counters, <1KB)
/// - **strace.txt** (syscall summary, <2KB)
///
/// The server runs `perf script | stackcollapse-perf.pl` to produce
/// the collapsed stacks. If stackcollapse-perf.pl is not available,
/// falls back to a simple `perf script` download.
fn stop_and_collect_profilers(args: &Args, key: &str) {
    if args.deployment_mode == DeploymentMode::Local {
        return;
    }

    println!("  Stopping profilers...");

    // Send SIGINT to perf (triggers clean shutdown + data flush).
    // Send SIGINT to strace (prints summary to stderr → /tmp/strace.txt).
    let _ = ssh_server(
        args,
        "for f in /tmp/perf-record.pid /tmp/perf-stat.pid /tmp/strace.pid; do \
         [ -f $f ] && doas kill -INT $(cat $f) 2>/dev/null; done",
    );

    // Give profilers time to flush data.
    thread::sleep(Duration::from_secs(3));

    // Force-kill anything still running.
    let _ = ssh_server(
        args,
        "for f in /tmp/perf-record.pid /tmp/perf-stat.pid /tmp/strace.pid; do \
         [ -f $f ] && doas kill -9 $(cat $f) 2>/dev/null; done",
    );

    // Collapse perf data on the server to avoid transferring raw perf.data.
    // Raw perf.data can be 100s of MB; collapsed stacks are ~100KB.
    println!("  Collapsing perf data on server...");
    let collapse_result = ssh_server(
        args,
        "if [ -f /tmp/perf.data ]; then \
           doas perf script -i /tmp/perf.data 2>/dev/null | \
           stackcollapse-perf.pl --all 2>/dev/null > /tmp/collapsed.txt; \
           if [ ! -s /tmp/collapsed.txt ]; then \
             doas perf script -i /tmp/perf.data 2>/dev/null > /tmp/perf-script.txt; \
           fi; \
         fi",
    );

    if let Ok(o) = &collapse_result
        && !o.status.success()
    {
        let stderr = String::from_utf8_lossy(&o.stderr);
        eprintln!("    warning: collapse failed: {}", stderr.trim());
    }

    // Collect only the compact artifacts.
    let ssh_user = &args.ssh_user;
    let host = args.server_ssh.as_deref().unwrap();

    let artifacts = [
        ("/tmp/collapsed.txt", format!("collapsed_{key}.txt")),
        ("/tmp/perf-script.txt", format!("perf-script_{key}.txt")),
        ("/tmp/perf-stat.txt", format!("perf-stat_{key}.txt")),
        ("/tmp/strace.txt", format!("strace_{key}.txt")),
    ];

    for (remote, local_name) in &artifacts {
        let local_path = args.results_dir.join(local_name);
        if ops::scp_from_remote(ssh_user, host, remote, &local_path) {
            let size = fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
            if size > 0 {
                println!("    {local_name}: {size} bytes");
            }
        }
    }

    // Generate flamegraph SVG locally from collapsed stacks.
    let collapsed_path = args.results_dir.join(format!("collapsed_{key}.txt"));
    if collapsed_path.exists() && fs::metadata(&collapsed_path).map(|m| m.len()).unwrap_or(0) > 0 {
        let svg_path = args.results_dir.join(format!("flamegraph_{key}.svg"));
        let result = std::process::Command::new("flamegraph.pl")
            .arg("--title")
            .arg(key)
            .arg(&collapsed_path)
            .stdout(std::process::Stdio::from(
                fs::File::create(&svg_path).unwrap(),
            ))
            .stderr(std::process::Stdio::piped())
            .status();
        match result {
            Ok(s) if s.success() => {
                let size = fs::metadata(&svg_path).map(|m| m.len()).unwrap_or(0);
                println!("    flamegraph_{key}.svg: {size} bytes");
            }
            _ => {
                eprintln!(
                    "    warning: flamegraph.pl not found or failed. \
                     Generate manually: flamegraph.pl {} > flamegraph.svg",
                    collapsed_path.display()
                );
            }
        }
    }

    // Cleanup remote temp files.
    let _ = ssh_server(
        args,
        "doas rm -f /tmp/perf.data /tmp/perf-stat.txt /tmp/strace.txt \
         /tmp/collapsed.txt /tmp/perf-script.txt \
         /tmp/perf-record.pid /tmp/perf-stat.pid /tmp/strace.pid",
    );
}

// ---------------------------------------------------------------------------
// Per-Config Run
// ---------------------------------------------------------------------------

fn run_config(args: &Args, target: &TestTarget, variant: &Variant) {
    let cfg_name = target.name();
    let key = run_key(&variant.label, &cfg_name);
    let framework = variant.framework;
    let allocator = variant.allocator;

    // Server flags
    let server_flags = {
        let mut parts = Vec::new();
        if target.is_session_config() && framework == Framework::Harrow {
            parts.push("--session".to_string());
        }
        if target.is_compression_config() {
            parts.push("--compression".to_string());
        }
        if framework == Framework::Harrow
            && let Some(ref flags) = args.server_flags
        {
            parts.push(flags.clone());
        }
        parts.join(" ")
    };

    println!();
    println!("--- {} / {} ---", variant.label, cfg_name);

    // 1. Start server
    start_server_container(args, framework, allocator, &server_flags);
    if let Err(e) = wait_for_server(args, Duration::from_secs(30)) {
        eprintln!("  {e}");
        stop_server_container(args, framework, allocator);
        std::process::exit(1);
    }

    // 2. Start profilers (if --perf enabled)
    let container_name = framework.container_name(args.backend, allocator);
    if args.perf_enabled {
        if let Some(pid) = get_container_pid(args, &container_name) {
            start_profilers(args, pid, args.perf_mode);
            // Let profilers settle before load starts.
            thread::sleep(Duration::from_secs(2));
        } else {
            eprintln!("  warning: could not get container PID, skipping profilers");
        }
    }

    // 3. Run the benchmark
    let outfile = args.results_dir.join(format!("{key}.json"));
    let started_at_utc = ops::chrono_lite_utc();

    let result = {
        let path = target.path();
        // Render template
        let raw = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let server_addr = match args.deployment_mode {
            DeploymentMode::Local => format!("localhost:{}", args.port),
            DeploymentMode::Remote => {
                format!("{}:{}", args.server_private.as_deref().unwrap(), args.port)
            }
        };
        let rendered = render_template(&raw, &server_addr, args.duration, args.warmup);

        run_spinr_bench(args, &key, path, &rendered, &outfile)
    };

    let completed_at_utc = ops::chrono_lite_utc();

    if result.error.is_none() {
        write_run_meta(
            args,
            &key,
            variant,
            &cfg_name,
            target.path(),
            &started_at_utc,
            &completed_at_utc,
        );
    }

    // 4. Stop profilers and collect perf artifacts
    if args.perf_enabled {
        stop_and_collect_profilers(args, &key);
    }

    // 5. Collect docker artifacts and stop
    collect_docker_stats(args, &key);
    collect_docker_logs(args, framework, allocator, &key);
    stop_server_container(args, framework, allocator);

    // Cleanup remote files
    if args.deployment_mode == DeploymentMode::Remote {
        cleanup_remote_file(args, &format!("/tmp/{cfg_name}.toml"));
    }

    if let Some(error) = result.error {
        eprintln!("  benchmark validation failed: {error}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn generate_report(args: &Args) {
    match harrow_bench::perf_summary::render_results_dir(&args.results_dir) {
        Ok(()) => {
            let report_path = args.results_dir.join("summary.md");
            println!("Summary written to {}", report_path.display());
        }
        Err(e) => {
            eprintln!("warning: summary generation failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

fn preflight_checks(args: &Args) {
    println!("--- Preflight checks ---");

    // Check Docker availability
    match ops::run_local("docker info >/dev/null 2>&1 && echo ok") {
        Ok(o) if o.status.success() => println!("  Docker: ok"),
        _ => {
            eprintln!("  Docker: FAILED — is Docker running?");
            std::process::exit(1);
        }
    }

    match args.deployment_mode {
        DeploymentMode::Local => {
            // Check spinr if using host mode
            if args.spinr_mode == SpinrMode::Host {
                match ops::run_local(&format!("test -x {DEFAULT_SPINR_BIN} && echo ok")) {
                    Ok(o) if o.status.success() => println!("  Spinr: {DEFAULT_SPINR_BIN} (ok)"),
                    _ => {
                        eprintln!("  Spinr: MISSING ({DEFAULT_SPINR_BIN})");
                        std::process::exit(1);
                    }
                }
            }
        }
        DeploymentMode::Remote => {
            // SSH checks
            for (label, host) in [
                ("server", args.server_ssh.as_deref().unwrap()),
                ("client", args.client_ssh.as_deref().unwrap()),
            ] {
                let out = ops::ssh_run(&args.ssh_user, host, "echo ok");
                match out {
                    Ok(o) if o.status.success() => println!("  SSH to {label} ({host}): ok"),
                    _ => {
                        eprintln!("  SSH to {label} ({host}): FAILED");
                        std::process::exit(1);
                    }
                }
            }

            // Docker on server
            let out = ssh_server(args, "docker info >/dev/null 2>&1 && echo ok");
            match out {
                Ok(o) if o.status.success() => println!("  Docker on server: ok"),
                _ => {
                    eprintln!("  Docker on server: FAILED");
                    std::process::exit(1);
                }
            }

            if args.spinr_mode == SpinrMode::Docker {
                let out = ssh_client(args, "docker info >/dev/null 2>&1 && echo ok");
                match out {
                    Ok(o) if o.status.success() => println!("  Docker on client: ok"),
                    _ => {
                        eprintln!("  Docker on client: FAILED");
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    // Check server images exist
    for variant in &comparison_variants(args) {
        let image = variant.framework.image(args.backend, variant.allocator);
        let cmd = format!("docker image inspect {image} >/dev/null 2>&1 && echo ok");

        let result = match args.deployment_mode {
            DeploymentMode::Local => ops::run_local(&cmd),
            DeploymentMode::Remote => ssh_server(args, &cmd),
        };

        match result {
            Ok(o) if o.status.success() => println!("  Image {image}: ok"),
            _ => {
                eprintln!("  Image {image}: MISSING");
                std::process::exit(1);
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

    // Build test targets list
    let targets: Vec<TestTarget> = args
        .config_paths
        .iter()
        .map(|p| TestTarget { path: p.clone() })
        .collect();

    let target_names: Vec<String> = targets.iter().map(|t| t.name()).collect();
    let variants = comparison_variants(&args);
    let variant_labels: Vec<&str> = variants.iter().map(|v| v.label.as_str()).collect();

    println!("============================================");
    println!(
        " Performance Test :: {} mode with spinr",
        args.deployment_mode.as_str()
    );
    if variant_labels.len() > 1 {
        println!(" Comparison: {}", variant_labels.join(" vs "));
    }
    println!(
        " Instance: {}",
        args.instance_type.as_deref().unwrap_or("unknown")
    );

    match args.deployment_mode {
        DeploymentMode::Local => {
            println!(" Server URL: {}", args.server_url.as_deref().unwrap());
        }
        DeploymentMode::Remote => {
            println!(
                " Server: {} (private: {}:{})",
                args.server_ssh.as_deref().unwrap(),
                args.server_private.as_deref().unwrap(),
                args.port
            );
            println!(" Client: {}", args.client_ssh.as_deref().unwrap());
        }
    }

    println!(" Duration: {}s  Warmup: {}s", args.duration, args.warmup);
    println!(" Allocator: {}", args.allocator.as_str());
    println!(" Targets: {}", target_names.join(", "));
    println!(" Results: {}/", args.results_dir.display());
    println!("============================================");
    println!();

    for target in &targets {
        println!("========== TARGET: {} ==========", target.name());

        let run_variants: Vec<&Variant> = if target.is_session_config() {
            variants.iter().take(1).collect()
        } else {
            variants.iter().collect()
        };

        for variant in &run_variants {
            run_config(&args, target, variant);
            thread::sleep(SLEEP_BETWEEN_RUNS);
        }
    }

    println!();
    println!("========== GENERATING SUMMARY ==========");
    generate_report(&args);
    println!();
    println!("Done! Results in {}/", args.results_dir.display());
}
