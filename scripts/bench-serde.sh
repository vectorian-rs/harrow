#!/usr/bin/env bash
#
# bench-serde.sh — Cross-node serde benchmark runner
#
# Runs on the CLIENT node. The SERVER node must already have Docker images
# built and Jaeger available. This script orchestrates the full test matrix
# via SSH to start/stop containers on the server, then runs mcp-load-tester
# locally against the remote server.
#
# Usage:
#   bench-serde.sh --server-host IP [--server-user USER] [--duration 60] [--warmup 5]
#
# Prerequisites:
#   - SSH access to server node (key-based)
#   - mcp-load-tester bench binary in PATH or ~/mcp-load-tester/target/release/
#   - Docker images built on server: serde-bench-server, axum-serde-server
#   - Jaeger image pulled on server: jaegertracing/all-in-one:latest

set -euo pipefail

# --- Defaults ---
SERVER_HOST=""
SERVER_USER="alpine"
SERVER_PORT=3090
DURATION=60
WARMUP=5
BENCH_BIN=""
RESULTS_DIR="results"
SLEEP_BETWEEN=2

usage() {
    echo "Usage: $0 --server-host IP [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --server-host IP      Server node IP (required)"
    echo "  --server-user USER    SSH user on server (default: alpine)"
    echo "  --port PORT           Server port (default: 3090)"
    echo "  --duration SECS       Test duration per run (default: 60)"
    echo "  --warmup SECS         Warmup duration per run (default: 5)"
    echo "  --results-dir DIR     Output directory (default: results)"
    exit 1
}

# --- Parse args ---
while [[ $# -gt 0 ]]; do
    case "$1" in
        --server-host) SERVER_HOST="$2"; shift 2 ;;
        --server-user) SERVER_USER="$2"; shift 2 ;;
        --port)        SERVER_PORT="$2"; shift 2 ;;
        --duration)    DURATION="$2"; shift 2 ;;
        --warmup)      WARMUP="$2"; shift 2 ;;
        --results-dir) RESULTS_DIR="$2"; shift 2 ;;
        -h|--help)     usage ;;
        *)             echo "Unknown option: $1"; usage ;;
    esac
done

if [[ -z "$SERVER_HOST" ]]; then
    echo "ERROR: --server-host is required"
    usage
fi

# --- Locate bench binary ---
if command -v bench &>/dev/null; then
    BENCH_BIN="bench"
elif [[ -x "$HOME/mcp-load-tester/target/release/bench" ]]; then
    BENCH_BIN="$HOME/mcp-load-tester/target/release/bench"
else
    echo "ERROR: 'bench' binary not found in PATH or ~/mcp-load-tester/target/release/"
    exit 1
fi

SSH="ssh -o StrictHostKeyChecking=no ${SERVER_USER}@${SERVER_HOST}"

# --- Helper functions ---

ssh_server() {
    $SSH "$@"
}

start_container() {
    local name="$1"
    local image="$2"
    shift 2
    echo ">>> Starting container: $name"
    ssh_server "docker rm -f $name 2>/dev/null || true"
    ssh_server "docker run -d --name $name --network host $* $image"
    sleep 2
}

stop_container() {
    local name="$1"
    echo ">>> Stopping container: $name"
    ssh_server "docker rm -f $name 2>/dev/null || true"
}

collect_docker_stats() {
    local label="$1"
    ssh_server "docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}'" \
        > "${RESULTS_DIR}/stats_${label}.txt" 2>/dev/null || true
}

collect_docker_logs() {
    local container="$1"
    local label="$2"
    ssh_server "docker logs $container 2>&1" \
        > "${RESULTS_DIR}/logs_${label}.txt" 2>/dev/null || true
}

wait_for_health() {
    local url="$1"
    local max_attempts=30
    for i in $(seq 1 $max_attempts); do
        if curl -sf "$url" >/dev/null 2>&1; then
            echo "    Health check passed"
            return 0
        fi
        sleep 1
    done
    echo "ERROR: Server not responding at $url after ${max_attempts}s"
    return 1
}

