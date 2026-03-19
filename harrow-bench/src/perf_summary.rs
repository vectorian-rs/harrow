use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
struct RunMeta {
    client_host: String,
    completed_at_utc: String,
    concurrency: u32,
    duration_secs: u32,
    framework: String,
    key: String,
    os_monitors: bool,
    path: String,
    perf_scope: String,
    server_host: String,
    spinr_mode: String,
    started_at_utc: String,
    test_case: String,
    warmup_secs: u32,
}

#[derive(Clone, Default)]
struct BenchMetrics {
    rps: f64,
    p50: f64,
    p99: f64,
    p999: f64,
}

#[derive(Clone, Default)]
struct CpuSummary {
    user: f64,
    system: f64,
    iowait: f64,
    idle: f64,
}

#[derive(Clone, Default)]
struct NetSummary {
    rx_kb_s: f64,
    tx_kb_s: f64,
    retrans_s: f64,
}

#[derive(Clone, Default)]
struct TelemetrySeries {
    cpu_busy_pct: Vec<f64>,
    net_tx_mb_s: Vec<f64>,
    retrans_s: Vec<f64>,
    run_queue: Vec<f64>,
    context_switches_s: Vec<f64>,
}

#[derive(Clone)]
struct PerfHotspot {
    pct: f64,
    shared_object: String,
    symbol: String,
}

#[derive(Clone)]
struct RunSummary {
    meta: RunMeta,
    metrics: BenchMetrics,
    server_cpu: Option<CpuSummary>,
    client_cpu: Option<CpuSummary>,
    server_net: Option<NetSummary>,
    client_net: Option<NetSummary>,
    telemetry: TelemetrySeries,
    perf_hotspots: Vec<PerfHotspot>,
}

#[derive(Clone)]
struct ReportSummary {
    instance_type: String,
    timestamp: String,
    server_host: String,
    client_host: String,
    duration_secs: u32,
    warmup_secs: u32,
    spinr_mode: String,
    os_monitors: bool,
    perf_scope: String,
    completed_at_utc: String,
    runs: Vec<RunSummary>,
}

const CANVAS: &str = "#fafafa";
const SURFACE: &str = "#ffffff";
const BORDER_LIGHT: &str = "#e2e8f0";
const TEXT_PRIMARY: &str = "#1e293b";
const TEXT_SECONDARY: &str = "#64748b";
const TEXT_MUTED: &str = "#94a3b8";
const ACCENT_ORANGE: &str = "#ea580c";
const AXUM_GRAY: &str = "#64748b";
const HARROW_FILL: &str = "#fff7ed";
const AXUM_FILL: &str = "#f8fafc";

pub fn render_results_dir(results_dir: &Path) -> io::Result<()> {
    let summary = load_results_dir(results_dir)?;
    generate_local_flamegraphs(results_dir, &summary.runs)?;
    generate_telemetry_svgs(results_dir, &summary)?;
    let svg = render_svg(&summary);
    fs::write(results_dir.join("summary.svg"), svg)?;
    fs::write(results_dir.join("summary.md"), render_markdown(results_dir, &summary))?;
    Ok(())
}

fn load_results_dir(results_dir: &Path) -> io::Result<ReportSummary> {
    let mut metas = Vec::new();
    for entry in fs::read_dir(results_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".meta.json"))
        {
            metas.push(load_meta(&path)?);
        }
    }

    metas.sort_by(|a, b| {
        a.test_case
            .cmp(&b.test_case)
            .then_with(|| a.framework.cmp(&b.framework))
    });

    if metas.is_empty() {
        return Err(io::Error::other(format!(
            "no *.meta.json files found in {}",
            results_dir.display()
        )));
    }

    let mut runs = Vec::with_capacity(metas.len());
    for meta in metas {
        let key = meta.key.clone();
        runs.push(RunSummary {
            metrics: load_metrics(results_dir.join(format!("{key}.json")))?,
            server_cpu: parse_cpu_summary(&results_dir.join(format!("{key}.server.sar-u.txt"))),
            client_cpu: parse_cpu_summary(&results_dir.join(format!("{key}.client.sar-u.txt"))),
            server_net: parse_net_summary(&results_dir.join(format!("{key}.server.sar-net.txt"))),
            client_net: parse_net_summary(&results_dir.join(format!("{key}.client.sar-net.txt"))),
            telemetry: parse_telemetry_series(
                &results_dir.join(format!("{key}.server.sar-u.txt")),
                &results_dir.join(format!("{key}.server.sar-net.txt")),
                &results_dir.join(format!("{key}.server.vmstat.txt")),
            ),
            perf_hotspots: parse_perf_hotspots(
                &results_dir.join(format!("{key}.server.perf-report.txt")),
            ),
            meta,
        });
    }

    let first = runs[0].meta.clone();
    let instance_type = results_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let timestamp = results_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let completed_at_utc = runs
        .iter()
        .map(|run| run.meta.completed_at_utc.as_str())
        .max()
        .unwrap_or("unknown")
        .to_string();

    Ok(ReportSummary {
        instance_type,
        timestamp,
        server_host: first.server_host,
        client_host: first.client_host,
        duration_secs: first.duration_secs,
        warmup_secs: first.warmup_secs,
        spinr_mode: first.spinr_mode,
        os_monitors: first.os_monitors,
        perf_scope: first.perf_scope,
        completed_at_utc,
        runs,
    })
}

