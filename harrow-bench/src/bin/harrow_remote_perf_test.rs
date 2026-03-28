//! Unified performance test orchestrator.
//!
//! Supports both local (single-node) and remote (multi-node) deployments,
//! with pluggable load generators (spinr, vegeta).
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
//! # Load Generators
//!
//! ## Spinr
//! Custom Rust load generator with advanced features.
//! Requires TOML config files.
//!
//! ## Vegeta
//! Popular Go load testing tool.
//! Uses target files or inline targets.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
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
// Deployment Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeploymentMode {
    /// Single node - server and load generator on localhost
    Local,
    /// Multi node - server and load generator on separate nodes via SSH
    Remote,
}

impl DeploymentMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

// ---------------------------------------------------------------------------
// Load Generator
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadGenerator {
    /// Spinr - Rust load generator with TOML configs
    Spintr,
    /// Vegeta - Go load testing tool with target files
    Vegeta,
}

impl LoadGenerator {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "spinr" => Some(Self::Spintr),
            "vegeta" => Some(Self::Vegeta),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Spintr => "spinr",
            Self::Vegeta => "vegeta",
        }
    }
}

// ---------------------------------------------------------------------------
// Test Target Definition
// ---------------------------------------------------------------------------

/// A test target - either a spinr TOML config or vegeta target file
#[derive(Clone, Debug)]
enum TestTarget {
    SpintrConfig { path: PathBuf },
    VegetaTarget {
        path: PathBuf,
        rate: u32,
        duration_secs: u32,
    },
}

impl TestTarget {
    fn name(&self) -> String {
        match self {
            Self::SpintrConfig { path } => config_name(path),
            Self::VegetaTarget { path, .. } => config_name(path),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::SpintrConfig { path } => path,
            Self::VegetaTarget { path, .. } => path,
        }
    }

    fn is_session_config(&self) -> bool {
        match self {
            Self::SpintrConfig { path } => is_session_config_path(path),
            Self::VegetaTarget { .. } => false,
        }
    }