run_bench() {
    local label="$1"
    local endpoint="$2"
    local concurrency="$3"
    local url="http://${SERVER_HOST}:${SERVER_PORT}${endpoint}"
    local outfile="${RESULTS_DIR}/${label}_c${concurrency}.json"

    echo "  [${label}] c=${concurrency} → ${url}"
    $BENCH_BIN run "$url" \
        --max-throughput \
        --connections "$concurrency" \
        --duration "$DURATION" \
        --warmup "$WARMUP" \
        --json > "$outfile" 2>/dev/null

    # Print quick summary
    if command -v jq &>/dev/null && [[ -s "$outfile" ]]; then
        local rps latency_p99
        rps=$(jq -r '.rps // .requests_per_sec // "n/a"' "$outfile" 2>/dev/null || echo "n/a")
        latency_p99=$(jq -r '.latency_p99_ms // .latency_percentiles.p99 // "n/a"' "$outfile" 2>/dev/null || echo "n/a")
        echo "    → rps=${rps} p99=${latency_p99}ms"
    fi

    sleep "$SLEEP_BETWEEN"
}

# --- Setup ---
mkdir -p "$RESULTS_DIR"

echo "============================================"
echo " Serde Benchmark Suite"
echo " Server: ${SERVER_HOST}:${SERVER_PORT}"
echo " Duration: ${DURATION}s  Warmup: ${WARMUP}s"
echo " Results: ${RESULTS_DIR}/"
echo "============================================"
echo ""

# --- Phase A: Raw serialization (no o11y) ---
echo "========== PHASE A: Raw serialization =========="

ENDPOINTS=("text" "json/small" "json/1kb" "json/10kb" "msgpack/small" "msgpack/1kb" "msgpack/10kb")
CONCURRENCIES=(1 8 32 128)

# --- Phase A.1: Harrow ---
echo ""
echo "--- Harrow (no o11y) ---"
start_container "serde-bench-server" "serde-bench-server"
wait_for_health "http://${SERVER_HOST}:${SERVER_PORT}/health"

for ep in "${ENDPOINTS[@]}"; do
    ep_label="${ep//\//_}"
    for c in "${CONCURRENCIES[@]}"; do
        run_bench "harrow_${ep_label}" "/${ep}" "$c"
    done
done

collect_docker_stats "harrow_raw"
collect_docker_logs "serde-bench-server" "harrow_raw"
stop_container "serde-bench-server"

echo ""
echo "--- Axum ---"
start_container "axum-serde-server" "axum-serde-server"
wait_for_health "http://${SERVER_HOST}:${SERVER_PORT}/health"

for ep in "${ENDPOINTS[@]}"; do
    ep_label="${ep//\//_}"
    for c in "${CONCURRENCIES[@]}"; do
        run_bench "axum_${ep_label}" "/${ep}" "$c"
    done
done

collect_docker_stats "axum_raw"
collect_docker_logs "axum-serde-server" "axum_raw"
stop_container "axum-serde-server"

# --- Phase B: O11y overhead (Harrow only) ---
echo ""
echo "========== PHASE B: O11y overhead (Harrow) =========="

# Start Jaeger on server
echo "--- Starting Jaeger ---"
ssh_server "docker rm -f jaeger 2>/dev/null || true"
ssh_server "docker run -d --name jaeger --network host jaegertracing/all-in-one:latest"
sleep 3

# Start harrow with --o11y pointing to local Jaeger
start_container "serde-bench-o11y" "serde-bench-server" \
    -e OTLP_ENDPOINT=http://127.0.0.1:4318 \
    -- /serde-bench-server --bind 0.0.0.0 --o11y
wait_for_health "http://${SERVER_HOST}:${SERVER_PORT}/health"

O11Y_ENDPOINTS=("text" "json/1kb" "msgpack/1kb")
O11Y_CONCURRENCIES=(1 32 128)