fn load_meta(path: &Path) -> io::Result<RunMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn load_metrics(path: PathBuf) -> io::Result<BenchMetrics> {
    let bytes = fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(BenchMetrics {
        rps: value.get("rps").and_then(Value::as_f64).unwrap_or_default(),
        p50: value
            .get("latency_p50_ms")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
        p99: value
            .get("latency_p99_ms")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
        p999: value
            .get("latency_p999_ms")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
    })
}

fn parse_cpu_summary(path: &Path) -> Option<CpuSummary> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() >= 8 && tokens[0] == "Average:" && tokens[1] == "all" {
            return Some(CpuSummary {
                user: parse_f64(tokens[2]),
                system: parse_f64(tokens[4]),
                iowait: parse_f64(tokens[5]),
                idle: parse_f64(tokens[7]),
            });
        }
    }
    None
}

fn parse_net_summary(path: &Path) -> Option<NetSummary> {
    enum Section {
        None,
        Iface,
        TcpErr,
    }

    let text = fs::read_to_string(path).ok()?;
    let mut section = Section::None;
    let mut summary = NetSummary::default();
    let mut saw_eth0 = false;
    let mut saw_tcp = false;

    for line in text.lines() {
        if line.contains("IFACE") {
            section = Section::Iface;
            continue;
        }
        if line.contains("atmptf/s") {
            section = Section::TcpErr;
            continue;
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        if !tokens.first().is_some_and(|t| *t == "Average:") {
            continue;
        }

        match section {
            Section::Iface if tokens.len() >= 10 && tokens[1] == "eth0" => {
                summary.rx_kb_s = parse_f64(tokens[4]);
                summary.tx_kb_s = parse_f64(tokens[5]);
                saw_eth0 = true;
            }
            Section::TcpErr if tokens.len() >= 6 => {
                summary.retrans_s = parse_f64(tokens[3]);
                saw_tcp = true;
            }
            Section::None | Section::Iface | Section::TcpErr => {}
        }
    }

    if saw_eth0 || saw_tcp {
        Some(summary)
    } else {
        None
    }
}

fn parse_telemetry_series(cpu_path: &Path, net_path: &Path, vmstat_path: &Path) -> TelemetrySeries {
    let mut series = TelemetrySeries::default();
    series.cpu_busy_pct = parse_cpu_busy_series(cpu_path);
    let (net_tx_mb_s, retrans_s) = parse_net_series(net_path);
    series.net_tx_mb_s = net_tx_mb_s;
    series.retrans_s = retrans_s;
    let (run_queue, context_switches_s) = parse_vmstat_series(vmstat_path);
    series.run_queue = run_queue;
    series.context_switches_s = context_switches_s;
    series
}

fn parse_cpu_busy_series(path: &Path) -> Vec<f64> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() >= 8 && tokens[1] == "all" && tokens[0] != "Average:" {
            out.push(parse_f64(tokens[2]) + parse_f64(tokens[4]) + parse_f64(tokens[5]));
        }
    }
    out
}

fn parse_net_series(path: &Path) -> (Vec<f64>, Vec<f64>) {
    enum Section {
        None,
        Iface,
        TcpErr,
    }

    let Ok(text) = fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };

    let mut section = Section::None;
    let mut tx_mb_s = Vec::new();
    let mut retrans_s = Vec::new();

    for line in text.lines() {
        if line.contains("IFACE") {
            section = Section::Iface;
            continue;
        }
        if line.contains("atmptf/s") {
            section = Section::TcpErr;
            continue;
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 || tokens[0] == "Average:" {
            continue;
        }

        match section {
            Section::Iface if tokens.len() >= 10 && tokens[1] == "eth0" => {
                tx_mb_s.push(parse_f64(tokens[5]) / 1024.0);
            }
            Section::TcpErr if tokens.len() >= 6 => {
                retrans_s.push(parse_f64(tokens[3]));
            }
            Section::None | Section::Iface | Section::TcpErr => {}
        }
    }

    (tx_mb_s, retrans_s)
}

