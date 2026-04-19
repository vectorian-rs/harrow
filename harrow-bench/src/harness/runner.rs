use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::perf_summary;

use super::ops::{
    chrono_lite_utc, http_health_check, metric_f64, run_local, scp_to_remote, spinr_metrics,
    ssh_health_check, ssh_run, timestamp_slug, val_str, validate_spinr_metrics,
    validation_success_rate,
};
use super::schema::{
    BenchmarkMetrics, CaseReport, CompareSide, GitDescriptor, ImageDescriptor,
    ImplementationLabels, ImplementationResult, LatencyMetrics, LoadProfile, RUN_SCHEMA_VERSION,
    ResultArtifacts, RunDefaults, RunReport, SuiteDescriptor, TargetDescriptor, TemplateFiles,
};
use super::spec::{
    CaseSpec, DeploymentMode, ImplementationRegistry, ImplementationSpec, LoadGeneratorKind,
    RunMode, SuiteSpec,
};
use super::template::{render_template_file, render_template_str};

const DEFAULT_PORT: u16 = 3090;
const DEFAULT_SSH_USER: &str = "alpine";
const DEFAULT_SPINR_IMAGE: &str = "spinr:arm64-0.5.1";
const DEFAULT_SPINR_BUILD_TASK: &str = "docker:loadgen:spinr";
const DEFAULT_WRK3_IMAGE: &str = "wrk3:arm64-0.2.0";
const DEFAULT_WRK3_BUILD_TASK: &str = "docker:loadgen:wrk3";
const SLEEP_BETWEEN_RUNS: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct CommonRunConfig {
    pub deployment_mode: DeploymentMode,
    pub suite_path: PathBuf,
    pub registry_path: PathBuf,
    pub case_filters: Vec<String>,
    pub results_dir: Option<PathBuf>,
    pub server_ssh: Option<String>,
    pub client_ssh: Option<String>,
    pub server_private_ip: Option<String>,
    pub ssh_user: String,
    pub port: u16,
    pub duration_secs: u32,
    pub warmup_secs: u32,
    pub build_missing: bool,
}

impl Default for CommonRunConfig {
    fn default() -> Self {
        Self {
            deployment_mode: DeploymentMode::Local,
            suite_path: PathBuf::from("harrow-bench/suites/http-basic.toml"),
            registry_path: PathBuf::from("harrow-bench/implementations.toml"),
            case_filters: Vec::new(),
            results_dir: None,
            server_ssh: None,
            client_ssh: None,
            server_private_ip: None,
            ssh_user: DEFAULT_SSH_USER.to_string(),
            port: DEFAULT_PORT,
            duration_secs: 30,
            warmup_secs: 5,
            build_missing: true,
        }
    }
}

#[derive(Clone)]
pub struct SingleRunConfig {
    pub common: CommonRunConfig,
    pub implementation_id: String,
}

#[derive(Clone)]
pub struct CompareRunConfig {
    pub common: CommonRunConfig,
    pub left_id: String,
    pub right_id: String,
}

#[derive(Clone)]
struct RunVariant {
    implementation: ImplementationSpec,
    variant_label: String,
    compare_side: Option<CompareSide>,
}

struct BenchRunResult {
    value: Option<Value>,
    error: Option<String>,
    raw_output_path: PathBuf,
}

#[derive(Clone)]
struct CaseExecutionRecord {
    case: CaseSpec,
    rendered_template: PathBuf,
    results: Vec<ImplementationExecutionRecord>,
}

#[derive(Clone)]
struct ImplementationExecutionRecord {
    variant: RunVariant,
    started_at_utc: String,
    completed_at_utc: String,
    image_id: Option<String>,
    raw_metrics_path: PathBuf,
    metrics: Value,
}

pub fn run_single(config: SingleRunConfig) -> Result<PathBuf, String> {
    let common = config.common.clone();
    let registry = ImplementationRegistry::load(&common.registry_path)?;
    let implementation = registry
        .get(&config.implementation_id)
        .cloned()
        .ok_or_else(|| format!("unknown implementation '{}'", config.implementation_id))?;

    run_plan(
        RunMode::Single,
        common,
        vec![RunVariant {
            variant_label: implementation.id.clone(),
            implementation,
            compare_side: None,
        }],
    )
}

pub fn run_compare(config: CompareRunConfig) -> Result<PathBuf, String> {
    let common = config.common.clone();
    let registry = ImplementationRegistry::load(&common.registry_path)?;

    let left = registry
        .get(&config.left_id)
        .cloned()
        .ok_or_else(|| format!("unknown implementation '{}'", config.left_id))?;
    let right = registry
        .get(&config.right_id)
        .cloned()
        .ok_or_else(|| format!("unknown implementation '{}'", config.right_id))?;

    run_plan(
        RunMode::Compare,
        common,
        vec![
            RunVariant {
                variant_label: left.id.clone(),
                implementation: left,
                compare_side: Some(CompareSide::Left),
            },
            RunVariant {
                variant_label: right.id.clone(),
                implementation: right,
                compare_side: Some(CompareSide::Right),
            },
        ],
    )
}

