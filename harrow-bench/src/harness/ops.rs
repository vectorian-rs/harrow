//! Shared shell, SSH, and metrics utilities for the benchmark harness.
//!
//! Used by both the harness runner (`runner.rs`) and the standalone
//! remote perf test binary (`harrow_remote_perf_test.rs`).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Shell execution
// ---------------------------------------------------------------------------

pub fn run_local(cmd: &str) -> std::io::Result<Output> {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

pub fn ssh_run(user: &str, host: &str, remote_cmd: &str) -> std::io::Result<Output> {
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

pub fn scp_to_remote(user: &str, host: &str, local_path: &Path, remote_path: &str) {
    let out = Command::new("scp")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg(local_path)
        .arg(format!("{user}@{host}:{remote_path}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match out {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "    warning: scp to {} failed: {}",
                remote_path,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(error) => eprintln!("    warning: scp to {} failed: {error}", remote_path),
    }
}

// ---------------------------------------------------------------------------
// Health checks
// ---------------------------------------------------------------------------

pub fn http_health_check(host: &str, port: u16, path: &str) -> bool {
    let addr = match format!("{host}:{port}").parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };

    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut buf = [0u8; 256];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return false,
    };
    let response = String::from_utf8_lossy(&buf[..n]);
    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

pub fn ssh_health_check(user: &str, host: &str, target_host: &str, port: u16, path: &str) -> bool {
    let cmd = format!(
        "wget -q -O /dev/null --timeout=2 http://{target_host}:{port}{path} 2>/dev/null && echo ok"
    );
    let out = Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("ConnectTimeout=3")
        .arg(format!("{user}@{host}"))
        .arg(&cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(out) => out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ok"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Metrics parsing
// ---------------------------------------------------------------------------

pub fn spinr_metrics(value: &Value) -> &Value {
    value.pointer("/scenarios/0/metrics").unwrap_or(value)
}

pub fn metric_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

pub fn metric_f64(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or_default()
}

pub fn validation_success_rate(metrics: &Value) -> f64 {
    let successful = metric_u64(metrics, "successful_requests").unwrap_or_default();
    let failed = metric_u64(metrics, "failed_requests").unwrap_or_default();
    let total = metric_u64(metrics, "total_requests").unwrap_or(successful + failed);
    if total > 0 {
        successful as f64 / total as f64
    } else if failed == 0 {
        1.0
    } else {
        0.0
    }
}

pub fn validate_spinr_metrics(metrics: &Value) -> Result<(), String> {
    let successful = metric_u64(metrics, "successful_requests").unwrap_or_default();
    let failed = metric_u64(metrics, "failed_requests").unwrap_or_default();
    let total = metric_u64(metrics, "total_requests").unwrap_or(successful + failed);
    let success_rate = validation_success_rate(metrics);

    if successful == 0 {
        return Err(format!(
            "benchmark produced no successful requests (status_codes={})",
            format_status_codes(metrics)
        ));
    }
    if failed > 0 {
        return Err(format!(
            "benchmark reported {failed} failed requests out of {total} (success={:.1}%, status_codes={})",
            success_rate * 100.0,
            format_status_codes(metrics)
        ));
    }

    Ok(())
}

pub fn format_status_codes(metrics: &Value) -> String {
    metrics
        .get("status_codes")
        .map(Value::to_string)
        .unwrap_or_else(|| "-".into())
}

pub fn val_str(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::Number(number)) => {
            if let Some(float) = number.as_f64() {
                if float == float.floor() && float.abs() < 1e15 {
                    format!("{}", float as i64)
                } else {
                    format!("{float:.3}")
                }
            } else {
                number.to_string()
            }
        }
        Some(other) => other.to_string(),
        None => "-".into(),
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

pub fn chrono_lite_utc() -> String {
    match Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S UTC"])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    }
}

pub fn timestamp_slug() -> String {
    match Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H-%M-%SZ"])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    }
}