fn parse_vmstat_series(path: &Path) -> (Vec<f64>, Vec<f64>) {
    let Ok(text) = fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };

    let mut run_queue = Vec::new();
    let mut context_switches_s = Vec::new();

    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 17 {
            continue;
        }
        if tokens[0].parse::<f64>().is_err() {
            continue;
        }
        run_queue.push(parse_f64(tokens[0]));
        context_switches_s.push(parse_f64(tokens[11]));
    }

    (run_queue, context_switches_s)
}

fn parse_perf_hotspots(path: &Path) -> Vec<PerfHotspot> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() < 5 {
            continue;
        }

        let (pct_idx, shared_idx, symbol_idx) = if tokens
            .get(1)
            .is_some_and(|token| token.ends_with('%'))
        {
            (0usize, 3usize, 5usize)
        } else {
            (0usize, 2usize, 4usize)
        };

        if tokens.len() <= symbol_idx {
            continue;
        }

        let shared_object = tokens[shared_idx];
        if shared_object == "[kernel.kallsyms]" {
            continue;
        }

        let symbol = tokens[symbol_idx..].join(" ");
        if symbol.starts_with("0x") {
            continue;
        }

        let key = format!("{shared_object}:{symbol}");
        if !seen.insert(key) {
            continue;
        }

        out.push(PerfHotspot {
            pct: parse_pct(tokens[pct_idx]),
            shared_object: shared_object.to_string(),
            symbol,
        });

        if out.len() == 3 {
            break;
        }
    }

    out
}

fn parse_f64(input: &str) -> f64 {
    input.parse::<f64>().unwrap_or_default()
}

fn parse_pct(input: &str) -> f64 {
    parse_f64(input.trim_end_matches('%'))
}