fn run_plan(
    mode: RunMode,
    config: CommonRunConfig,
    variants: Vec<RunVariant>,
) -> Result<PathBuf, String> {
    validate_common_config(&config)?;

    let suite = SuiteSpec::load(&config.suite_path)?;
    let selected_cases = suite.selected_cases(&config.case_filters)?;
    if selected_cases.is_empty() {
        return Err(format!("suite '{}' contains no runnable cases", suite.name));
    }

    let results_dir = config
        .results_dir
        .clone()
        .unwrap_or_else(|| default_results_dir(mode, &suite, &variants));
    fs::create_dir_all(results_dir.join("rendered")).map_err(|e| {
        format!(
            "failed to create results dir {}: {e}",
            results_dir.display()
        )
    })?;

    preflight_checks(&config, &variants, selected_cases.as_slice())?;

    let started_at_utc = chrono_lite_utc();
    let mut case_records = Vec::with_capacity(selected_cases.len());

    print_run_header(
        mode,
        &config,
        &suite,
        &variants,
        selected_cases.as_slice(),
        &results_dir,
    );

    for case in selected_cases {
        println!("========== TARGET: {} ==========", case.id);
        let rendered_template = render_case_template(&config, &suite, case, &results_dir)?;

        let mut implementation_records = Vec::with_capacity(variants.len());
        for variant in &variants {
            println!();
            println!("--- {} / {} ---", variant.variant_label, case.id);
            let record =
                run_case_for_variant(&config, case, &rendered_template, variant, &results_dir)?;
            implementation_records.push(record);
            thread::sleep(SLEEP_BETWEEN_RUNS);
        }

        case_records.push(CaseExecutionRecord {
            case: case.clone(),
            rendered_template,
            results: implementation_records,
        });
    }

    let completed_at_utc = chrono_lite_utc();
    write_canonical_report(
        mode,
        &config,
        &suite,
        &results_dir,
        &started_at_utc,
        &completed_at_utc,
        &case_records,
    )?;

    println!();
    println!("========== GENERATING SUMMARY ==========");
    perf_summary::render_results_dir(&results_dir)
        .map_err(|e| format!("failed to render summary in {}: {e}", results_dir.display()))?;
    println!(
        "Summary written to {}",
        results_dir.join("summary.md").display()
    );
    println!();
    println!("Done! Results in {}/", results_dir.display());

    Ok(results_dir)
}

fn validate_common_config(config: &CommonRunConfig) -> Result<(), String> {
    match config.deployment_mode {
        DeploymentMode::Local => Ok(()),
        DeploymentMode::Remote => {
            if config.server_ssh.is_none()
                || config.client_ssh.is_none()
                || config.server_private_ip.is_none()
            {
                return Err(
                    "--server-ssh, --client-ssh, and --server-private-ip are required in remote mode".into(),
                );
            }
            Ok(())
        }
    }
}

fn default_results_dir(mode: RunMode, suite: &SuiteSpec, variants: &[RunVariant]) -> PathBuf {
    let ts = timestamp_slug();
    let label = match mode {
        RunMode::Single => sanitize_label(&variants[0].variant_label),
        RunMode::Compare => format!(
            "{}-vs-{}",
            sanitize_label(&variants[0].variant_label),
            sanitize_label(&variants[1].variant_label)
        ),
    };
    PathBuf::from(format!(
        "perf/{ts}-{}-{}",
        sanitize_label(&suite.name),
        label
    ))
}

