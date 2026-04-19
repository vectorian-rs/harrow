#!/usr/bin/env bash
#
# perf-capture.sh — Run a spinr load test and capture perf data
#
# Usage:
#   perf-capture.sh <framework> <server-ip> <client-ip> <server-private-ip> [duration]
#
# framework: harrow | ntex
# duration: spinr measurement duration in seconds (default: 30)
#
set -euo pipefail

FRAMEWORK="${1:?Usage: perf-capture.sh <harrow|ntex> SERVER_IP CLIENT_IP SERVER_PRIVATE_IP [DURATION]}"
SERVER_IP="${2:?}"
CLIENT_IP="${3:?}"
SERVER_PRIVATE_IP="${4:?}"
DURATION="${5:-30}"
WARMUP=5
PORT=3090
CONNS=128
SSH="ssh -o StrictHostKeyChecking=no"
HARROW_VERSION="${HARROW_VERSION:-0.10.0}"

# Output directory
TIMESTAMP=$(date -u +%Y-%m-%dT%H-%M-%SZ)
OUTDIR="perf/${TIMESTAMP}-${FRAMEWORK}"
mkdir -p "$OUTDIR"

echo "=== Perf capture: $FRAMEWORK ==="
echo "  Server:   $SERVER_IP (private: $SERVER_PRIVATE_IP)"
echo "  Client:   $CLIENT_IP"
echo "  Duration: ${DURATION}s (warmup: ${WARMUP}s)"
echo "  Output:   $OUTDIR"
echo ""

# --- Select image and binary ---
case "$FRAMEWORK" in
  harrow)
    IMAGE="harrow-perf-server:perf-${HARROW_VERSION}"
    CMD="/harrow-perf-server --bind 0.0.0.0"
    HEALTH_PATH="/health"
    ;;
  ntex)
    IMAGE="ntex-perf-server:perf-${HARROW_VERSION}"
    CMD="/ntex-perf-server --bind 0.0.0.0"
    HEALTH_PATH="/health"
    ;;
  *)
    echo "Unknown framework: $FRAMEWORK (use harrow or ntex)"
    exit 1
    ;;
esac

# --- Cleanup any previous container ---
echo "[1/7] Cleaning up previous containers..."
$SSH "alpine@$SERVER_IP" "docker rm -f perf-server 2>/dev/null || true"

# --- Start server ---
echo "[2/7] Starting $FRAMEWORK server..."
$SSH "alpine@$SERVER_IP" "docker run -d --name perf-server \
  --network host \
  --ulimit nofile=32000:32000 \
  --security-opt seccomp=unconfined \
  $IMAGE $CMD"

# --- Health check ---
echo "[3/7] Waiting for health check..."
for i in $(seq 1 20); do
  if $SSH "alpine@$SERVER_IP" "curl -sf http://127.0.0.1:${PORT}${HEALTH_PATH} >/dev/null 2>&1"; then
    echo "  Server healthy after ${i}s"
    break
  fi
  if [ "$i" -eq 20 ]; then
    echo "  ERROR: Server not healthy after 20s"
    $SSH "alpine@$SERVER_IP" "docker logs perf-server"
    exit 1
  fi
  sleep 1
done

# --- Get server PID (the main process inside the container) ---
SERVER_PID=$($SSH "alpine@$SERVER_IP" "docker inspect --format '{{.State.Pid}}' perf-server")
echo "  Server PID: $SERVER_PID"

# --- Capture phase: run spinr + perf tools in parallel ---
PERF_DURATION=$((WARMUP + DURATION + 5))

echo "[4/7] Starting perf capture (${PERF_DURATION}s) and spinr load..."

# Start perf record on server (background)
$SSH "alpine@$SERVER_IP" "
  doas perf record -g -F 99 -p $SERVER_PID -o /tmp/perf.data -- sleep $PERF_DURATION &
  doas perf stat -e task-clock,context-switches,cpu-migrations,page-faults -p $SERVER_PID -o /tmp/perf-stat.txt -- sleep $PERF_DURATION &
  doas strace -c -f -p $SERVER_PID -o /tmp/strace.txt 2>/dev/null &
  STRACE_PID=\$!
  sleep $PERF_DURATION
  kill \$STRACE_PID 2>/dev/null || true