fn render_markdown(results_dir: &Path, summary: &ReportSummary) -> String {
    let mut out = String::new();
    writeln!(&mut out, "# Performance Test Results").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(&mut out, "![Run Dashboard](summary.svg)").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(&mut out, "Instance: {}", summary.instance_type).unwrap();
    writeln!(
        &mut out,
        "Server: {}",
        summary.server_host
    )
    .unwrap();
    writeln!(
        &mut out,
        "Client: {}",
        summary.client_host
    )
    .unwrap();
    writeln!(
        &mut out,
        "Duration: {}s | Warmup: {}s",
        summary.duration_secs, summary.warmup_secs
    )
    .unwrap();
    writeln!(&mut out, "Spinr mode: {}", summary.spinr_mode).unwrap();
    writeln!(&mut out, "OS monitors: {}", summary.os_monitors).unwrap();
    writeln!(&mut out, "Perf: {}", summary.perf_scope).unwrap();
    writeln!(&mut out, "Date: {}", summary.completed_at_utc).unwrap();
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "## Runs").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "| Test case | Framework | Path | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |"
    )
    .unwrap();
    writeln!(
        &mut out,
        "|-----------|-----------|------|-------------|-----|----------|----------|-----------|"
    )
    .unwrap();
    for run in &summary.runs {
        writeln!(
            &mut out,
            "| {} | {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} |",
            run.meta.test_case,
            run.meta.framework,
            run.meta.path,
            run.meta.concurrency,
            run.metrics.rps,
            run.metrics.p50,
            run.metrics.p99,
            run.metrics.p999
        )
        .unwrap();
    }
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "## Comparison").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "| Test case | Harrow RPS | Axum RPS | Delta % | Harrow p99 (ms) | Axum p99 (ms) |"
    )
    .unwrap();
    writeln!(
        &mut out,
        "|-----------|------------|----------|---------|------------------|---------------|"
    )
    .unwrap();

    for (test_case, pair) in grouped_runs(&summary.runs) {
        let harrow = pair.get("harrow");
        let axum = pair.get("axum");
        let delta = match (harrow, axum) {
            (Some(h), Some(a)) if a.metrics.rps != 0.0 => ((h.metrics.rps - a.metrics.rps) / a.metrics.rps) * 100.0,
            _ => 0.0,
        };
        writeln!(
            &mut out,
            "| {} | {} | {} | {:+.2}% | {} | {} |",
            test_case,
            harrow
                .map(|run| format!("{:.3}", run.metrics.rps))
                .unwrap_or_else(|| "-".into()),
            axum
                .map(|run| format!("{:.3}", run.metrics.rps))
                .unwrap_or_else(|| "-".into()),
            delta,
            harrow
                .map(|run| format!("{:.3}", run.metrics.p99))
                .unwrap_or_else(|| "-".into()),
            axum
                .map(|run| format!("{:.3}", run.metrics.p99))
                .unwrap_or_else(|| "-".into()),
        )
        .unwrap();
    }
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "## Telemetry Digest").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "| Run | Server CPU (user/sys/wait/idle) | Client CPU (user/sys/wait/idle) | Server Net (rx/tx MB/s, retrans/s) | Client Net (rx/tx MB/s, retrans/s) | Top Perf Hotspot |"
    )
    .unwrap();
    writeln!(
        &mut out,
        "|-----|----------------------------------|----------------------------------|------------------------------------|------------------------------------|------------------|"
    )
    .unwrap();
    for run in &summary.runs {
        let server_cpu = format_cpu(run.server_cpu.as_ref());
        let client_cpu = format_cpu(run.client_cpu.as_ref());
        let server_net = format_net(run.server_net.as_ref());
        let client_net = format_net(run.client_net.as_ref());
        let perf = run
            .perf_hotspots
            .first()
            .map(|hotspot| {
                format!(
                    "{:.2}% {} ({})",
                    hotspot.pct, hotspot.symbol, hotspot.shared_object
                )
            })
            .unwrap_or_else(|| "-".into());
        writeln!(
            &mut out,
            "| {} | {} | {} | {} | {} | {} |",
            run.meta.key, server_cpu, client_cpu, server_net, client_net, perf
        )
        .unwrap();
    }
    writeln!(&mut out).unwrap();

    let telemetry_runs: Vec<&str> = grouped_runs(&summary.runs)
        .keys()
        .copied()
        .filter(|test_case| results_dir.join(telemetry_svg_filename(test_case)).exists())
        .collect();

    if !telemetry_runs.is_empty() {
        writeln!(&mut out, "## Telemetry Charts").unwrap();
        writeln!(&mut out).unwrap();
        for test_case in telemetry_runs {
            writeln!(&mut out, "### {}", test_case).unwrap();
            writeln!(&mut out).unwrap();
            writeln!(
                &mut out,
                "![{} telemetry](./{})",
                test_case,
                telemetry_svg_filename(test_case)
            )
            .unwrap();
            writeln!(&mut out).unwrap();
        }
    }

    writeln!(&mut out, "## Artifacts").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "| Run | JSON | Perf Report | Perf Script | Perf SVG | Server CPU | Server Net | Client CPU | Client Net |"
    )
    .unwrap();
    writeln!(
        &mut out,
        "|-----|------|-------------|-------------|----------|------------|------------|------------|------------|"
    )
    .unwrap();
    for run in &summary.runs {
        let key = &run.meta.key;
        writeln!(
            &mut out,
            "| {} | [{}](./{}.json) | [{}](./{}.server.perf-report.txt) | [{}](./{}.server.perf.script) | {} | [{}](./{}.server.sar-u.txt) | [{}](./{}.server.sar-net.txt) | [{}](./{}.client.sar-u.txt) | [{}](./{}.client.sar-net.txt) |",
            key,
            "json",
            key,
            "perf-report",
            key,
            "perf-script",
            key,
            if results_dir.join(format!("{key}.server.perf.svg")).exists() {
                format!("[perf.svg](./{key}.server.perf.svg)")
            } else {
                "-".into()
            },
            "server cpu",
            key,
            "server net",
            key,
            "client cpu",
            key,
            "client net",
            key
        )
        .unwrap();
    }

    let flamegraph_runs: Vec<&RunSummary> = summary
        .runs
        .iter()
        .filter(|run| {
            results_dir
                .join(format!("{}.server.perf.svg", run.meta.key))
                .exists()
        })
        .collect();

    if !flamegraph_runs.is_empty() {
        writeln!(&mut out).unwrap();
        writeln!(&mut out, "## Flamegraphs").unwrap();
        writeln!(&mut out).unwrap();
        for run in flamegraph_runs {
            writeln!(&mut out, "### {}", run.meta.key).unwrap();
            writeln!(&mut out).unwrap();
            writeln!(
                &mut out,
                "![{} flamegraph](./{}.server.perf.svg)",
                run.meta.key, run.meta.key
            )
            .unwrap();
            writeln!(&mut out).unwrap();
        }
    }

    out
}