fn print_run_header(
    mode: RunMode,
    config: &CommonRunConfig,
    suite: &SuiteSpec,
    variants: &[RunVariant],
    cases: &[&CaseSpec],
    results_dir: &Path,
) {
    println!("============================================");
    println!(" Benchmark Harness :: {}", mode.as_str());
    println!(" Suite: {}", suite.name);
    if variants.len() == 1 {
        println!(" Implementation: {}", variants[0].variant_label);
    } else {
        println!(
            " Comparison: {} vs {}",
            variants[0].variant_label, variants[1].variant_label
        );
    }
    println!(" Mode: {}", config.deployment_mode.as_str());
    match config.deployment_mode {
        DeploymentMode::Local => {
            println!(
                " Server target: {}:{}",
                config.server_private_ip.as_deref().unwrap_or("127.0.0.1"),
                config.port
            );
        }
        DeploymentMode::Remote => {
            println!(
                " Server: {} (private: {}:{})",
                config.server_ssh.as_deref().unwrap_or("unknown"),
                config.server_private_ip.as_deref().unwrap_or("unknown"),
                config.port
            );
            println!(
                " Client: {}",
                config.client_ssh.as_deref().unwrap_or("unknown")
            );
        }
    }
    println!(
        " Duration: {}s  Warmup: {}s",
        config.duration_secs, config.warmup_secs
    );
    println!(
        " Cases: {}",
        cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(" Results: {}/", results_dir.display());
    println!("============================================");
    println!();
}

fn render_case_template(
    config: &CommonRunConfig,
    suite: &SuiteSpec,
    case: &CaseSpec,
    results_dir: &Path,
) -> Result<PathBuf, String> {
    let template_path = case.resolved_template_path(&config.suite_path);
    let context = template_context(config, suite, case);
    let rendered = render_template_file(&template_path, &context)?;

    let suffix = match case.generator {
        LoadGeneratorKind::Spinr => "toml",
        LoadGeneratorKind::Wrk3 => "wrk3.toml",
    };
    let rendered_path = results_dir
        .join("rendered")
        .join(format!("{}.{}", case.id, suffix));
    fs::write(&rendered_path, rendered).map_err(|e| {
        format!(
            "failed to write rendered template {}: {e}",
            rendered_path.display()
        )
    })?;
    Ok(rendered_path)
}

fn template_context(
    config: &CommonRunConfig,
    suite: &SuiteSpec,
    case: &CaseSpec,
) -> BTreeMap<String, Value> {
    let mut context = BTreeMap::new();
    let server_private_ip = config
        .server_private_ip
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let duration_secs = case.duration_secs.unwrap_or(config.duration_secs);
    let warmup_secs = case.warmup_secs.unwrap_or(config.warmup_secs);

    context.insert("suite".into(), Value::String(suite.name.clone()));
    context.insert("case_id".into(), Value::String(case.id.clone()));
    context.insert(
        "server_private_ip".into(),
        Value::String(server_private_ip.clone()),
    );
    context.insert("port".into(), Value::Number(config.port.into()));
    context.insert(
        "base_url".into(),
        Value::String(format!("http://{server_private_ip}:{}", config.port)),
    );
    context.insert("duration_secs".into(), Value::Number(duration_secs.into()));
    context.insert("warmup_secs".into(), Value::Number(warmup_secs.into()));
    if let Some(rate) = case.rate {
        context.insert("rate".into(), Value::Number(rate.into()));
    }
    if let Some(concurrency) = resolved_concurrency(case) {
        context.insert("connections".into(), Value::Number(concurrency.into()));
        context.insert("concurrency".into(), Value::Number(concurrency.into()));
        context.insert("workers".into(), Value::Number(concurrency.into()));
    }

    for (key, value) in &case.context {
        if let Ok(json_value) = serde_json::to_value(value) {
            context.insert(key.clone(), json_value);
        }
    }

    context
}

fn run_case_for_variant(
    config: &CommonRunConfig,
    case: &CaseSpec,
    rendered_template: &Path,
    variant: &RunVariant,
    results_dir: &Path,
) -> Result<ImplementationExecutionRecord, String> {
    start_server_container(config, case, variant)?;
    if let Err(error) = wait_for_server(config, &variant.implementation, Duration::from_secs(30)) {
        stop_server_container(config, variant);
        return Err(error);
    }

    let artifacts_dir = results_dir
        .join("raw")
        .join(sanitize_label(&case.id))
        .join(sanitize_label(&variant.implementation.id));
    if let Err(error) = fs::create_dir_all(&artifacts_dir) {
        stop_server_container(config, variant);
        return Err(format!(
            "failed to create raw artifact dir {}: {error}",
            artifacts_dir.display()
        ));
    }

    let key = run_key(&variant.variant_label, &case.id);
    let raw_metrics_path = artifacts_dir.join("loadgen.json");
    let started_at_utc = chrono_lite_utc();

    let run_result = match case.generator {
        LoadGeneratorKind::Spinr => {
            run_spinr_bench(config, &key, rendered_template, &raw_metrics_path)
        }
        LoadGeneratorKind::Wrk3 => run_wrk3_bench(config, case, &raw_metrics_path),
    };

    let completed_at_utc = chrono_lite_utc();
    stop_server_container(config, variant);

    if let Some(error) = run_result.error {
        return Err(error);
    }

    let metrics = run_result
        .value
        .clone()
        .ok_or_else(|| format!("run '{}' completed without parsed metrics", key))?;
    let image_id = image_id(config, &variant.implementation.image);

    Ok(ImplementationExecutionRecord {
        variant: variant.clone(),
        started_at_utc,
        completed_at_utc,
        image_id,
        raw_metrics_path: run_result.raw_output_path,
        metrics,
    })
}

fn preflight_checks(
    config: &CommonRunConfig,
    variants: &[RunVariant],
    cases: &[&CaseSpec],
) -> Result<(), String> {
    println!("--- Preflight checks ---");

    match run_local("docker info >/dev/null 2>&1 && echo ok") {
        Ok(out) if out.status.success() => println!("  Docker: ok"),
        _ => return Err("Docker is not available locally".into()),
    }

    match config.deployment_mode {
        DeploymentMode::Local => {}
        DeploymentMode::Remote => {
            for (label, host) in [
                ("server", config.server_ssh.as_deref().unwrap_or("")),
                ("client", config.client_ssh.as_deref().unwrap_or("")),
            ] {
                let out = ssh_run(&config.ssh_user, host, "echo ok")
                    .map_err(|e| format!("failed to reach {label} host {host}: {e}"))?;
                if !out.status.success() {
                    return Err(format!("SSH to {label} host {host} failed"));
                }
                println!("  SSH to {label} ({host}): ok");
            }

            for (label, cmd) in [
                (
                    "server docker",
                    ssh_server(config, "docker info >/dev/null 2>&1 && echo ok"),
                ),
                (
                    "client docker",
                    ssh_client(config, "docker info >/dev/null 2>&1 && echo ok"),
                ),
            ] {
                let out = cmd.map_err(|e| format!("failed to check {label}: {e}"))?;
                if !out.status.success() {
                    return Err(format!("{label} check failed"));
                }
                println!("  {label}: ok");
            }
        }
    }

    for variant in variants {
        ensure_server_image(config, &variant.implementation)?;
        println!("  Image {}: ok", variant.implementation.image);
    }

    for case in cases {
        ensure_loadgen_image(config, case.generator)?;
        println!("  Load generator {}: ok", case.generator.as_str());
    }

    println!("--- Preflight checks passed ---");
    println!();
    Ok(())
}

fn ensure_server_image(
    config: &CommonRunConfig,
    implementation: &ImplementationSpec,
) -> Result<(), String> {
    let inspect_cmd = format!(
        "docker image inspect {} >/dev/null 2>&1 && echo ok",
        implementation.image
    );
    match config.deployment_mode {
        DeploymentMode::Local => {
            let out = run_local(&inspect_cmd).map_err(|e| {
                format!(
                    "failed to inspect local image {}: {e}",
                    implementation.image
                )
            })?;
            if out.status.success() {
                return Ok(());
            }

            if config.build_missing
                && let Some(task) = implementation.build_task.as_deref()
            {
                println!(
                    "  Building missing image {} via {}",
                    implementation.image, task
                );
                let build = run_local(&format!("mise run {task}"))
                    .map_err(|e| format!("failed to run local build task {task}: {e}"))?;
                if !build.status.success() {
                    let stderr = String::from_utf8_lossy(&build.stderr);
                    return Err(format!("local build task {task} failed: {}", stderr.trim()));
                }
                let recheck = run_local(&inspect_cmd).map_err(|e| {
                    format!("failed to re-check image {}: {e}", implementation.image)
                })?;
                if recheck.status.success() {
                    return Ok(());
                }
            }

            Err(format!(
                "missing local image '{}' and no successful build path was available",
                implementation.image
            ))
        }
        DeploymentMode::Remote => {
            let out = ssh_server(config, &inspect_cmd).map_err(|e| {
                format!(
                    "failed to inspect remote image {}: {e}",
                    implementation.image
                )
            })?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "missing remote image '{}' on server host",
                    implementation.image
                ))
            }
        }
    }
}