    fn is_compression_config(&self) -> bool {
        match self {
            Self::SpintrConfig { path } => is_compression_config_path(path),
            Self::VegetaTarget { .. } => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Vegeta-specific types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
struct VegetaMetrics {
    #[serde(rename = "latencies")]
    latencies: VegetaLatencies,
    #[serde(rename = "throughput")]
    throughput: f64,
    #[serde(rename = "success")]
    success: f64,
    #[serde(rename = "status_codes")]
    status_codes: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct VegetaLatencies {
    #[serde(rename = "mean")]
    mean: f64,
    #[serde(rename = "50th")]
    p50: f64,
    #[serde(rename = "95th")]
    p95: f64,
    #[serde(rename = "99th")]
    p99: f64,
    #[serde(rename = "max")]
    max: f64,
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
        }
    }

    fn image(self, backend: Backend, alloc: Allocator) -> String {
        match (self, backend) {
            (Self::Harrow, Backend::Monoio) => "harrow-monoio-server".to_string(),
            (Self::Harrow, Backend::Tokio) => {
                let suffix = alloc.suffix();
                format!("harrow-perf-server{suffix}")
            }
            (Self::Axum, _) => {
                let suffix = alloc.suffix();
                format!("axum-perf-server{suffix}")
            }
        }
    }

    fn container_name(self, backend: Backend, alloc: Allocator) -> String {
        match (self, backend) {
            (Self::Harrow, Backend::Monoio) => "harrow-monoio-server".to_string(),
            (Self::Harrow, Backend::Tokio) => {
                let suffix = alloc.suffix();
                format!("harrow-perf-server{suffix}")
            }
            (Self::Axum, _) => {
                let suffix = alloc.suffix();
                format!("axum-perf-server{suffix}")
            }
        }
    }

    fn launch_cmd(self, backend: Backend, port: u16, extra_flags: &str) -> String {
        let base = match (self, backend) {
            (Self::Harrow, Backend::Monoio) => format!("/harrow-monoio-server"),
            (Self::Harrow, Backend::Tokio) => format!("/harrow-perf-server --bind 0.0.0.0 --port {port}"),
            (Self::Axum, _) => format!("/axum-perf-server --bind 0.0.0.0 --port {port}"),
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

    fn supports_backend(self, backend: Backend) -> bool {
        match (self, backend) {
            (Self::Harrow, _) => true,  // Harrow supports both tokio and monoio
            (Self::Axum, Backend::Monoio) => false,  // Axum only supports tokio
            (Self::Axum, Backend::Tokio) => true,
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
    load_generator: LoadGenerator,

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
    config_paths: Vec<PathBuf>,    // For spinr
    target_paths: Vec<PathBuf>,    // For vegeta
    target_rate: u32,              // For vegeta

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
    args.perf_enabled && args.spinr_mode == SpinrMode::Host && args.load_generator == LoadGenerator::Spintr
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
        "Usage: harrow-remote-perf-test [MODE] [GENERATOR] [OPTIONS]\n\
         \n\
         MODE (required):\n\
         \x20 --mode MODE            Deployment mode: local|remote\n\
         \n\
         GENERATOR (required):\n\
         \x20 --load-generator GEN   Load generator: spinr|vegeta\n\
         \n\
         LOCAL MODE OPTIONS:\n\
         \x20 --server-url URL       Server URL (default: http://localhost:3000)\n\
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
         VEGETA OPTIONS:\n\
         \x20 --target-file PATH     Vegeta target file (repeatable)\n\
         \x20 --rate RPS             Requests per second (default: 1000)\n\
         \n\
         COMMON OPTIONS:\n\
         \x20 --port PORT            Server port (default: 3090 for spinr, 3000 for vegeta)\n\
         \x20 --duration SECS        Test duration in seconds (default: 60)\n\
         \x20 --warmup SECS          Warmup duration in seconds (default: 5)\n\
         \x20 --results-dir DIR      Override output directory\n\
         \x20 --server-flags FLAGS   Extra flags for harrow-perf-server\n\
         \x20 --os-monitors          Collect vmstat/sar/iostat/pidstat\n\
         \x20 --perf                 Collect perf artifacts (default mode: stat)\n\
         \x20 --perf-mode MODE       Perf mode: stat|record|both\n\
         \x20 --allocator ALLOC      Allocator: mimalloc|system (default: mimalloc)\n\
         \x20 --compare MODE         Comparison mode: framework|allocator (spinr only)\n\
         \x20 --framework FW         Framework: harrow|axum (default: harrow)\n\
         \x20 --backend BACKEND      Runtime backend: tokio|monoio (default: tokio, harrow only)\n\
         \n\
         EXAMPLES:\n\
         \n\
         # Local Vegeta test\n\
         harrow-remote-perf-test --mode local --load-generator vegeta \\\\n\
             --server-url http://localhost:3000 --target-file targets/basic-get.txt\n\
         \n\
         # Remote Vegeta test\n\
         harrow-remote-perf-test --mode remote --load-generator vegeta \\\\n\
             --server-ssh 10.0.1.10 --client-ssh 10.0.1.20 --server-private 10.0.1.10 \\\\n\
             --instance-type c8g.12xlarge --target-file targets/basic-get.txt\n\
         \n\
         # Remote Spinr test (existing functionality)\n\
         harrow-remote-perf-test --mode remote --load-generator spinr \\\\n\
             --server-ssh 10.0.1.10 --client-ssh 10.0.1.20 --server-private 10.0.1.10 \\\\n\
             --instance-type c8g.12xlarge --config spinr/text-c128.toml\n"
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    
    // Mode and generator (required)
    let mut deployment_mode: Option<DeploymentMode> = None;
    let mut load_generator: Option<LoadGenerator> = None;
    
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
    let mut target_paths: Vec<PathBuf> = Vec::new();
    let mut target_rate: u32 = 1000;
    
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
            // Mode and generator
            "--mode" => {
                deployment_mode = Some(DeploymentMode::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --mode: {}", args[i + 1]);
                    usage();
                }));
                i += 2;
            }
            "--load-generator" => {
                load_generator = Some(LoadGenerator::parse(&args[i + 1]).unwrap_or_else(|| {
                    eprintln!("invalid --load-generator: {}", args[i + 1]);
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
            "--target-file" => {
                target_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--rate" => {
                target_rate = args[i + 1].parse().expect("invalid --rate");
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
                framework = match args[i + 1].as_str() {
                    "harrow" => Framework::Harrow,
                    "axum" => Framework::Axum,
                    other => {
                        eprintln!("invalid --framework: {other}");
                        usage();
                    }
                };
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

    // Validate required mode and generator
    let deployment_mode = deployment_mode.unwrap_or_else(|| {
        eprintln!("error: --mode is required (local|remote)");
        usage();
    });

    let load_generator = load_generator.unwrap_or_else(|| {
        eprintln!("error: --load-generator is required (spinr|vegeta)");
        usage();
    });

    // Validate test targets match generator
    match load_generator {
        LoadGenerator::Spintr => {
            if config_paths.is_empty() {
                eprintln!("error: at least one --config is required for spinr mode");
                usage();
            }
            if !target_paths.is_empty() {
                eprintln!("error: --target-file is only valid for vegeta mode");
                usage();
            }
        }
        LoadGenerator::Vegeta => {
            if target_paths.is_empty() {
                eprintln!("error: at least one --target-file is required for vegeta mode");
                usage();
            }
            if !config_paths.is_empty() {
                eprintln!("error: --config is only valid for spinr mode");
                usage();
            }
        }
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
                eprintln!("error: --server-ssh, --client-ssh, and --server-private are required for remote mode");
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
        eprintln!("error: framework '{}' does not support backend '{}'", 
            framework.as_str(), backend.as_str());
        eprintln!("note: Axum only supports Tokio backend; use --backend tokio or --framework harrow");
        usage();
    }

    // Verify all target files exist
    let files_to_check: Vec<_> = match load_generator {
        LoadGenerator::Spintr => config_paths.iter().collect(),
        LoadGenerator::Vegeta => target_paths.iter().collect(),
    };
    for p in &files_to_check {
        if !p.exists() {
            eprintln!("error: file not found: {}", p.display());
            std::process::exit(1);
        }
    }

    // Set default port based on generator
    let port = port.unwrap_or_else(|| match load_generator {
        LoadGenerator::Spintr => DEFAULT_PORT,
        LoadGenerator::Vegeta => 3000,
    });

    // Set default server URL for local mode
    let server_url = server_url.unwrap_or_else(|| format!("http://localhost:{port}"));

    let results_dir = results_dir_override.unwrap_or_else(|| {
        let ts = Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H-%M-%SZ"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let instance = instance_type.as_deref().unwrap_or("unknown");
        PathBuf::from(format!("docs/perf/{instance}/{ts}"))
    });

    Args {
        deployment_mode,
        load_generator,
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
        target_paths,
        target_rate,
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
    raw.replace("{{ server }}", server)
        .replace("{{ duration }}", &duration.to_string())
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
                label: "axum".into(),
                framework: Framework::Axum,
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

fn ssh_run(user: &str, host: &str, remote_cmd: &str) -> std::io::Result<Output> {
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

fn ssh_server(args: &Args, remote_cmd: &str) -> std::io::Result<Output> {
    ssh_run(&args.ssh_user, args.server_ssh.as_deref().unwrap(), remote_cmd)
}

fn ssh_client(args: &Args, remote_cmd: &str) -> std::io::Result<Output> {
    ssh_run(&args.ssh_user, args.client_ssh.as_deref().unwrap(), remote_cmd)
}

fn ssh_side(args: &Args, side: RemoteSide, remote_cmd: &str) -> std::io::Result<Output> {
    match side {
        RemoteSide::Server => ssh_server(args, remote_cmd),
        RemoteSide::Client => ssh_client(args, remote_cmd),
    }
}

fn scp_to_remote(user: &str, host: &str, local_path: &Path, remote_path: &str) {
    let dest = format!("{user}@{host}:{remote_path}");
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

fn scp_to_client(args: &Args, local_path: &Path, remote_path: &str) {
    scp_to_remote(&args.ssh_user, args.client_ssh.as_deref().unwrap(), local_path, remote_path);
}

// ---------------------------------------------------------------------------
// Local Command Helpers (for Local Mode)
// ---------------------------------------------------------------------------

fn run_local(cmd: &str) -> std::io::Result<Output> {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

// ---------------------------------------------------------------------------
// Server Container Management
// ---------------------------------------------------------------------------

fn start_server_container(args: &Args, framework: Framework, allocator: Allocator, server_flags: &str) {
    let name = framework.container_name(args.backend, allocator);
    let image = framework.image(args.backend, allocator);
    let command = framework.launch_cmd(args.backend, args.port, server_flags);
    
    println!(">>> Starting {} server on {}", framework.as_str(), args.deployment_mode.as_str());
    
    match args.deployment_mode {
        DeploymentMode::Local => {
            // Stop any existing container
            let _ = run_local(&format!("docker rm -f {name} 2>/dev/null || true"));
            let docker_cmd = format!(
                "docker run -d --name {name} -p {0}:{0} --ulimit nofile=65535:65535 {image} {command}",
                args.port
            );
            match run_local(&docker_cmd) {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    eprintln!("  warning: docker run {} stderr: {}", framework.as_str(), stderr.trim());
                }
                Err(e) => eprintln!("  failed to start {}: {e}", framework.as_str()),
            }
        }
        DeploymentMode::Remote => {
            let _ = ssh_server(args, &format!("docker rm -f {name} 2>/dev/null || true"));
            let docker_cmd = format!(
                "docker run -d --name {name} --network host --ulimit nofile=65535:65535 {image} {command}"
            );
            match ssh_server(args, &docker_cmd) {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    eprintln!("  warning: docker run {} stderr: {}", framework.as_str(), stderr.trim());
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
            let _ = run_local(&format!("docker rm -f {name} 2>/dev/null || true"));
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
        if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(500)).is_ok() {
            println!("    Health check passed");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("server on {addr} did not start within {timeout:?}"))
}

fn container_pid(args: &Args, framework: Framework, allocator: Allocator) -> Option<u32> {
    let name = framework.container_name(args.backend, allocator);
    let remote_cmd = format!("docker inspect -f '{{{{.State.Pid}}}}' {name}");
    
    let out = match args.deployment_mode {
        DeploymentMode::Local => run_local(&remote_cmd).ok(),
        DeploymentMode::Remote => ssh_server(args, &remote_cmd).ok(),
    }?;
    
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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
) -> Option<Value> {
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
                    &*local_tmp.to_string_lossy()
                ),
            };
            
            let result = run_local(&cmd);
            let _ = fs::remove_file(&local_tmp);
            
            match result {
                Ok(o) if o.status.success() => {
                    let _ = fs::write(outfile, &o.stdout);
                    let val: Option<Value> = serde_json::from_slice(&o.stdout).ok();
                    if let Some(ref v) = val {
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
    }
}

/// Run a vegeta benchmark
fn run_vegeta_bench(
    args: &Args,
    key: &str,
    target_path: &Path,
    rate: u32,
    duration_secs: u32,
    server_url: &str,
    outfile: &Path,
) -> Option<Value> {
    // Read and update target file with server URL
    let target_content = fs::read_to_string(target_path).ok()?;
    let updated_target = target_content.replace("{{ server_url }}", server_url);
    
    // Create temp target file
    let temp_target = std::env::temp_dir().join(format!("{key}.targets"));
    let _ = fs::write(&temp_target, updated_target);
    
    let duration_str = format!("{duration_secs}s");
    let bin_file = format!("/tmp/{key}.bin");
    
    let vegeta_cmd = format!(
        "vegeta attack -targets={} -duration={} -rate={}/s -output={} && \\\n         vegeta report -type=json {}",
        temp_target.display(),
        duration_str,
        rate,
        bin_file,
        bin_file
    );
    
    println!("  [{key}] -> vegeta attack -duration={duration_str} -rate={rate}/s");

    let result = match args.deployment_mode {
        DeploymentMode::Local => {
            run_local(&vegeta_cmd)
        }
        DeploymentMode::Remote => {
            // Upload target file to client
            let remote_target = format!("/tmp/{key}.targets");
            scp_to_client(args, &temp_target, &remote_target);
            
            let remote_cmd = format!(
                "vegeta attack -targets={remote_target} -duration={duration_str} -rate={rate}/s -output={bin_file} && \\
                 vegeta report -type=json {bin_file}"
            );
            ssh_client(args, &remote_cmd)
        }
    };
    
    let _ = fs::remove_file(&temp_target);

    match result {
        Ok(o) if o.status.success() => {
            // Parse vegeta JSON output and convert to common format
            let vegeta_metrics: Option<VegetaMetrics> = serde_json::from_slice(&o.stdout).ok();
            
            if let Some(metrics) = vegeta_metrics {
                // Convert vegeta metrics to spinr-compatible format
                let converted = convert_vegeta_to_spinr_format(&metrics);
                let _ = fs::write(outfile, serde_json::to_vec_pretty(&converted).unwrap());
                
                println!(
                    "    -> rps={:.0} p99={:.3}ms success={:.1}%",
                    metrics.throughput,
                    metrics.latencies.p99 / 1_000_000.0, // Convert ns to ms
                    metrics.success * 100.0
                );
                
                Some(converted)
            } else {
                // Save raw output
                let _ = fs::write(outfile, &o.stdout);
                serde_json::from_slice(&o.stdout).ok()
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("    vegeta failed (exit {}): {}", o.status, stderr.trim());
            None
        }
        Err(e) => {
            eprintln!("    failed to run vegeta: {e}");
            None
        }
    }
}

/// Convert Vegeta metrics to spinr-compatible JSON format
fn convert_vegeta_to_spinr_format(metrics: &VegetaMetrics) -> Value {
    // Convert nanoseconds to milliseconds
    let ns_to_ms = |ns: f64| ns / 1_000_000.0;
    
    serde_json::json!({
        "scenarios": [{
            "metrics": {
                "rps": metrics.throughput,
                "latency_mean_ms": ns_to_ms(metrics.latencies.mean),
                "latency_p50_ms": ns_to_ms(metrics.latencies.p50),
                "latency_p95_ms": ns_to_ms(metrics.latencies.p95),
                "latency_p99_ms": ns_to_ms(metrics.latencies.p99),
                "latency_max_ms": ns_to_ms(metrics.latencies.max),
                "success_rate": metrics.success,
                "status_codes": metrics.status_codes
            }
        }]
    })
}

// ---------------------------------------------------------------------------
// Artifact Collection
// ---------------------------------------------------------------------------

fn run_key(variant_label: &str, cfg_name: &str) -> String {
    format!("{variant_label}_{cfg_name}")
}

fn pull_remote_file(args: &Args, side: RemoteSide, remote_path: &str, local_path: &Path) {
    let remote_cmd = format!("test -f {remote_path} && cat {remote_path}");
    
    let result = match args.deployment_mode {
        DeploymentMode::Local => run_local(&format!("cat {remote_path}")),
        DeploymentMode::Remote => ssh_side(args, side, &remote_cmd),
    };
    
    match result {
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
    let cmd = format!("rm -f {remote_path}");
    match args.deployment_mode {
        DeploymentMode::Local => { let _ = run_local(&cmd); }
        DeploymentMode::Remote => { let _ = ssh_side(args, side, &cmd); }
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
        "load_generator": args.load_generator.as_str(),
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
    let cmd = "docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}'";
    
    let result = match args.deployment_mode {
        DeploymentMode::Local => run_local(cmd),
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
        DeploymentMode::Local => run_local(&cmd),
        DeploymentMode::Remote => ssh_server(args, &cmd),
    };
    
    if let Ok(out) = result {
        let path = args.results_dir.join(format!("logs_{key}.txt"));
        let _ = fs::write(path, &out.stdout);
    }
}

// ---------------------------------------------------------------------------
// Per-Config Run
// ---------------------------------------------------------------------------

fn run_config(
    args: &Args,
    target: &TestTarget,
    variant: &Variant,
    results: &mut BTreeMap<String, Value>,
) {
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
        if framework == Framework::Harrow && let Some(ref flags) = args.server_flags {
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

    // 2. Run the benchmark
    let outfile = args.results_dir.join(format!("{key}.json"));
    let started_at_utc = chrono_lite_utc();
    
    let result = match (args.load_generator, target) {
        (LoadGenerator::Spintr, TestTarget::SpintrConfig { path }) => {
            // Render template
            let raw = fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            let server_addr = match args.deployment_mode {
                DeploymentMode::Local => format!("localhost:{}", args.port),
                DeploymentMode::Remote => format!("{}:{}", args.server_private.as_deref().unwrap(), args.port),
            };
            let rendered = render_template(&raw, &server_addr, args.duration, args.warmup);
            
            let _remote_config = format!("/tmp/{cfg_name}.toml");
            run_spinr_bench(args, &key, path, &rendered, &outfile)
        }
        (LoadGenerator::Vegeta, TestTarget::VegetaTarget { path, rate, duration_secs }) => {
            let server_url = args.server_url.as_deref().unwrap_or("http://localhost:3000");
            run_vegeta_bench(args, &key, path, *rate, *duration_secs, server_url, &outfile)
        }
        _ => {
            eprintln!("  error: mismatched load generator and target type");
            None
        }
    };
    
    let completed_at_utc = chrono_lite_utc();

    if let Some(v) = result {
        results.insert(key.clone(), v);
    }

    // 3. Collect artifacts and stop
    write_run_meta(
        args,
        &key,
        variant,
        &cfg_name,
        target.path(),
        &started_at_utc,
        &completed_at_utc,
    );
    collect_docker_stats(args, &key);
    collect_docker_logs(args, framework, allocator, &key);
    stop_server_container(args, framework, allocator);
    
    // Cleanup remote files
    if args.deployment_mode == DeploymentMode::Remote {
        match args.load_generator {
            LoadGenerator::Spintr => {
                cleanup_remote_file(args, RemoteSide::Client, &format!("/tmp/{cfg_name}.toml"));
            }
            LoadGenerator::Vegeta => {
                cleanup_remote_file(args, RemoteSide::Client, &format!("/tmp/{key}.targets"));
                cleanup_remote_file(args, RemoteSide::Client, &format!("/tmp/{key}.bin"));
            }
        }
    }
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

    // Check Docker availability
    match run_local("docker info >/dev/null 2>&1 && echo ok") {
        Ok(o) if o.status.success() => println!("  Docker: ok"),
        _ => {
            eprintln!("  Docker: FAILED — is Docker running?");
            std::process::exit(1);
        }
    }

    match args.deployment_mode {
        DeploymentMode::Local => {
            // Check vegeta is available if using vegeta
            if args.load_generator == LoadGenerator::Vegeta {
                match run_local("command -v vegeta >/dev/null && echo ok") {
                    Ok(o) if o.status.success() => println!("  Vegeta: ok"),
                    _ => {
                        eprintln!("  Vegeta: MISSING — install with: go install github.com/tsenart/vegeta@latest");
                        std::process::exit(1);
                    }
                }
            }
            
            // Check spinr if using spinr in host mode
            if args.load_generator == LoadGenerator::Spintr && args.spinr_mode == SpinrMode::Host {
                match run_local(&format!("test -x {DEFAULT_SPINR_BIN} && echo ok")) {
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
                let out = ssh_run(&args.ssh_user, host, "echo ok");
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
            DeploymentMode::Local => run_local(&cmd),
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
    let targets: Vec<TestTarget> = match args.load_generator {
        LoadGenerator::Spintr => args.config_paths
            .iter()
            .map(|p| TestTarget::SpintrConfig { path: p.clone() })
            .collect(),
        LoadGenerator::Vegeta => args.target_paths
            .iter()
            .map(|p| TestTarget::VegetaTarget {
                path: p.clone(),
                rate: args.target_rate,
                duration_secs: args.duration,
            })
            .collect(),
    };

    let target_names: Vec<String> = targets.iter().map(|t| t.name()).collect();
    let variants = comparison_variants(&args);
    let variant_labels: Vec<&str> = variants.iter().map(|v| v.label.as_str()).collect();

    println!("============================================");
    println!(
        " Performance Test :: {} mode with {}",
        args.deployment_mode.as_str(),
        args.load_generator.as_str()
    );
    if variant_labels.len() > 1 {
        println!(" Comparison: {}", variant_labels.join(" vs "));
    }
    println!(" Instance: {}", args.instance_type.as_deref().unwrap_or("unknown"));
    
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
    if args.load_generator == LoadGenerator::Vegeta {
        println!(" Rate: {} req/s", args.target_rate);
    }
    println!(" Allocator: {}", args.allocator.as_str());
    println!(" Targets: {}", target_names.join(", "));
    println!(" Results: {}/", args.results_dir.display());
    println!("============================================");
    println!();

    let mut results: BTreeMap<String, Value> = BTreeMap::new();

    for target in &targets {
        println!("========== TARGET: {} ==========", target.name());

        let run_variants: Vec<&Variant> = if target.is_session_config() {
            variants.iter().take(1).collect()
        } else {
            variants.iter().collect()
        };

        for variant in &run_variants {
            run_config(&args, target, variant, &mut results);
            thread::sleep(SLEEP_BETWEEN_RUNS);
        }
    }

    println!();
    println!("========== GENERATING SUMMARY ==========");
    generate_report(&args);
    println!();
    println!("Done! Results in {}/", args.results_dir.display());
}