" &
PERF_SSH_PID=$!

# Run spinr from client
echo "  Running spinr: ${CONNS} connections, ${DURATION}s..."
$SSH "alpine@$CLIENT_IP" "docker run --rm --network host \
  spinr:arm64-0.5.1 \
  --url http://${SERVER_PRIVATE_IP}:${PORT}/text \
  --connections $CONNS \
  --duration ${DURATION}s \
  --warmup ${WARMUP}s \
  --json" > "$OUTDIR/spinr.json" 2>"$OUTDIR/spinr.stderr" || true

echo "  Spinr complete."

# Wait for perf tools to finish
echo "[5/7] Waiting for perf tools to finish..."
wait $PERF_SSH_PID 2>/dev/null || true
sleep 2

# --- Collect artifacts ---
echo "[6/7] Collecting artifacts..."

# Copy perf data from server
scp -o StrictHostKeyChecking=no "alpine@$SERVER_IP:/tmp/perf.data" "$OUTDIR/perf.data" 2>/dev/null || echo "  WARNING: perf.data not available"
scp -o StrictHostKeyChecking=no "alpine@$SERVER_IP:/tmp/perf-stat.txt" "$OUTDIR/perf-stat.txt" 2>/dev/null || echo "  WARNING: perf-stat.txt not available"
scp -o StrictHostKeyChecking=no "alpine@$SERVER_IP:/tmp/strace.txt" "$OUTDIR/strace.txt" 2>/dev/null || echo "  WARNING: strace.txt not available"

# Generate flamegraph on server (if perf script is available)
$SSH "alpine@$SERVER_IP" "
  doas perf script -i /tmp/perf.data 2>/dev/null | \
  stackcollapse-perf.pl 2>/dev/null | \
  flamegraph.pl --title '$FRAMEWORK perf-${HARROW_VERSION}' > /tmp/flamegraph.svg 2>/dev/null
" && scp -o StrictHostKeyChecking=no "alpine@$SERVER_IP:/tmp/flamegraph.svg" "$OUTDIR/flamegraph.svg" 2>/dev/null || echo "  WARNING: flamegraph generation not available (install FlameGraph tools on server)"

# --- Stop server ---
echo "[7/7] Stopping server..."
$SSH "alpine@$SERVER_IP" "docker rm -f perf-server >/dev/null 2>&1" || true

# --- Summary ---
echo ""
echo "=== Done: $FRAMEWORK ==="
echo "  Artifacts in: $OUTDIR/"
ls -la "$OUTDIR/"
echo ""

# Print spinr summary if available
if [ -f "$OUTDIR/spinr.json" ] && [ -s "$OUTDIR/spinr.json" ]; then
  echo "  Spinr results:"
  python3 -c "
import json, sys
d = json.load(open('$OUTDIR/spinr.json'))
print(f\"  RPS:      {d.get('successful_requests',0) / $DURATION:.0f}\")
print(f\"  p50:      {d.get('latency_p50_us', 'N/A')} us\")
print(f\"  p99:      {d.get('latency_p99_us', 'N/A')} us\")
print(f\"  p99.9:    {d.get('latency_p999_us', 'N/A')} us\")
print(f\"  Errors:   {d.get('failed_requests', 0)}\")
" 2>/dev/null || cat "$OUTDIR/spinr.json"
fi

if [ -f "$OUTDIR/perf-stat.txt" ]; then
  echo ""
  echo "  Perf stat:"
  cat "$OUTDIR/perf-stat.txt"
fi

if [ -f "$OUTDIR/strace.txt" ]; then
  echo ""
  echo "  Syscall summary:"
  cat "$OUTDIR/strace.txt"
fi