fn ensure_loadgen_image(
    config: &CommonRunConfig,
    generator: LoadGeneratorKind,
) -> Result<(), String> {
    let (image, build_task) = match generator {
        LoadGeneratorKind::Spinr => (DEFAULT_SPINR_IMAGE, Some(DEFAULT_SPINR_BUILD_TASK)),
        LoadGeneratorKind::Wrk3 => (DEFAULT_WRK3_IMAGE, Some(DEFAULT_WRK3_BUILD_TASK)),
    };

    let inspect_cmd = format!("docker image inspect {image} >/dev/null 2>&1 && echo ok");
    match config.deployment_mode {
        DeploymentMode::Local => {
            let out = run_local(&inspect_cmd).map_err(|e| {
                format!("failed to inspect local load generator image {image}: {e}")
            })?;
            if out.status.success() {
                return Ok(());
            }

            if config.build_missing
                && let Some(task) = build_task
            {
                println!("  Building missing image {image} via {task}");
                let build = run_local(&format!("mise run {task}"))
                    .map_err(|e| format!("failed to run load generator build task {task}: {e}"))?;
                if !build.status.success() {
                    let stderr = String::from_utf8_lossy(&build.stderr);
                    return Err(format!(
                        "load generator build task {task} failed: {}",
                        stderr.trim()
                    ));
                }

                let recheck = run_local(&inspect_cmd)
                    .map_err(|e| format!("failed to re-check image {image}: {e}"))?;
                if recheck.status.success() {
                    return Ok(());
                }
            }

            Err(format!("missing local load generator image '{image}'"))
        }
        DeploymentMode::Remote => {
            let out = ssh_client(config, &inspect_cmd).map_err(|e| {
                format!("failed to inspect remote load generator image {image}: {e}")
            })?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "missing remote load generator image '{image}' on client host"
                ))
            }
        }
    }
}