fn render_svg(summary: &ReportSummary) -> String {
    let width = 1400.0;
    let throughput_panel_height = 120.0 + (grouped_runs(&summary.runs).len() as f64 * 72.0);
    let p99_panel_height = throughput_panel_height;
    let card_height = 228.0;
    let card_gap = 20.0;
    let rows = summary.runs.len().div_ceil(2) as f64;
    let cards_height = rows * (card_height + card_gap);
    let height = 180.0 + throughput_panel_height.max(p99_panel_height) + 40.0 + cards_height + 60.0;

    let mut svg = String::new();
    writeln!(
        &mut svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" fill="none">"##
    )
    .unwrap();
    writeln!(
        &mut svg,
        r##"<rect x="0" y="0" width="{width:.0}" height="{height:.0}" fill="{CANVAS}"/>"##
    )
    .unwrap();

    svg.push_str(&mono_text(
        44.0,
        52.0,
        18,
        700,
        TEXT_PRIMARY,
        &format!("run dashboard :: {}", summary.timestamp),
    ));
    svg.push_str(&ui_text(
        44.0,
        82.0,
        14,
        500,
        TEXT_SECONDARY,
        &format!(
            "{} · server {} · client {} · {}s run / {}s warmup · {}",
            summary.instance_type,
            summary.server_host,
            summary.client_host,
            summary.duration_secs,
            summary.warmup_secs,
            summary.perf_scope
        ),
    ));

    let panel_y = 120.0;
    let panel_gap = 32.0;
    let panel_w = (width - 44.0 * 2.0 - panel_gap) / 2.0;
    let throughput_x = 44.0;
    let p99_x = throughput_x + panel_w + panel_gap;
    svg.push_str(&panel_card(
        throughput_x,
        panel_y,
        panel_w,
        throughput_panel_height,
        ACCENT_ORANGE,
    ));
    svg.push_str(&panel_card(
        p99_x,
        panel_y,
        panel_w,
        p99_panel_height,
        ACCENT_ORANGE,
    ));
    svg.push_str(&mono_text(
        throughput_x + 24.0,
        panel_y + 32.0,
        18,
        700,
        ACCENT_ORANGE,
        "throughput",
    ));
    svg.push_str(&mono_text(
        p99_x + 24.0,
        panel_y + 32.0,
        18,
        700,
        ACCENT_ORANGE,
        "p99 latency",
    ));

    let grouped = grouped_runs(&summary.runs);
    let max_rps = summary
        .runs
        .iter()
        .map(|run| run.metrics.rps)
        .fold(0.0, f64::max)
        .max(1.0);
    let max_p99 = summary
        .runs
        .iter()
        .map(|run| run.metrics.p99)
        .fold(0.0, f64::max)
        .max(1.0);

    for (idx, (test_case, pair)) in grouped.iter().enumerate() {
        let base_y = panel_y + 72.0 + idx as f64 * 72.0;
        svg.push_str(&mono_text(
            throughput_x + 24.0,
            base_y,
            12,
            700,
            TEXT_PRIMARY,
            test_case,
        ));
        svg.push_str(&mono_text(
            p99_x + 24.0,
            base_y,
            12,
            700,
            TEXT_PRIMARY,
            test_case,
        ));

        let bars = [
            ("harrow", ACCENT_ORANGE, HARROW_FILL, 0.0),
            ("axum", AXUM_GRAY, AXUM_FILL, 26.0),
        ];

        for (framework, color, fill, offset) in bars {
            if let Some(run) = pair.get(framework) {
                let rps_w = ((run.metrics.rps / max_rps) * (panel_w - 210.0)).max(4.0);
                let p99_w = ((run.metrics.p99 / max_p99) * (panel_w - 210.0)).max(4.0);
                let y = base_y + 12.0 + offset;
                svg.push_str(&mono_text(
                    throughput_x + 24.0,
                    y + 14.0,
                    12,
                    600,
                    color,
                    framework,
                ));
                svg.push_str(&mono_text(p99_x + 24.0, y + 14.0, 12, 600, color, framework));
                svg.push_str(&metric_bar(
                    throughput_x + 92.0,
                    y,
                    rps_w,
                    14.0,
                    color,
                    fill,
                ));
                svg.push_str(&metric_bar(
                    p99_x + 92.0,
                    y,
                    p99_w,
                    14.0,
                    color,
                    fill,
                ));
                svg.push_str(&ui_text(
                    throughput_x + 100.0 + (panel_w - 210.0),
                    y + 13.0,
                    10,
                    500,
                    TEXT_SECONDARY,
                    &format!("{:.0} rps", run.metrics.rps),
                ));
                svg.push_str(&ui_text(
                    p99_x + 100.0 + (panel_w - 210.0),
                    y + 13.0,
                    10,
                    500,
                    TEXT_SECONDARY,
                    &format!("{:.3} ms", run.metrics.p99),
                ));
            }
        }
    }

    let cards_y = panel_y + throughput_panel_height.max(p99_panel_height) + 36.0;
    for (idx, run) in summary.runs.iter().enumerate() {
        let col = (idx % 2) as f64;
        let row = (idx / 2) as f64;
        let x = 44.0 + col * (panel_w + panel_gap);
        let y = cards_y + row * (card_height + card_gap);
        let accent = if run.meta.framework == "harrow" {
            ACCENT_ORANGE
        } else {
            AXUM_GRAY
        };
        svg.push_str(&panel_card(x, y, panel_w, card_height, accent));
        svg.push_str(&mono_text(
            x + 24.0,
            y + 32.0,
            18,
            700,
            accent,
            &format!("{} · {}", run.meta.framework, run.meta.test_case),
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 58.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!("{} · started {}", run.meta.path, run.meta.started_at_utc),
        ));
        svg.push_str(&metric_chip(
            x + 24.0,
            y + 78.0,
            "RPS",
            &format!("{:.0}", run.metrics.rps),
            accent,
        ));
        svg.push_str(&metric_chip(
            x + 192.0,
            y + 78.0,
            "p99",
            &format!("{:.3} ms", run.metrics.p99),
            accent,
        ));
        svg.push_str(&metric_chip(
            x + 332.0,
            y + 78.0,
            "p999",
            &format!("{:.3} ms", run.metrics.p999),
            accent,
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 132.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!("Server CPU {}", format_cpu(run.server_cpu.as_ref())),
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 154.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!("Client CPU {}", format_cpu(run.client_cpu.as_ref())),
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 176.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!("Server Net {}", format_net(run.server_net.as_ref())),
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 198.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!("Client Net {}", format_net(run.client_net.as_ref())),
        ));
        svg.push_str(&ui_text(
            x + 24.0,
            y + 220.0,
            10,
            500,
            TEXT_SECONDARY,
            &format!(
                "Perf {}",
                run.perf_hotspots
                    .first()
                    .map(|hotspot| {
                        format!(
                            "{:.2}% {} ({})",
                            hotspot.pct,
                            trim_text(&hotspot.symbol, 38),
                            hotspot.shared_object
                        )
                    })
                    .unwrap_or_else(|| "no user-space hotspot parsed".into())
            ),
        ));
    }

    svg.push_str("</svg>");
    svg
}