for ep in "${O11Y_ENDPOINTS[@]}"; do
    ep_label="${ep//\//_}"
    for c in "${O11Y_CONCURRENCIES[@]}"; do
        run_bench "harrow_o11y_${ep_label}" "/${ep}" "$c"
    done
done

collect_docker_stats "harrow_o11y"
collect_docker_logs "serde-bench-o11y" "harrow_o11y"

# Export Jaeger traces
echo ""
echo "--- Exporting Jaeger traces ---"
curl -sf "http://${SERVER_HOST}:16686/api/traces?service=harrow-bench-o11y&limit=100" \
    > "${RESULTS_DIR}/jaeger_traces.json" 2>/dev/null || echo "  (Jaeger export failed or no traces)"

stop_container "serde-bench-o11y"
stop_container "jaeger"

# --- Summary ---
echo ""
echo "========== GENERATING SUMMARY =========="

{
    echo "# Serde Benchmark Results"
    echo ""
    echo "Server: ${SERVER_HOST}:${SERVER_PORT}"
    echo "Duration: ${DURATION}s | Warmup: ${WARMUP}s"
    echo "Date: $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
    echo ""
    echo "## Phase A: Raw Serialization"
    echo ""
    echo "| Framework | Endpoint | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |"
    echo "|-----------|----------|-------------|-----|----------|----------|-----------|"

    for fw in harrow axum; do
        for ep in "${ENDPOINTS[@]}"; do
            ep_label="${ep//\//_}"
            for c in "${CONCURRENCIES[@]}"; do
                f="${RESULTS_DIR}/${fw}_${ep_label}_c${c}.json"
                if [[ -f "$f" ]] && command -v jq &>/dev/null; then
                    rps=$(jq -r '.rps // .requests_per_sec // "-"' "$f" 2>/dev/null || echo "-")
                    p50=$(jq -r '.latency_p50_ms // .latency_percentiles.p50 // "-"' "$f" 2>/dev/null || echo "-")
                    p99=$(jq -r '.latency_p99_ms // .latency_percentiles.p99 // "-"' "$f" 2>/dev/null || echo "-")
                    p999=$(jq -r '.latency_p999_ms // .latency_percentiles.p999 // "-"' "$f" 2>/dev/null || echo "-")
                    echo "| ${fw} | /${ep} | ${c} | ${rps} | ${p50} | ${p99} | ${p999} |"
                fi
            done
        done
    done

    echo ""
    echo "## Phase B: O11y Overhead (Harrow)"
    echo ""
    echo "| Endpoint | Concurrency | RPS | p50 (ms) | p99 (ms) | p999 (ms) |"
    echo "|----------|-------------|-----|----------|----------|-----------|"

    for ep in "${O11Y_ENDPOINTS[@]}"; do
        ep_label="${ep//\//_}"
        for c in "${O11Y_CONCURRENCIES[@]}"; do
            f="${RESULTS_DIR}/harrow_o11y_${ep_label}_c${c}.json"
            if [[ -f "$f" ]] && command -v jq &>/dev/null; then
                rps=$(jq -r '.rps // .requests_per_sec // "-"' "$f" 2>/dev/null || echo "-")
                p50=$(jq -r '.latency_p50_ms // .latency_percentiles.p50 // "-"' "$f" 2>/dev/null || echo "-")
                p99=$(jq -r '.latency_p99_ms // .latency_percentiles.p99 // "-"' "$f" 2>/dev/null || echo "-")
                p999=$(jq -r '.latency_p999_ms // .latency_percentiles.p999 // "-"' "$f" 2>/dev/null || echo "-")
                echo "| /${ep} | ${c} | ${rps} | ${p50} | ${p99} | ${p999} |"
            fi
        done
    done
} > "${RESULTS_DIR}/summary.md"

echo "Summary written to ${RESULTS_DIR}/summary.md"
echo ""
echo "Done! Results in ${RESULTS_DIR}/"
ls -lh "${RESULTS_DIR}/"
