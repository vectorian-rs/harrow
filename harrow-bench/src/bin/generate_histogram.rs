//! Generate SVG comparison charts from Harrow vs Axum benchmark JSON.
//!
//! Usage:
//!   generate-histogram target/comparison/
//!
//! Produces:
//!   target/comparison/throughput.svg
//!   target/comparison/latency-p50.svg
//!   target/comparison/latency-p99.svg

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Colors & layout
// ---------------------------------------------------------------------------

const HARROW_COLOR: &str = "#3B82F6";
const AXUM_COLOR: &str = "#F97316";
const BG_COLOR: &str = "#FFFFFF";
const GRID_COLOR: &str = "#E5E7EB";
const TEXT_COLOR: &str = "#374151";
const LABEL_COLOR: &str = "#6B7280";

const WIDTH: f64 = 1200.0;
const HEIGHT: f64 = 600.0;
const MARGIN_TOP: f64 = 80.0;
const MARGIN_RIGHT: f64 = 40.0;
const MARGIN_BOTTOM: f64 = 100.0;
const MARGIN_LEFT: f64 = 90.0;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

struct Scenario {
    label: String,
    harrow: f64,
    axum: f64,
}

fn load_results(dir: &Path) -> BTreeMap<String, BTreeMap<String, Value>> {
    let mut results: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return results,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let stem = path.file_stem().unwrap().to_string_lossy().to_string();

        // Parse: harrow_root_c1 -> (harrow, root, 1)
        let parts: Vec<&str> = stem.splitn(2, '_').collect();
        if parts.len() != 2 {
            continue;
        }
        let framework = parts[0];
        if framework != "harrow" && framework != "axum" {
            continue;
        }

        // The rest is like "root_c1" or "greet_bench_c32"
        let rest = parts[1];
        let c_pos = rest.rfind("_c").unwrap_or(rest.len());
        let scenario_key = &rest[..c_pos];
        let conc_str = &rest[c_pos..];
        let label = format!("{scenario_key}\n{conc_str}");

        let data = match fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v) => v,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        results
            .entry(label)
            .or_default()
            .insert(framework.to_string(), data);
    }

    results
}