fn grouped_runs<'a>(runs: &'a [RunSummary]) -> BTreeMap<&'a str, BTreeMap<&'a str, &'a RunSummary>> {
    let mut out: BTreeMap<&str, BTreeMap<&str, &RunSummary>> = BTreeMap::new();
    for run in runs {
        out.entry(run.meta.test_case.as_str())
            .or_default()
            .insert(run.meta.framework.as_str(), run);
    }
    out
}

fn format_cpu(cpu: Option<&CpuSummary>) -> String {
    match cpu {
        Some(cpu) => format!(
            "{:.1}% / {:.1}% / {:.1}% / {:.1}%",
            cpu.user, cpu.system, cpu.iowait, cpu.idle
        ),
        None => "-".into(),
    }
}

fn format_net(net: Option<&NetSummary>) -> String {
    match net {
        Some(net) => format!(
            "{:.1} / {:.1} MB/s · retrans {:.2}/s",
            net.rx_kb_s / 1024.0,
            net.tx_kb_s / 1024.0,
            net.retrans_s
        ),
        None => "-".into(),
    }
}

fn panel_card(x: f64, y: f64, w: f64, h: f64, accent: &str) -> String {
    format!(
        r##"<g><rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="8" fill="{SURFACE}" stroke="{BORDER_LIGHT}" stroke-width="1"/><rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="4" rx="8" fill="{accent}"/></g>"##
    )
}

fn metric_chip(x: f64, y: f64, label: &str, value: &str, accent: &str) -> String {
    let label = xml_escape(label);
    let value = xml_escape(value);
    let label_x = x + 14.0;
    let label_y = y + 14.0;
    let value_x = x + 14.0;
    let value_y = y + 29.0;
    let fill = if accent == ACCENT_ORANGE {
        HARROW_FILL
    } else {
        AXUM_FILL
    };
    format!(
        r##"<g><rect x="{x:.1}" y="{y:.1}" width="132" height="38" rx="4" fill="{fill}" stroke="{accent}" stroke-width="1"/><text x="{label_x:.1}" y="{label_y:.1}" font-family="monospace" font-size="12" font-weight="700" fill="{accent}">{label}</text><text x="{value_x:.1}" y="{value_y:.1}" font-family="system-ui, -apple-system, sans-serif" font-size="10" font-weight="500" fill="{TEXT_PRIMARY}">{value}</text></g>"##
    )
}

fn metric_bar(x: f64, y: f64, w: f64, h: f64, stroke: &str, fill: &str) -> String {
    format!(
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="4" fill="{fill}" stroke="{stroke}" stroke-width="1.5"/>"##
    )
}

fn ui_text(x: f64, y: f64, size: u32, weight: u32, fill: &str, value: &str) -> String {
    format!(
        r#"<text x="{x:.1}" y="{y:.1}" font-family="system-ui, -apple-system, sans-serif" font-size="{size}" font-weight="{weight}" fill="{fill}">{}</text>"#,
        xml_escape(value)
    )
}

