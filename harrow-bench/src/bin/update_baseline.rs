//! Read criterion JSON results and update `benches/baseline.toml`.
//!
//! Usage:
//!   cargo run --bin update-baseline
//!
//! Reads `target/criterion/{criterion_path}/new/estimates.json` for each
//! benchmark entry in `baseline.toml` and fills in `mean_ns` / `median_ns`.
//! Also computes the resource budget derived fields.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TOML data model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    metadata: Metadata,
    benchmarks: BTreeMap<String, BenchEntry>,
    axum_benchmarks: BTreeMap<String, BenchEntry>,
    traffic_weights: BTreeMap<String, f64>,
    resource_budget: ResourceBudget,
}

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    version: String,
    date: String,
    platform: String,
    cpu: String,
    rust_version: String,
    notes: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchEntry {
    criterion_path: String,
    description: String,
    mean_ns: f64,
    median_ns: f64,
    alloc_bytes: u64,
    alloc_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResourceBudget {
    target_ops_per_sec: u64,
    cpu_budget_percent: f64,
    memory_budget_mb: f64,
    weighted_mean_ns: f64,
    total_cpu_percent: f64,
    verdict: String,
}

// ---------------------------------------------------------------------------
// Criterion JSON model (only the fields we need)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CriterionEstimates {
    mean: Estimate,
    median: Estimate,
}

#[derive(Debug, Deserialize)]
struct Estimate {
    point_estimate: f64,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let toml_path = Path::new(manifest_dir).join("benches/baseline.toml");
    let criterion_base = Path::new(manifest_dir).join("../target/criterion");

    let toml_text = fs::read_to_string(&toml_path).unwrap_or_else(|e| {
        eprintln!("error: cannot read {}: {e}", toml_path.display());
        std::process::exit(1);
    });

    let mut baseline: Baseline = toml::from_str(&toml_text).unwrap_or_else(|e| {
        eprintln!("error: cannot parse TOML: {e}");
        std::process::exit(1);
    });

    // Update date to today.
    baseline.metadata.date = today();

    let mut updated = 0u32;
    let mut missing = 0u32;

    for (name, entry) in &mut baseline.benchmarks {
        let estimates_path = criterion_base
            .join(&entry.criterion_path)
            .join("new/estimates.json");

        if !estimates_path.exists() {
            eprintln!(
                "  skip {name}: no criterion data at {}",
                estimates_path.display()
            );
            missing += 1;
            continue;
        }

        let json_text = fs::read_to_string(&estimates_path).unwrap_or_else(|e| {
            eprintln!("  error reading {}: {e}", estimates_path.display());
            std::process::exit(1);
        });

        let estimates: CriterionEstimates = serde_json::from_str(&json_text).unwrap_or_else(|e| {
            eprintln!("  error parsing JSON for {name}: {e}");
            std::process::exit(1);
        });

        entry.mean_ns = estimates.mean.point_estimate;
        entry.median_ns = estimates.median.point_estimate;
        updated += 1;
        println!(
            "  {name}: mean={:.1} ns, median={:.1} ns",
            entry.mean_ns, entry.median_ns
        );
    }

    // Update Axum benchmarks from criterion data.
    println!("\nAxum benchmarks:");
    for (name, entry) in &mut baseline.axum_benchmarks {
        let estimates_path = criterion_base
            .join(&entry.criterion_path)
            .join("new/estimates.json");

        if !estimates_path.exists() {
            eprintln!(
                "  skip {name}: no criterion data at {}",
                estimates_path.display()
            );
            missing += 1;
            continue;
        }

        let json_text = fs::read_to_string(&estimates_path).unwrap_or_else(|e| {
            eprintln!("  error reading {}: {e}", estimates_path.display());
            std::process::exit(1);
        });

        let estimates: CriterionEstimates = serde_json::from_str(&json_text).unwrap_or_else(|e| {
            eprintln!("  error parsing JSON for {name}: {e}");
            std::process::exit(1);
        });

        entry.mean_ns = estimates.mean.point_estimate;
        entry.median_ns = estimates.median.point_estimate;
        updated += 1;
        println!(
            "  {name}: mean={:.1} ns, median={:.1} ns",
            entry.mean_ns, entry.median_ns
        );
    }

    // Compute resource budget derived fields.
    let mut weighted_sum = 0.0;
    let mut weight_sum = 0.0;
    for (key, &weight) in &baseline.traffic_weights {
        if let Some(entry) = baseline.benchmarks.get(key) {
            weighted_sum += entry.mean_ns * weight;
            weight_sum += weight;
        } else {
            eprintln!("  warning: traffic weight key '{key}' not found in benchmarks");
        }
    }

    if weight_sum > 0.0 {
        baseline.resource_budget.weighted_mean_ns = weighted_sum / weight_sum;
    }

    // CPU% = weighted_mean_ns * target_ops / 1e9 * 100
    let target_ops = baseline.resource_budget.target_ops_per_sec as f64;
    baseline.resource_budget.total_cpu_percent =
        baseline.resource_budget.weighted_mean_ns * target_ops / 1e9 * 100.0;

    baseline.resource_budget.verdict = if baseline.resource_budget.total_cpu_percent
        < baseline.resource_budget.cpu_budget_percent
    {
        "PASS".to_string()
    } else if baseline.resource_budget.total_cpu_percent == 0.0 {
        "PENDING".to_string()
    } else {
        "FAIL".to_string()
    };

    // Write back.
    let output = toml::to_string_pretty(&baseline).unwrap_or_else(|e| {
        eprintln!("error: cannot serialize TOML: {e}");
        std::process::exit(1);
    });

    fs::write(&toml_path, &output).unwrap_or_else(|e| {
        eprintln!("error: cannot write {}: {e}", toml_path.display());
        std::process::exit(1);
    });

    println!();
    println!("Updated {updated} benchmarks ({missing} missing criterion data)");
    println!(
        "Weighted mean: {:.1} ns",
        baseline.resource_budget.weighted_mean_ns
    );
    println!(
        "CPU at {} ops/s: {:.2}% (budget: {}%)",
        baseline.resource_budget.target_ops_per_sec,
        baseline.resource_budget.total_cpu_percent,
        baseline.resource_budget.cpu_budget_percent,
    );
    println!("Verdict: {}", baseline.resource_budget.verdict);
    println!("Written: {}", toml_path.display());
}

fn today() -> String {
    // Use chrono-free approach: parse from system command or fallback.
    let output = std::process::Command::new("date")
        .args(["+%Y-%m-%d"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}