fn start_server_container(
    config: &CommonRunConfig,
    case: &CaseSpec,
    variant: &RunVariant,
) -> Result<(), String> {
    let container_name = container_name(&variant.implementation.id);
    let mut context = BTreeMap::new();
    context.insert("port".into(), Value::Number(config.port.into()));
    let base_command = render_template_str(&variant.implementation.command, &context)?;
    let command = if case.server_flags.is_empty() {
        base_command
    } else {
        format!("{base_command} {}", case.server_flags.join(" "))
    };

    println!(
        ">>> Starting {} server on {}",
        variant.variant_label,
        config.deployment_mode.as_str()
    );

    let privileged = if backend_requires_privileged(variant.implementation.backend.as_deref()) {
        " --privileged"
    } else {
        ""
    };

    match config.deployment_mode {
        DeploymentMode::Local => {
            let _ = run_local(&format!(
                "docker rm -f {container_name} >/dev/null 2>&1 || true"
            ));
            let docker_cmd = format!(
                "docker run -d --name {container_name} --network host --ulimit nofile=65535:65535{privileged} {} {}",
                variant.implementation.image, command
            );
            let out = run_local(&docker_cmd)
                .map_err(|e| format!("failed to start local container {container_name}: {e}"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(format!(
                    "failed to start local container {container_name}: {}",
                    stderr.trim()
                ));
            }
        }
        DeploymentMode::Remote => {
            let _ = ssh_server(
                config,
                &format!("docker rm -f {container_name} >/dev/null 2>&1 || true"),
            );
            let docker_cmd = format!(
                "docker run -d --name {container_name} --network host --ulimit nofile=65535:65535{privileged} {} {}",
                variant.implementation.image, command
            );
            let out = ssh_server(config, &docker_cmd)
                .map_err(|e| format!("failed to start remote container {container_name}: {e}"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(format!(
                    "failed to start remote container {container_name}: {}",
                    stderr.trim()
                ));
            }
        }
    }

    thread::sleep(Duration::from_secs(2));
    Ok(())
}

fn backend_requires_privileged(backend: Option<&str>) -> bool {
    matches!(backend, Some("monoio" | "meguri" | "compio"))
}

fn stop_server_container(config: &CommonRunConfig, variant: &RunVariant) {
    let container_name = container_name(&variant.implementation.id);
    println!(">>> Stopping {} server", variant.variant_label);
    match config.deployment_mode {
        DeploymentMode::Local => {
            let _ = run_local(&format!(
                "docker rm -f {container_name} >/dev/null 2>&1 || true"
            ));
        }
        DeploymentMode::Remote => {
            let _ = ssh_server(
                config,
                &format!("docker rm -f {container_name} >/dev/null 2>&1 || true"),
            );
        }
    }
}

fn wait_for_server(
    config: &CommonRunConfig,
    implementation: &ImplementationSpec,
    timeout: Duration,
) -> Result<(), String> {
    let host = config.server_private_ip.as_deref().unwrap_or("127.0.0.1");
    let addr = format!("{host}:{}", config.port);
    let health_path = implementation.health_path();
    println!("    Waiting for {addr}...");

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let ok = match config.deployment_mode {
            DeploymentMode::Local => http_health_check(host, config.port, health_path),
            DeploymentMode::Remote => ssh_health_check(
                &config.ssh_user,
                config.server_ssh.as_deref().unwrap_or_default(),
                host,
                config.port,
                health_path,
            ),
        };
        if ok {
            println!("    Health endpoint passed");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }

    Err(format!(
        "server on {addr} did not pass GET {health_path} within {timeout:?}",
    ))
}

fn run_spinr_bench(
    config: &CommonRunConfig,
    key: &str,
    rendered_template: &Path,
    output_path: &Path,
) -> BenchRunResult {
    let cmd = match config.deployment_mode {
        DeploymentMode::Local => format!(
            "docker run --rm --network host --ulimit nofile=65535:65535 -v {}:/bench.toml {DEFAULT_SPINR_IMAGE} bench /bench.toml -j",
            rendered_template.display()
        ),
        DeploymentMode::Remote => {
            let remote_config = format!("/tmp/{key}.toml");
            scp_to_client(config, rendered_template, &remote_config);
            format!(
                "docker run --rm --network host --ulimit nofile=65535:65535 -v {remote_config}:/bench.toml {DEFAULT_SPINR_IMAGE} bench /bench.toml -j"
            )
        }
    };

    let result = match config.deployment_mode {
        DeploymentMode::Local => run_local(&cmd),
        DeploymentMode::Remote => ssh_client(config, &cmd),
    };

    if config.deployment_mode == DeploymentMode::Remote {
        cleanup_remote_file(config, &format!("/tmp/{key}.toml"));
    }

    match result {
        Ok(out) if out.status.success() => {
            let _ = fs::write(output_path, &out.stdout);
            let parsed: Option<Value> = serde_json::from_slice(&out.stdout).ok();
            if let Some(ref value) = parsed {
                let metrics = spinr_metrics(value);
                let success_rate = validation_success_rate(metrics);
                println!(
                    "    -> rps={} p99={}ms success={:.1}%",
                    val_str(metrics, "rps"),
                    val_str(metrics, "latency_p99_ms"),
                    success_rate * 100.0
                );
                if let Err(error) = validate_spinr_metrics(metrics) {
                    return BenchRunResult {
                        value: parsed,
                        error: Some(error),
                        raw_output_path: output_path.to_path_buf(),
                    };
                }
                return BenchRunResult {
                    value: parsed,
                    error: None,
                    raw_output_path: output_path.to_path_buf(),
                };
            }

            BenchRunResult {
                value: None,
                error: Some("spinr returned non-JSON output".into()),
                raw_output_path: output_path.to_path_buf(),
            }
        }
        Ok(out) => BenchRunResult {
            value: None,
            error: Some(format!(
                "spinr benchmark failed (exit {}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            raw_output_path: output_path.to_path_buf(),
        },
        Err(error) => BenchRunResult {
            value: None,
            error: Some(format!("failed to run spinr benchmark: {error}")),
            raw_output_path: output_path.to_path_buf(),
        },
    }
}

fn run_wrk3_bench(config: &CommonRunConfig, case: &CaseSpec, output_path: &Path) -> BenchRunResult {
    let duration = case.duration_secs.unwrap_or(config.duration_secs);
    let warmup = case.warmup_secs.unwrap_or(config.warmup_secs);
    let connections = resolved_concurrency(case).unwrap_or(128);
    let threads = std::cmp::min(connections, 12);
    let rate = case.rate.unwrap_or(50_000);
    let server_ip = config.server_private_ip.as_deref().unwrap_or("127.0.0.1");
    let url = case
        .context
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/");
    let base_url = format!("http://{server_ip}:{}{url}", config.port);

    // Build -H flags from context.headers (array of "Key: Value" strings)
    let header_flags = case
        .context
        .get("headers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|h| h.as_str())
                .map(|h| {
                    let escaped = h.replace('\'', "'\\''");
                    format!(" -H '{escaped}'")
                })
                .collect::<String>()
        })
        .unwrap_or_default();

    // wrk3 doesn't have a separate warmup flag — run a short warmup pass first
    if warmup > 0 {
        let warmup_cmd = format!(
            "docker run --rm --network host {DEFAULT_WRK3_IMAGE} -t{threads} -c{connections} -d{warmup}s -R{rate}{header_flags} {base_url}"
        );
        let _ = match config.deployment_mode {
            DeploymentMode::Local => run_local(&warmup_cmd),
            DeploymentMode::Remote => ssh_client(config, &warmup_cmd),
        };
    }

    let cmd = format!(
        "docker run --rm --network host {DEFAULT_WRK3_IMAGE} -t{threads} -c{connections} -d{duration}s -R{rate} -L{header_flags} {base_url}"
    );
    let result = match config.deployment_mode {
        DeploymentMode::Local => run_local(&cmd),
        DeploymentMode::Remote => ssh_client(config, &cmd),
    };

    match result {
        Ok(out) if out.status.success() => {
            let _ = fs::write(output_path, &out.stdout);
            let raw = String::from_utf8_lossy(&out.stdout);

            match parse_wrk3_output(&raw) {
                Some(value) => {
                    let metrics = spinr_metrics(&value);
                    let success_rate = validation_success_rate(metrics);
                    println!(
                        "    -> rps={} p99={}ms success={:.1}%",
                        val_str(metrics, "rps"),
                        val_str(metrics, "latency_p99_ms"),
                        success_rate * 100.0
                    );
                    if let Err(error) = validate_spinr_metrics(metrics) {
                        return BenchRunResult {
                            value: Some(value),
                            error: Some(error),
                            raw_output_path: output_path.to_path_buf(),
                        };
                    }
                    BenchRunResult {
                        value: Some(value),
                        error: None,
                        raw_output_path: output_path.to_path_buf(),
                    }
                }
                None => BenchRunResult {
                    value: None,
                    error: Some("failed to parse wrk3 output".into()),
                    raw_output_path: output_path.to_path_buf(),
                },
            }
        }
        Ok(out) => BenchRunResult {
            value: None,
            error: Some(format!(
                "wrk3 benchmark failed (exit {}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            raw_output_path: output_path.to_path_buf(),
        },
        Err(error) => BenchRunResult {
            value: None,
            error: Some(format!("failed to run wrk3 benchmark: {error}")),
            raw_output_path: output_path.to_path_buf(),
        },
    }
}

/// Parse wrk3 text output into the spinr-compatible JSON format.
///
/// wrk3 with `-L` outputs an HdrHistogram percentile table like:
/// ```text
///   50.000%    1.23ms
///   75.000%    2.34ms
///   90.000%    3.45ms
///   99.000%    5.67ms
///   99.900%    8.90ms
///   99.990%   12.34ms
///   99.999%   56.78ms
///  100.000%  123.45ms
/// ```
/// and a summary line like:
/// ```text
///   12345 requests in 30.00s, 1.23MB read
/// Requests/sec:  12345.67
/// Transfer/sec:    123.45KB
/// ```
fn parse_wrk3_output(raw: &str) -> Option<Value> {
    let mut p50 = 0.0_f64;
    let mut p75 = 0.0_f64;
    let mut p90 = 0.0_f64;
    let mut p95 = 0.0_f64;
    let mut p99 = 0.0_f64;
    let mut p999 = 0.0_f64;
    let mut p9999 = 0.0_f64;
    let mut max_latency = 0.0_f64;
    let mut rps = 0.0_f64;
    let mut total_requests = 0_u64;
    let mut errors = 0_u64;

    for line in raw.lines() {
        let trimmed = line.trim();

        // Parse summary percentile lines: "  50.000%    1.23ms"
        if let Some(pct_end) = trimmed.find('%') {
            let pct_str = trimmed[..pct_end].trim();
            if let Ok(pct) = pct_str.parse::<f64>()
                && let Some(latency_ms) = parse_wrk3_latency(&trimmed[pct_end + 1..])
            {
                if (pct - 50.0).abs() < 0.01 {
                    p50 = latency_ms;
                } else if (pct - 75.0).abs() < 0.01 {
                    p75 = latency_ms;
                } else if (pct - 90.0).abs() < 0.01 {
                    p90 = latency_ms;
                } else if (pct - 95.0).abs() < 0.01 {
                    p95 = latency_ms;
                } else if (pct - 99.0).abs() < 0.01 {
                    p99 = latency_ms;
                } else if (pct - 99.9).abs() < 0.01 {
                    p999 = latency_ms;
                } else if (pct - 99.99).abs() < 0.001 {
                    p9999 = latency_ms;
                } else if (pct - 100.0).abs() < 0.001 {
                    max_latency = latency_ms;
                }
            }
        }

        // Parse detailed spectrum lines for p95 if the summary block omits it:
        // "  2.503     0.950000        94647        20.00"
        if p95 == 0.0 && !trimmed.contains('%') && !trimmed.is_empty() {
            let mut words = trimmed.split_whitespace();
            if let (Some(val_str), Some(pct_str)) = (words.next(), words.next())
                && words.count() == 2
                && let (Ok(value_ms), Ok(pct)) = (val_str.parse::<f64>(), pct_str.parse::<f64>())
                && (pct - 0.95).abs() < 0.001
            {
                p95 = value_ms;
            }
        }

        if let Some(val) = trimmed.strip_prefix("Requests/sec:") {
            rps = val.trim().parse().unwrap_or(0.0);
        }

        if trimmed.contains("requests in")
            && let Some(count_str) = trimmed.split_whitespace().next()
        {
            total_requests = count_str.parse().unwrap_or(0);
        }

        // Parse "Socket errors: connect 0, read 5, write 0, timeout 2"
        if let Some(rest) = trimmed.strip_prefix("Socket errors:") {
            for part in rest.split(',') {
                errors += part
                    .split_whitespace()
                    .filter_map(|w| w.parse::<u64>().ok())
                    .next()
                    .unwrap_or(0);
            }
        }

        if trimmed.starts_with("Non-2xx")
            && let Some(count) = trimmed.split_whitespace().last()
        {
            errors += count.parse::<u64>().unwrap_or(0);
        }
    }

    if total_requests == 0 && rps == 0.0 {
        return None;
    }

    let successful = total_requests.saturating_sub(errors);

    Some(serde_json::json!({
        "scenarios": [{
            "metrics": {
                "rps": rps,
                "latency_p50_ms": p50,
                "latency_p75_ms": p75,
                "latency_p90_ms": p90,
                "latency_p95_ms": p95,
                "latency_p99_ms": p99,
                "latency_p999_ms": p999,
                "latency_p9999_ms": p9999,
                "latency_max_ms": max_latency,
                "total_requests": total_requests,
                "successful_requests": successful,
                "failed_requests": errors,
                "status_codes": {
                    "200": successful
                }
            }
        }]
    }))
}

/// Parse a wrk3 latency value like "1.23ms", "45.67us", "1.23s"
fn parse_wrk3_latency(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(val) = s.strip_suffix("ms") {
        val.trim().parse().ok()
    } else if let Some(val) = s.strip_suffix("us") {
        val.trim().parse::<f64>().ok().map(|v| v / 1000.0)
    } else if let Some(val) = s.strip_suffix('s') {
        val.trim().parse::<f64>().ok().map(|v| v * 1000.0)
    } else {
        None
    }
}

fn write_canonical_report(
    mode: RunMode,
    config: &CommonRunConfig,
    suite: &SuiteSpec,
    results_dir: &Path,
    started_at_utc: &str,
    completed_at_utc: &str,
    case_records: &[CaseExecutionRecord],
) -> Result<(), String> {
    let mut cases = Vec::with_capacity(case_records.len());
    for record in case_records {
        let mut results = Vec::with_capacity(record.results.len());
        for run in &record.results {
            results.push(ImplementationResult {
                implementation_id: run.variant.implementation.id.clone(),
                compare_side: run.variant.compare_side,
                started_at_utc: run.started_at_utc.clone(),
                completed_at_utc: run.completed_at_utc.clone(),
                image: ImageDescriptor {
                    tag: run.variant.implementation.image.clone(),
                    id: run.image_id.clone(),
                },
                labels: ImplementationLabels {
                    framework: run.variant.implementation.framework_label().to_string(),
                    backend: run.variant.implementation.backend_label().to_string(),
                    profile: run.variant.implementation.profile_label().to_string(),
                },
                metrics: canonical_metrics(&run.metrics),
                artifacts: ResultArtifacts {
                    loadgen_raw: relative_display(results_dir, &run.raw_metrics_path),
                },
                os: None,
                perf: None,
            });
        }

        cases.push(CaseReport {
            id: record.case.id.clone(),
            generator: record.case.generator,
            template: TemplateFiles {
                source: record
                    .case
                    .resolved_template_path(&config.suite_path)
                    .display()
                    .to_string(),
                rendered: relative_display(results_dir, &record.rendered_template),
            },
            load: LoadProfile {
                concurrency: resolved_concurrency(&record.case),
                rate: record.case.rate,
                duration_secs: record.case.duration_secs.unwrap_or(config.duration_secs),
                warmup_secs: record.case.warmup_secs.unwrap_or(config.warmup_secs),
            },
            results,
        });
    }

    let report = RunReport {
        schema_version: RUN_SCHEMA_VERSION,
        run_mode: mode,
        deployment_mode: config.deployment_mode,
        suite: SuiteDescriptor {
            name: suite.name.clone(),
            path: config.suite_path.display().to_string(),
        },
        targets: TargetDescriptor {
            server_host: config
                .server_ssh
                .clone()
                .unwrap_or_else(|| "localhost".into()),
            client_host: config
                .client_ssh
                .clone()
                .unwrap_or_else(|| "localhost".into()),
            server_private_ip: config
                .server_private_ip
                .clone()
                .unwrap_or_else(|| "127.0.0.1".into()),
            port: config.port,
        },
        defaults: RunDefaults {
            duration_secs: config.duration_secs,
            warmup_secs: config.warmup_secs,
        },
        started_at_utc: started_at_utc.to_string(),
        completed_at_utc: completed_at_utc.to_string(),
        git: GitDescriptor {
            sha: git_sha(),
            dirty: git_dirty(),
        },
        cases,
    };

    let path = results_dir.join("run.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&report).unwrap_or_default(),
    )
    .map_err(|e| format!("failed to write canonical report {}: {e}", path.display()))
}

fn canonical_metrics(value: &Value) -> BenchmarkMetrics {
    let metrics = spinr_metrics(value);
    BenchmarkMetrics {
        rps: metric_f64(metrics, "rps"),
        success_rate: validation_success_rate(metrics),
        status_codes: status_code_map(metrics),
        latency_ms: LatencyMetrics {
            p50: metric_f64(metrics, "latency_p50_ms"),
            p95: metric_f64(metrics, "latency_p95_ms"),
            p99: metric_f64(metrics, "latency_p99_ms"),
            p999: metric_f64(metrics, "latency_p999_ms"),
            max: metric_f64(metrics, "latency_max_ms"),
        },
    }
}

fn image_id(config: &CommonRunConfig, image: &str) -> Option<String> {
    let cmd = format!("docker image inspect -f '{{{{.Id}}}}' {image}");
    let output = match config.deployment_mode {
        DeploymentMode::Local => run_local(&cmd).ok()?,
        DeploymentMode::Remote => ssh_server(config, &cmd).ok()?,
    };
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn relative_display(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn resolved_concurrency(case: &CaseSpec) -> Option<u32> {
    case.concurrency.or_else(|| {
        case.context
            .get("connections")
            .or_else(|| case.context.get("concurrency"))
            .and_then(|value| value.as_integer())
            .map(|value| value as u32)
    })
}

fn run_key(variant_label: &str, case_id: &str) -> String {
    format!(
        "{}_{}",
        sanitize_label(variant_label),
        sanitize_label(case_id)
    )
}

fn container_name(label: &str) -> String {
    format!("bench-{}", sanitize_label(label))
}

fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

fn status_code_map(metrics: &Value) -> BTreeMap<String, u64> {
    metrics
        .get("status_codes")
        .and_then(Value::as_object)
        .map(|codes| {
            codes
                .iter()
                .filter_map(|(status, count)| count.as_u64().map(|count| (status.clone(), count)))
                .collect()
        })
        .unwrap_or_default()
}

fn ssh_server(config: &CommonRunConfig, remote_cmd: &str) -> std::io::Result<Output> {
    ssh_run(
        &config.ssh_user,
        config.server_ssh.as_deref().unwrap_or_default(),
        remote_cmd,
    )
}

fn ssh_client(config: &CommonRunConfig, remote_cmd: &str) -> std::io::Result<Output> {
    ssh_run(
        &config.ssh_user,
        config.client_ssh.as_deref().unwrap_or_default(),
        remote_cmd,
    )
}

fn scp_to_client(config: &CommonRunConfig, local_path: &Path, remote_path: &str) {
    if let Some(host) = config.client_ssh.as_deref() {
        scp_to_remote(&config.ssh_user, host, local_path, remote_path);
    }
}

fn cleanup_remote_file(config: &CommonRunConfig, remote_path: &str) {
    let _ = ssh_client(config, &format!("rm -f {remote_path}"));
}

fn git_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn git_dirty() -> bool {
    Command::new("git")
        .args(["status", "--short"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty())
}