fn mono_text(x: f64, y: f64, size: u32, weight: u32, fill: &str, value: &str) -> String {
    format!(
        r#"<text x="{x:.1}" y="{y:.1}" font-family="monospace" font-size="{size}" font-weight="{weight}" fill="{fill}">{}</text>"#,
        xml_escape(value)
    )
}

fn trim_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        input.to_string()
    } else {
        let trimmed: String = input.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{trimmed}...")
    }
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn telemetry_svg_filename(test_case: &str) -> String {
    format!("{test_case}.server.telemetry.svg")
}

fn generate_local_flamegraphs(results_dir: &Path, runs: &[RunSummary]) -> io::Result<()> {
    if !local_inferno_available() {
        return Ok(());
    }

    for run in runs {
        let script_path = results_dir.join(format!("{}.server.perf.script", run.meta.key));
        if !script_path.exists() {
            continue;
        }

        let folded_path = results_dir.join(format!("{}.server.perf.folded", run.meta.key));
        let svg_path = results_dir.join(format!("{}.server.perf.svg", run.meta.key));
        if svg_path.exists() {
            continue;
        }
        generate_local_flamegraph(&script_path, &folded_path, &svg_path)?;
    }

    Ok(())
}

fn generate_telemetry_svgs(results_dir: &Path, summary: &ReportSummary) -> io::Result<()> {
    for (test_case, pair) in grouped_runs(&summary.runs) {
        let svg = render_telemetry_svg(test_case, &pair);
        fs::write(results_dir.join(telemetry_svg_filename(test_case)), svg)?;
    }
    Ok(())
}

fn render_telemetry_svg(
    test_case: &str,
    pair: &BTreeMap<&str, &RunSummary>,
) -> String {
    let width = 1400.0;
    let height = 860.0;
    let panel_gap = 28.0;
    let panel_w = (width - 44.0 * 2.0 - panel_gap) / 2.0;
    let panel_h = 260.0;
    let harrow = pair.get("harrow").copied();
    let axum = pair.get("axum").copied();

    let mut svg = String::new();
    writeln!(
        &mut svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" fill="none">"##
    )
    .unwrap();
    writeln!(
        &mut svg,
        r##"<rect x="0" y="0" width="{width:.0}" height="{height:.0}" fill="{CANVAS}"/>"##
    )
    .unwrap();
    svg.push_str(&mono_text(
        44.0,
        52.0,
        18,
        700,
        TEXT_PRIMARY,
        &format!("server telemetry :: {test_case}"),
    ));
    svg.push_str(&ui_text(
        44.0,
        82.0,
        14,
        500,
        TEXT_SECONDARY,
        "Harrow overlays Axum for server-side sar and vmstat signals",
    ));

    svg.push_str(&legend_chip(44.0, 104.0, ACCENT_ORANGE, HARROW_FILL, "harrow"));
    svg.push_str(&legend_chip(156.0, 104.0, AXUM_GRAY, AXUM_FILL, "axum"));

    let top_y = 140.0;
    let left_x = 44.0;
    let right_x = left_x + panel_w + panel_gap;
    let bottom_y = top_y + panel_h + panel_gap;

    svg.push_str(&render_series_panel(
        left_x,
        top_y,
        panel_w,
        panel_h,
        "Server CPU Busy %",
        "%",
        harrow.map_or(&[][..], |run| run.telemetry.cpu_busy_pct.as_slice()),
        axum.map_or(&[][..], |run| run.telemetry.cpu_busy_pct.as_slice()),
    ));
    svg.push_str(&render_series_panel(
        right_x,
        top_y,
        panel_w,
        panel_h,
        "Server Net TX",
        "MB/s",
        harrow.map_or(&[][..], |run| run.telemetry.net_tx_mb_s.as_slice()),
        axum.map_or(&[][..], |run| run.telemetry.net_tx_mb_s.as_slice()),
    ));
    svg.push_str(&render_series_panel(
        left_x,
        bottom_y,
        panel_w,
        panel_h,
        "VMstat Runnable (r)",
        "threads",
        harrow.map_or(&[][..], |run| run.telemetry.run_queue.as_slice()),
        axum.map_or(&[][..], |run| run.telemetry.run_queue.as_slice()),
    ));
    svg.push_str(&render_series_panel(
        right_x,
        bottom_y,
        panel_w,
        panel_h,
        "VMstat Context Switches",
        "/s",
        harrow.map_or(&[][..], |run| run.telemetry.context_switches_s.as_slice()),
        axum.map_or(&[][..], |run| run.telemetry.context_switches_s.as_slice()),
    ));

    svg.push_str("</svg>");
    svg
}