fn extract_scenarios(
    results: &BTreeMap<String, BTreeMap<String, Value>>,
    metric: &str,
) -> Vec<Scenario> {
    results
        .iter()
        .map(|(label, frameworks)| {
            let h = frameworks
                .get("harrow")
                .and_then(|v| v.get(metric))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let a = frameworks
                .get("axum")
                .and_then(|v| v.get(metric))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            Scenario {
                label: label.clone(),
                harrow: h,
                axum: a,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SVG generation
// ---------------------------------------------------------------------------

fn nice_ticks(max_val: f64, n: usize) -> Vec<f64> {
    if max_val <= 0.0 {
        return vec![0.0];
    }
    let raw = max_val / n as f64;
    let mag = 10.0_f64.powf(raw.log10().floor());
    let mut step = mag;
    for &c in &[1.0, 2.0, 2.5, 5.0, 10.0] {
        if c * mag >= raw {
            step = c * mag;
            break;
        }
    }
    let mut ticks = Vec::new();
    let mut v = 0.0;
    while v <= max_val * 1.05 {
        ticks.push(v);
        v += step;
    }
    ticks
}

fn fmt_number(val: f64) -> String {
    if val >= 1_000_000.0 {
        format!("{:.1}M", val / 1_000_000.0)
    } else if val >= 1_000.0 {
        format!("{:.0}K", val / 1_000.0)
    } else if val == val.floor() && val.abs() < 1e12 {
        format!("{}", val as i64)
    } else if val < 1.0 {
        format!("{val:.3}")
    } else {
        format!("{val:.1}")
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn bar_chart_svg(scenarios: &[Scenario], title: &str, y_label: &str) -> String {
    if scenarios.is_empty() {
        return String::new();
    }

    let chart_w = WIDTH - MARGIN_LEFT - MARGIN_RIGHT;
    let chart_h = HEIGHT - MARGIN_TOP - MARGIN_BOTTOM;

    let y_max_raw = scenarios
        .iter()
        .flat_map(|s| [s.harrow, s.axum])
        .fold(0.0_f64, f64::max);

    let ticks = nice_ticks(y_max_raw, 5);
    let y_max = ticks.last().copied().filter(|&v| v > 0.0).unwrap_or(1.0);

    let n = scenarios.len() as f64;
    let gw = chart_w / n;
    let bw = gw * 0.35;
    let gap = gw * 0.05;

    let mut svg = String::with_capacity(8192);

    // Header
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}" font-family="system-ui,-apple-system,sans-serif">
<rect width="{WIDTH}" height="{HEIGHT}" fill="{BG_COLOR}"/>
"#
    )
    .unwrap();

    // Title
    writeln!(
        svg,
        r#"<text x="{}" y="35" text-anchor="middle" font-size="20" font-weight="600" fill="{TEXT_COLOR}">{}</text>"#,
        WIDTH / 2.0,
        escape_xml(title)
    )
    .unwrap();

    // Legend
    let lx = WIDTH / 2.0 - 100.0;
    let ly = 58.0;
    write!(
        svg,
        r#"<rect x="{lx}" y="{}" width="14" height="14" rx="2" fill="{HARROW_COLOR}"/>
<text x="{}" y="{ly}" font-size="13" fill="{LABEL_COLOR}">Harrow</text>
<rect x="{}" y="{}" width="14" height="14" rx="2" fill="{AXUM_COLOR}"/>
<text x="{}" y="{ly}" font-size="13" fill="{LABEL_COLOR}">Axum</text>
"#,
        ly - 10.0,
        lx + 20.0,
        lx + 100.0,
        ly - 10.0,
        lx + 120.0,
    )
    .unwrap();

    // Chart group
    write!(
        svg,
        r#"<g transform="translate({MARGIN_LEFT},{MARGIN_TOP})">"#
    )
    .unwrap();

    // Grid + Y labels
    for &t in &ticks {
        let y = chart_h - (t / y_max * chart_h);
        write!(
            svg,
            r#"
<line x1="0" y1="{y:.1}" x2="{chart_w}" y2="{y:.1}" stroke="{GRID_COLOR}" stroke-dasharray="4,4"/>
<text x="-10" y="{:.1}" text-anchor="end" font-size="11" fill="{LABEL_COLOR}">{}</text>"#,
            y + 4.0,
            fmt_number(t)
        )
        .unwrap();
    }

    // Y axis label
    write!(
        svg,
        r#"
<text x="-60" y="{:.1}" text-anchor="middle" font-size="13" fill="{TEXT_COLOR}" transform="rotate(-90,-60,{:.1})">{}</text>"#,
        chart_h / 2.0,
        chart_h / 2.0,
        escape_xml(y_label)
    )
    .unwrap();

    // X baseline
    write!(
        svg,
        r#"
<line x1="0" y1="{chart_h}" x2="{chart_w}" y2="{chart_h}" stroke="{GRID_COLOR}"/>"#
    )
    .unwrap();

    // Bars
    for (i, sc) in scenarios.iter().enumerate() {
        let x0 = i as f64 * gw + gap;

        // Harrow bar
        let hh = sc.harrow / y_max * chart_h;
        let hy = chart_h - hh;
        write!(
            svg,
            r#"
<rect x="{x0:.1}" y="{hy:.1}" width="{bw:.1}" height="{hh:.1}" rx="3" fill="{HARROW_COLOR}" opacity="0.9"><title>Harrow: {}</title></rect>"#,
            fmt_number(sc.harrow)
        )
        .unwrap();

        if hh > 20.0 {
            write!(
                svg,
                r#"
<text x="{:.1}" y="{:.1}" text-anchor="middle" font-size="10" font-weight="500" fill="white">{}</text>"#,
                x0 + bw / 2.0,
                hy + 15.0,
                fmt_number(sc.harrow)
            )
            .unwrap();
        }

        // Axum bar
        let ax = x0 + bw + gap;
        let ah = sc.axum / y_max * chart_h;
        let ay = chart_h - ah;
        write!(
            svg,
            r#"
<rect x="{ax:.1}" y="{ay:.1}" width="{bw:.1}" height="{ah:.1}" rx="3" fill="{AXUM_COLOR}" opacity="0.9"><title>Axum: {}</title></rect>"#,
            fmt_number(sc.axum)
        )
        .unwrap();

        if ah > 20.0 {
            write!(
                svg,
                r#"
<text x="{:.1}" y="{:.1}" text-anchor="middle" font-size="10" font-weight="500" fill="white">{}</text>"#,
                ax + bw / 2.0,
                ay + 15.0,
                fmt_number(sc.axum)
            )
            .unwrap();
        }

        // X labels
        let cx = x0 + bw + gap / 2.0;
        for (j, part) in sc.label.split('\n').enumerate() {
            write!(
                svg,
                r#"
<text x="{cx:.1}" y="{:.1}" text-anchor="middle" font-size="11" fill="{LABEL_COLOR}">{}</text>"#,
                chart_h + 18.0 + j as f64 * 15.0,
                escape_xml(part)
            )
            .unwrap();
        }
    }

    svg.push_str("\n</g>\n</svg>\n");
    svg
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: generate-histogram <data-directory>");
        std::process::exit(1);
    }

    let dir = PathBuf::from(&args[1]);
    if !dir.is_dir() {
        eprintln!("error: {} is not a directory", dir.display());
        std::process::exit(1);
    }

    let results = load_results(&dir);
    if results.is_empty() {
        eprintln!("No benchmark JSON files found in {}", dir.display());
        std::process::exit(1);
    }

    println!("Loaded {} scenarios from {}", results.len(), dir.display());

    let charts: &[(&str, &str, &str, &str)] = &[
        (
            "rps",
            "Harrow vs Axum — Throughput (requests/sec)",
            "Requests per second",
            "throughput.svg",
        ),
        (
            "latency_p50_ms",
            "Harrow vs Axum — p50 Latency (ms)",
            "Latency (ms)",
            "latency-p50.svg",
        ),
        (
            "latency_p99_ms",
            "Harrow vs Axum — p99 Tail Latency (ms)",
            "Latency (ms)",
            "latency-p99.svg",
        ),
    ];

    for &(metric, title, y_label, filename) in charts {
        let scenarios = extract_scenarios(&results, metric);
        let svg = bar_chart_svg(&scenarios, title, y_label);
        if svg.is_empty() {
            println!("  No data for {metric}, skipping {filename}");
            continue;
        }
        let out = dir.join(filename);
        fs::write(&out, &svg).unwrap();
        println!("  Written: {}", out.display());
    }
}
