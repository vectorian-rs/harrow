//! Render `benches/baseline.toml` as a self-contained SVG visualization.
//!
//! Usage:
//!   cargo run --bin render-baseline
//!
//! Produces `docs/performance.svg` with panels:
//! 1. Harrow per-operation latency (horizontal bar chart)
//! 2. Harrow vs Axum latency comparison (paired bars for TCP benchmarks)
//! 3. Harrow vs Axum allocation comparison (paired bars)
//! 4. Resource budget summary

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// TOML data model (read-only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Baseline {
    metadata: Metadata,
    benchmarks: BTreeMap<String, BenchEntry>,
    axum_benchmarks: BTreeMap<String, BenchEntry>,
    traffic_weights: BTreeMap<String, f64>,
    resource_budget: ResourceBudget,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Metadata {
    version: String,
    date: String,
    platform: String,
    cpu: String,
    rust_version: String,
    notes: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BenchEntry {
    criterion_path: String,
    description: String,
    mean_ns: f64,
    median_ns: f64,
    alloc_bytes: u64,
    alloc_count: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResourceBudget {
    target_ops_per_sec: u64,
    cpu_budget_percent: f64,
    memory_budget_mb: f64,
    weighted_mean_ns: f64,
    total_cpu_percent: f64,
    verdict: String,
}

// ---------------------------------------------------------------------------
// Colors & constants
// ---------------------------------------------------------------------------

const BG: &str = "#FFFFFF";
const TEXT: &str = "#1F2937";
const MUTED: &str = "#6B7280";
const GRID: &str = "#E5E7EB";
const HARROW_COLOR: &str = "#3B82F6"; // blue
const AXUM_COLOR: &str = "#F97316"; // orange
const MICRO_COLOR: &str = "#6366F1"; // indigo for micro-only benchmarks
const ALLOC_HARROW: &str = "#8B5CF6"; // violet
const ALLOC_AXUM: &str = "#F59E0B"; // amber
const PASS_COLOR: &str = "#10B981"; // green
const FAIL_COLOR: &str = "#EF4444"; // red
const PENDING_COLOR: &str = "#F59E0B"; // amber
const BUDGET_BG: &str = "#F9FAFB"; // light gray

const SVG_WIDTH: f64 = 900.0;
const BAR_HEIGHT: f64 = 20.0;
const BAR_GAP: f64 = 6.0;
const PAIR_GAP: f64 = 16.0; // gap between paired benchmark groups
const LABEL_WIDTH: f64 = 200.0;
const CHART_LEFT: f64 = 220.0;
const CHART_WIDTH: f64 = 580.0;
const SECTION_GAP: f64 = 40.0;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn fmt_time(ns: f64) -> String {
    if ns == 0.0 {
        "—".to_string()
    } else if ns < 1_000.0 {
        format!("{ns:.1} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} \u{00B5}s", ns / 1_000.0)
    } else {
        format!("{:.2} ms", ns / 1_000_000.0)
    }
}

fn fmt_bytes(b: u64) -> String {
    if b == 0 {
        "0 B".to_string()
    } else if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    }
}

fn is_tcp(entry: &BenchEntry) -> bool {
    let p = &entry.criterion_path;
    p.starts_with("echo_tcp")
        || p.starts_with("full_stack")
        || p.starts_with("middleware_depth")
        || p.starts_with("axum_echo_tcp")
}

/// Friendly display name for a benchmark key.
fn display_name(key: &str) -> &str {
    match key {
        "echo_text" => "text echo",
        "echo_json" => "json echo",
        "echo_param" => "param echo",
        "echo_404" => "404 miss",
        "full_json_3mw" => "json + 3mw + state",
        "full_health_3mw" => "health + 3mw",
        "mw_depth_10" => "10 middleware",
        "path_match_exact_hit" => "exact match",
        "path_match_1_param" => "1 param match",
        "path_match_glob" => "glob match",
        "route_lookup_100" => "route lookup (100)",
        _ => key,
    }
}

// ---------------------------------------------------------------------------
// SVG rendering
// ---------------------------------------------------------------------------

fn render(baseline: &Baseline) -> String {
    let mut svg = String::with_capacity(32768);

    // Separate micro and TCP harrow benchmarks.
    let mut micro_entries: Vec<(&str, &BenchEntry)> = Vec::new();
    let mut tcp_entries: Vec<(&str, &BenchEntry)> = Vec::new();
    for (k, v) in &baseline.benchmarks {
        if is_tcp(v) {
            tcp_entries.push((k.as_str(), v));
        } else {
            micro_entries.push((k.as_str(), v));
        }
    }
    micro_entries.sort_by(|a, b| b.1.mean_ns.partial_cmp(&a.1.mean_ns).unwrap());
    tcp_entries.sort_by(|a, b| b.1.mean_ns.partial_cmp(&a.1.mean_ns).unwrap());

    // Comparison keys: TCP benchmarks that exist in both harrow and axum.
    let comparison_keys: Vec<&str> = tcp_entries
        .iter()
        .filter(|(k, _)| baseline.axum_benchmarks.contains_key(*k))
        .map(|(k, _)| *k)
        .collect();

    let has_axum =
        !comparison_keys.is_empty() && baseline.axum_benchmarks.values().any(|e| e.mean_ns > 0.0);
    let has_alloc_data = baseline.benchmarks.values().any(|e| e.alloc_bytes > 0);
    let has_axum_alloc = baseline.axum_benchmarks.values().any(|e| e.alloc_bytes > 0);

    // Compute total height.
    let harrow_panel_h =
        50.0 + (micro_entries.len() + tcp_entries.len()) as f64 * (BAR_HEIGHT + BAR_GAP);
    let comparison_panel_h = if has_axum {
        50.0 + comparison_keys.len() as f64 * (2.0 * BAR_HEIGHT + PAIR_GAP)
    } else {
        0.0
    };
    let alloc_panel_h = if has_alloc_data || has_axum_alloc {
        let alloc_count = if has_axum_alloc {
            comparison_keys.len()
        } else {
            baseline
                .benchmarks
                .values()
                .filter(|e| e.alloc_bytes > 0)
                .count()
        };
        50.0 + alloc_count as f64 * (2.0 * BAR_HEIGHT + PAIR_GAP)
    } else {
        0.0
    };
    let budget_panel_h = 140.0;

    let total_h = 60.0
        + harrow_panel_h
        + if has_axum {
            SECTION_GAP + comparison_panel_h
        } else {
            0.0
        }
        + if has_alloc_data || has_axum_alloc {
            SECTION_GAP + alloc_panel_h
        } else {
            0.0
        }
        + SECTION_GAP
        + budget_panel_h
        + 30.0;

    // SVG header
    write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SVG_WIDTH} {total_h}" width="{SVG_WIDTH}" height="{total_h}" font-family="system-ui,-apple-system,sans-serif">
<rect width="{SVG_WIDTH}" height="{total_h}" fill="{BG}"/>
"#
    )
    .unwrap();

    // Title + metadata
    write!(
        svg,
        r#"<text x="30" y="30" font-size="18" font-weight="600" fill="{TEXT}">Harrow Performance Baseline</text>
<text x="30" y="48" font-size="11" fill="{MUTED}">{} | {} | {} | Rust {} | {}</text>
"#,
        escape_xml(&baseline.metadata.date),
        escape_xml(&baseline.metadata.platform),
        escape_xml(&baseline.metadata.cpu),
        escape_xml(&baseline.metadata.rust_version),
        escape_xml(&baseline.metadata.version),
    )
    .unwrap();

    let mut y = 60.0;

    // ── Panel 1: All Harrow Benchmarks ──────────────────────────────────
    write!(
        svg,
        r#"<text x="30" y="{:.0}" font-size="14" font-weight="600" fill="{TEXT}">Harrow Latency (all benchmarks)</text>
"#,
        y + 16.0
    )
    .unwrap();

    // Legend
    let ly = y + 14.0;
    write!(
        svg,
        r#"<rect x="650" y="{ly:.0}" width="12" height="12" rx="2" fill="{MICRO_COLOR}"/>
<text x="666" y="{:.0}" font-size="11" fill="{MUTED}">Micro</text>
<rect x="720" y="{ly:.0}" width="12" height="12" rx="2" fill="{HARROW_COLOR}"/>
<text x="736" y="{:.0}" font-size="11" fill="{MUTED}">TCP</text>
"#,
        ly + 10.0,
        ly + 10.0,
    )
    .unwrap();
    y += 32.0;

    let all_entries: Vec<(&str, &BenchEntry)> = micro_entries
        .iter()
        .chain(tcp_entries.iter())
        .copied()
        .collect();
    let max_ns = all_entries
        .iter()
        .map(|(_, e)| e.mean_ns)
        .fold(0.0_f64, f64::max);
    let scale = if max_ns > 0.0 {
        CHART_WIDTH / max_ns
    } else {
        1.0
    };

    for (name, entry) in &all_entries {
        let bar_w = entry.mean_ns * scale;
        let color = if is_tcp(entry) {
            HARROW_COLOR
        } else {
            MICRO_COLOR
        };

        write!(
            svg,
            r#"<text x="{LABEL_WIDTH}" y="{:.1}" text-anchor="end" font-size="11" fill="{TEXT}">{}</text>
<rect x="{CHART_LEFT}" y="{y:.1}" width="{bar_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{color}" opacity="0.85"><title>{}: {}</title></rect>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{}</text>
"#,
            y + BAR_HEIGHT * 0.72,
            escape_xml(display_name(name)),
            escape_xml(&entry.description),
            fmt_time(entry.mean_ns),
            CHART_LEFT + bar_w + 6.0,
            y + BAR_HEIGHT * 0.72,
            escape_xml(&fmt_time(entry.mean_ns)),
        )
        .unwrap();
        y += BAR_HEIGHT + BAR_GAP;
    }

    // ── Panel 2: Harrow vs Axum Latency ─────────────────────────────────
    if has_axum {
        y += SECTION_GAP;
        write!(
            svg,
            r#"<text x="30" y="{:.0}" font-size="14" font-weight="600" fill="{TEXT}">Harrow vs Axum — TCP Latency</text>
"#,
            y + 16.0
        )
        .unwrap();

        let ly = y + 14.0;
        write!(
            svg,
            r#"<rect x="650" y="{ly:.0}" width="12" height="12" rx="2" fill="{HARROW_COLOR}"/>
<text x="666" y="{:.0}" font-size="11" fill="{MUTED}">Harrow</text>
<rect x="730" y="{ly:.0}" width="12" height="12" rx="2" fill="{AXUM_COLOR}"/>
<text x="746" y="{:.0}" font-size="11" fill="{MUTED}">Axum</text>
"#,
            ly + 10.0,
            ly + 10.0,
        )
        .unwrap();
        y += 32.0;

        // Find max across both for consistent scale.
        let cmp_max = comparison_keys
            .iter()
            .flat_map(|k| {
                let h = baseline
                    .benchmarks
                    .get(*k)
                    .map(|e| e.mean_ns)
                    .unwrap_or(0.0);
                let a = baseline
                    .axum_benchmarks
                    .get(*k)
                    .map(|e| e.mean_ns)
                    .unwrap_or(0.0);
                [h, a]
            })
            .fold(0.0_f64, f64::max);
        let cmp_scale = if cmp_max > 0.0 {
            CHART_WIDTH / cmp_max
        } else {
            1.0
        };

        for key in &comparison_keys {
            let h_entry = &baseline.benchmarks[*key];
            let a_entry = &baseline.axum_benchmarks[*key];
            let h_w = h_entry.mean_ns * cmp_scale;
            let a_w = a_entry.mean_ns * cmp_scale;

            // Label
            write!(
                svg,
                r#"<text x="{LABEL_WIDTH}" y="{:.1}" text-anchor="end" font-size="11" fill="{TEXT}">{}</text>
"#,
                y + BAR_HEIGHT * 0.72,
                escape_xml(display_name(key)),
            )
            .unwrap();

            // Harrow bar
            write!(
                svg,
                r#"<rect x="{CHART_LEFT}" y="{y:.1}" width="{h_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{HARROW_COLOR}" opacity="0.85"/>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{}</text>
"#,
                CHART_LEFT + h_w + 6.0,
                y + BAR_HEIGHT * 0.72,
                escape_xml(&fmt_time(h_entry.mean_ns)),
            )
            .unwrap();
            y += BAR_HEIGHT;

            // Axum bar
            write!(
                svg,
                r#"<rect x="{CHART_LEFT}" y="{y:.1}" width="{a_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{AXUM_COLOR}" opacity="0.85"/>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{}</text>
"#,
                CHART_LEFT + a_w + 6.0,
                y + BAR_HEIGHT * 0.72,
                escape_xml(&fmt_time(a_entry.mean_ns)),
            )
            .unwrap();
            y += PAIR_GAP;
        }
    }

    // ── Panel 3: Allocation Comparison ──────────────────────────────────
    if has_alloc_data || has_axum_alloc {
        y += SECTION_GAP;
        write!(
            svg,
            r#"<text x="30" y="{:.0}" font-size="14" font-weight="600" fill="{TEXT}">Allocation Profile — bytes per operation</text>
"#,
            y + 16.0
        )
        .unwrap();

        if has_axum_alloc {
            let ly = y + 14.0;
            write!(
                svg,
                r#"<rect x="620" y="{ly:.0}" width="12" height="12" rx="2" fill="{ALLOC_HARROW}"/>
<text x="636" y="{:.0}" font-size="11" fill="{MUTED}">Harrow</text>
<rect x="700" y="{ly:.0}" width="12" height="12" rx="2" fill="{ALLOC_AXUM}"/>
<text x="716" y="{:.0}" font-size="11" fill="{MUTED}">Axum</text>
"#,
                ly + 10.0,
                ly + 10.0,
            )
            .unwrap();
        }
        y += 32.0;

        if has_axum_alloc {
            // Paired comparison for TCP benchmarks
            let alloc_max = comparison_keys
                .iter()
                .flat_map(|k| {
                    let h = baseline
                        .benchmarks
                        .get(*k)
                        .map(|e| e.alloc_bytes)
                        .unwrap_or(0);
                    let a = baseline
                        .axum_benchmarks
                        .get(*k)
                        .map(|e| e.alloc_bytes)
                        .unwrap_or(0);
                    [h, a]
                })
                .max()
                .unwrap_or(1) as f64;
            let alloc_scale = CHART_WIDTH / alloc_max;

            for key in &comparison_keys {
                let h_entry = &baseline.benchmarks[*key];
                let a_entry = &baseline.axum_benchmarks[*key];
                let h_w = h_entry.alloc_bytes as f64 * alloc_scale;
                let a_w = a_entry.alloc_bytes as f64 * alloc_scale;

                write!(
                    svg,
                    r#"<text x="{LABEL_WIDTH}" y="{:.1}" text-anchor="end" font-size="11" fill="{TEXT}">{}</text>
"#,
                    y + BAR_HEIGHT * 0.72,
                    escape_xml(display_name(key)),
                )
                .unwrap();

                // Harrow alloc bar
                write!(
                    svg,
                    r#"<rect x="{CHART_LEFT}" y="{y:.1}" width="{h_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{ALLOC_HARROW}" opacity="0.8"/>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{} ({} allocs)</text>
"#,
                    CHART_LEFT + h_w + 6.0,
                    y + BAR_HEIGHT * 0.72,
                    escape_xml(&fmt_bytes(h_entry.alloc_bytes)),
                    h_entry.alloc_count,
                )
                .unwrap();
                y += BAR_HEIGHT;

                // Axum alloc bar
                write!(
                    svg,
                    r#"<rect x="{CHART_LEFT}" y="{y:.1}" width="{a_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{ALLOC_AXUM}" opacity="0.8"/>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{} ({} allocs)</text>
"#,
                    CHART_LEFT + a_w + 6.0,
                    y + BAR_HEIGHT * 0.72,
                    escape_xml(&fmt_bytes(a_entry.alloc_bytes)),
                    a_entry.alloc_count,
                )
                .unwrap();
                y += PAIR_GAP;
            }
        } else {
            // Harrow-only alloc bars
            let alloc_entries: Vec<(&str, &BenchEntry)> = all_entries
                .iter()
                .filter(|(_, e)| e.alloc_bytes > 0)
                .copied()
                .collect();
            let max_alloc = alloc_entries
                .iter()
                .map(|(_, e)| e.alloc_bytes)
                .max()
                .unwrap_or(1) as f64;
            let alloc_scale = CHART_WIDTH / max_alloc;

            for (name, entry) in &alloc_entries {
                let bar_w = entry.alloc_bytes as f64 * alloc_scale;
                write!(
                    svg,
                    r#"<text x="{LABEL_WIDTH}" y="{:.1}" text-anchor="end" font-size="11" fill="{TEXT}">{}</text>
<rect x="{CHART_LEFT}" y="{y:.1}" width="{bar_w:.1}" height="{BAR_HEIGHT}" rx="3" fill="{ALLOC_HARROW}" opacity="0.8"/>
<text x="{:.1}" y="{:.1}" font-size="10" fill="{MUTED}">{} ({} allocs)</text>
"#,
                    y + BAR_HEIGHT * 0.72,
                    escape_xml(display_name(name)),
                    CHART_LEFT + bar_w + 6.0,
                    y + BAR_HEIGHT * 0.72,
                    escape_xml(&fmt_bytes(entry.alloc_bytes)),
                    entry.alloc_count,
                )
                .unwrap();
                y += BAR_HEIGHT + BAR_GAP;
            }
        }
    }

    // ── Panel 4: Resource Budget ────────────────────────────────────────
    y += SECTION_GAP;
    let budget = &baseline.resource_budget;
    let verdict_color = match budget.verdict.as_str() {
        "PASS" => PASS_COLOR,
        "FAIL" => FAIL_COLOR,
        _ => PENDING_COLOR,
    };

    write!(
        svg,
        r#"<text x="30" y="{:.0}" font-size="14" font-weight="600" fill="{TEXT}">Resource Budget</text>
"#,
        y + 16.0
    )
    .unwrap();
    y += 30.0;

    write!(
        svg,
        r#"<rect x="30" y="{y:.0}" width="840" height="100" rx="8" fill="{BUDGET_BG}" stroke="{GRID}" stroke-width="1"/>
"#
    )
    .unwrap();

    let box_y = y;
    let col1_x = 50.0;
    let col2_x = 280.0;
    let col3_x = 530.0;

    let r1_y = box_y + 28.0;
    write!(
        svg,
        r#"<text x="{col1_x}" y="{r1_y:.0}" font-size="11" fill="{MUTED}">Weighted Mean Latency</text>
<text x="{col1_x}" y="{:.0}" font-size="13" font-weight="500" fill="{TEXT}">{}</text>
<text x="{col2_x}" y="{r1_y:.0}" font-size="11" fill="{MUTED}">Max Single-Core Throughput</text>
<text x="{col2_x}" y="{:.0}" font-size="13" font-weight="500" fill="{TEXT}">{}</text>
<text x="{col3_x}" y="{r1_y:.0}" font-size="11" fill="{MUTED}">Verdict</text>
<text x="{col3_x}" y="{:.0}" font-size="16" font-weight="700" fill="{verdict_color}">{}</text>
"#,
        r1_y + 18.0,
        escape_xml(&fmt_time(budget.weighted_mean_ns)),
        r1_y + 18.0,
        if budget.weighted_mean_ns > 0.0 {
            format!("{:.0} req/s", 1e9 / budget.weighted_mean_ns)
        } else {
            "—".to_string()
        },
        r1_y + 20.0,
        escape_xml(&budget.verdict),
    )
    .unwrap();

    let r2_y = box_y + 70.0;
    write!(
        svg,
        r#"<text x="{col1_x}" y="{r2_y:.0}" font-size="11" fill="{MUTED}">Target Ops/s</text>
<text x="{col1_x}" y="{:.0}" font-size="13" font-weight="500" fill="{TEXT}">{}</text>
<text x="{col2_x}" y="{r2_y:.0}" font-size="11" fill="{MUTED}">CPU at Target</text>
<text x="{col2_x}" y="{:.0}" font-size="13" font-weight="500" fill="{TEXT}">{:.2}%</text>
<text x="{col3_x}" y="{r2_y:.0}" font-size="11" fill="{MUTED}">Memory Budget</text>
<text x="{col3_x}" y="{:.0}" font-size="13" font-weight="500" fill="{TEXT}">{:.0} MB</text>
"#,
        r2_y + 18.0,
        budget.target_ops_per_sec,
        r2_y + 18.0,
        budget.total_cpu_percent,
        r2_y + 18.0,
        budget.memory_budget_mb,
    )
    .unwrap();

    svg.push_str("</svg>\n");
    svg
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let toml_path = Path::new(manifest_dir).join("benches/baseline.toml");
    let svg_path = Path::new(manifest_dir).join("../docs/performance.svg");

    let toml_text = fs::read_to_string(&toml_path).unwrap_or_else(|e| {
        eprintln!("error: cannot read {}: {e}", toml_path.display());
        std::process::exit(1);
    });

    let baseline: Baseline = toml::from_str(&toml_text).unwrap_or_else(|e| {
        eprintln!("error: cannot parse TOML: {e}");
        std::process::exit(1);
    });

    let svg = render(&baseline);

    fs::write(&svg_path, &svg).unwrap_or_else(|e| {
        eprintln!("error: cannot write {}: {e}", svg_path.display());
        std::process::exit(1);
    });

    println!("Written: {}", svg_path.display());
    println!(
        "  {} harrow benchmarks, {} axum benchmarks",
        baseline.benchmarks.len(),
        baseline.axum_benchmarks.len(),
    );
}