fn render_series_panel(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    title: &str,
    unit: &str,
    harrow: &[f64],
    axum: &[f64],
) -> String {
    let mut out = String::new();
    out.push_str(&panel_card(x, y, w, h, ACCENT_ORANGE));
    out.push_str(&mono_text(x + 22.0, y + 32.0, 18, 700, ACCENT_ORANGE, title));

    let plot_x = x + 20.0;
    let plot_y = y + 56.0;
    let plot_w = w - 40.0;
    let plot_h = h - 108.0;
    let max_y = series_max(harrow)
        .max(series_max(axum))
        .max(1.0);

    for idx in 0..=3 {
        let frac = idx as f64 / 3.0;
        let gy = plot_y + plot_h - frac * plot_h;
        out.push_str(&format!(
            r##"<line x1="{plot_x:.1}" y1="{gy:.1}" x2="{:.1}" y2="{gy:.1}" stroke="{BORDER_LIGHT}" stroke-width="1"/>"##,
            plot_x + plot_w
        ));
        out.push_str(&ui_text(
            plot_x + plot_w - 2.0,
            gy - 4.0,
            10,
            500,
            TEXT_MUTED,
            &format!("{:.1} {}", max_y * frac, unit),
        ));
    }

    out.push_str(&polyline(plot_x, plot_y, plot_w, plot_h, max_y, harrow, ACCENT_ORANGE));
    out.push_str(&polyline(plot_x, plot_y, plot_w, plot_h, max_y, axum, AXUM_GRAY));

    out.push_str(&mono_text(
        plot_x,
        y + h - 34.0,
        12,
        600,
        ACCENT_ORANGE,
        &format!(
            "harrow avg {:.1} {} peak {:.1}",
            series_avg(harrow),
            unit,
            series_max(harrow)
        ),
    ));
    out.push_str(&mono_text(
        plot_x,
        y + h - 18.0,
        12,
        600,
        AXUM_GRAY,
        &format!(
            "axum avg {:.1} {} peak {:.1}",
            series_avg(axum),
            unit,
            series_max(axum)
        ),
    ));
    out
}

fn legend_chip(x: f64, y: f64, color: &str, fill: &str, label: &str) -> String {
    format!(
        r##"<g><rect x="{x:.1}" y="{y:.1}" width="92" height="28" rx="4" fill="{fill}" stroke="{BORDER_LIGHT}" stroke-width="1"/><rect x="{:.1}" y="{:.1}" width="12" height="12" rx="4" fill="{fill}" stroke="{color}" stroke-width="1.5"/><text x="{:.1}" y="{:.1}" font-family="monospace" font-size="12" font-weight="700" fill="{color}">{}</text></g>"##,
        x + 12.0,
        y + 8.0,
        x + 30.0,
        y + 18.0,
        xml_escape(label)
    )
}

fn polyline(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    max_y: f64,
    series: &[f64],
    color: &str,
) -> String {
    if series.is_empty() {
        return String::new();
    }

    if series.len() == 1 {
        let cy = y + h - (series[0] / max_y) * h;
        return format!(
            r##"<circle cx="{:.1}" cy="{cy:.1}" r="4" fill="{color}"/>"##,
            x + w / 2.0
        );
    }

    let mut points = String::new();
    for (idx, value) in series.iter().enumerate() {
        let frac = idx as f64 / (series.len() - 1) as f64;
        let px = x + frac * w;
        let py = y + h - (value / max_y) * h;
        if !points.is_empty() {
            points.push(' ');
        }
        write!(&mut points, "{px:.1},{py:.1}").unwrap();
    }

    format!(
        r##"<polyline fill="none" stroke="{color}" stroke-width="2.5" points="{points}" stroke-linejoin="round" stroke-linecap="round"/>"##
    )
}

fn series_avg(series: &[f64]) -> f64 {
    if series.is_empty() {
        0.0
    } else {
        series.iter().sum::<f64>() / series.len() as f64
    }
}

fn series_max(series: &[f64]) -> f64 {
    series.iter().copied().fold(0.0, f64::max)
}

fn local_inferno_available() -> bool {
    command_exists("inferno-collapse-perf") && command_exists("inferno-flamegraph")
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn generate_local_flamegraph(script_path: &Path, folded_path: &Path, svg_path: &Path) -> io::Result<()> {
    let script_file = fs::File::open(script_path)?;
    let folded_file = fs::File::create(folded_path)?;
    let status = Command::new("inferno-collapse-perf")
        .stdin(Stdio::from(script_file))
        .stdout(Stdio::from(folded_file))
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "inferno-collapse-perf failed for {}",
            script_path.display()
        )));
    }

    let folded_file = fs::File::open(folded_path)?;
    let svg_file = fs::File::create(svg_path)?;
    let status = Command::new("inferno-flamegraph")
        .stdin(Stdio::from(folded_file))
        .stdout(Stdio::from(svg_file))
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "inferno-flamegraph failed for {}",
            folded_path.display()
        )));
    }

    Ok(())
}
