#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  verify-bench.sh SERVER_IP CLIENT_IP
  verify-bench.sh INVENTORY_FILE

If an inventory file is provided, the first host under [server] and [client]
is used.
EOF
}

inventory_server_ip() {
  awk '
    /^\[server\]/ { in_server=1; next }
    /^\[/ && $0 != "[server]" { in_server=0 }
    in_server && NF { print $1; exit }
  ' "$1"
}

inventory_client_ip() {
  awk '
    /^\[client\]/ { in_client=1; next }
    /^\[/ && $0 != "[client]" { in_client=0 }
    in_client && NF { print $1; exit }
  ' "$1"
}

if [ "$#" -eq 1 ] && [ -f "$1" ]; then
  SERVER_IP="$(inventory_server_ip "$1")"
  CLIENT_IP="$(inventory_client_ip "$1")"
elif [ "$#" -eq 2 ]; then
  SERVER_IP="$1"
  CLIENT_IP="$2"
else
  usage >&2
  exit 1
fi

if [ -z "${SERVER_IP:-}" ] || [ -z "${CLIENT_IP:-}" ]; then
  echo "failed to resolve server/client IPs" >&2
  exit 1
fi

HARROW_VERSION="${HARROW_VERSION:-0.10.0}"
SPINR_VERSION="${SPINR_VERSION:-0.5.1}"
WRK3_VERSION="${WRK3_VERSION:-0.2.0}"

SSH_OPTS=(
  -o StrictHostKeyChecking=no
  -o ConnectTimeout=5
)

run_remote_verify() {
  local role="$1"
  local ip="$2"

  ssh "${SSH_OPTS[@]}" "alpine@$ip" \
    "sh -s -- '$role' '$HARROW_VERSION' '$SPINR_VERSION' '$WRK3_VERSION'" <<'EOF'
set -eu

role="$1"
harrow_version="$2"
spinr_version="$3"
wrk3_version="$4"

failed=0

check_eq() {
  key="$1"
  expected="$2"
  actual="$3"

  if [ "$actual" = "$expected" ]; then
    printf 'ok   %-32s %s\n' "$key" "$actual"
  else
    printf 'FAIL %-32s got=%s expected=%s\n' "$key" "$actual" "$expected" >&2
    failed=1
  fi
}

check_image() {
  image="$1"
  if docker image inspect "$image" >/dev/null 2>&1; then
    printf 'ok   image %-26s present\n' "$image"
  else
    printf 'FAIL image %-26s missing\n' "$image" >&2
    failed=1
  fi
}

echo "--- kernel ---"
uname -r

echo "--- tuning ---"
check_eq "net.core.somaxconn" "65535" "$(sysctl -n net.core.somaxconn)"
check_eq "net.core.netdev_max_backlog" "65535" "$(sysctl -n net.core.netdev_max_backlog)"
check_eq "net.ipv4.tcp_max_syn_backlog" "65535" "$(sysctl -n net.ipv4.tcp_max_syn_backlog)"
check_eq "net.core.rmem_max" "16777216" "$(sysctl -n net.core.rmem_max)"
check_eq "net.core.wmem_max" "16777216" "$(sysctl -n net.core.wmem_max)"
check_eq "net.ipv4.ip_local_port_range" "1024	65535" "$(sysctl -n net.ipv4.ip_local_port_range)"
check_eq "net.ipv4.tcp_tw_reuse" "1" "$(sysctl -n net.ipv4.tcp_tw_reuse)"
check_eq "vm.swappiness" "5" "$(sysctl -n vm.swappiness)"
check_eq "fs.file-max" "2097152" "$(sysctl -n fs.file-max)"
check_eq "kernel.kptr_restrict" "0" "$(sysctl -n kernel.kptr_restrict)"
check_eq "kernel.perf_event_paranoid" "1" "$(sysctl -n kernel.perf_event_paranoid)"

if grep -qx 'rc_ulimit="-n 32000"' /etc/rc.conf; then
  echo "ok   rc_ulimit                        -n 32000"
else
  echo "FAIL rc_ulimit                        expected rc_ulimit=\"-n 32000\"" >&2
  failed=1
fi

if grep -qx 'ulimit -Sn 32000 2>/dev/null; ulimit -Hn 32000 2>/dev/null' /etc/profile.d/bench-ulimit.sh; then
  echo "ok   shell ulimit profile             32000"
else
  echo "FAIL shell ulimit profile             missing 32000 profile hook" >&2
  failed=1
fi

if grep -Eq '"Hard"[[:space:]]*:[[:space:]]*32000' /etc/docker/daemon.json &&
   grep -Eq '"Soft"[[:space:]]*:[[:space:]]*32000' /etc/docker/daemon.json; then
  echo "ok   docker daemon nofile             32000/32000"
else
  echo "FAIL docker daemon nofile             expected Hard/Soft 32000 in /etc/docker/daemon.json" >&2
  failed=1
fi

echo "--- clock ---"
if chronyc sources >/dev/null 2>&1; then
  chronyc sources | sed 's/^/  /'
else
  echo "FAIL chronyc sources unavailable" >&2
  failed=1
fi

echo "--- images ---"
if [ "$role" = "server" ]; then
  check_image "harrow-bench:prod-mimalloc-$harrow_version"
  check_image "harrow-bench:prod-jemalloc-$harrow_version"
  check_image "harrow-bench:prod-sysalloc-$harrow_version"
  check_image "harrow-server-monoio:arm64-$harrow_version"
  check_image "harrow-perf-server:perf-$harrow_version"
  check_image "harrow-perf-server:latest"
  check_image "ntex-perf-server:perf-$harrow_version"
  check_image "ntex-perf-server:latest"
else
  check_image "spinr:arm64-$spinr_version"
  check_image "spinr:latest"
  check_image "wrk3:arm64-$wrk3_version"
fi

echo "--- docker images snapshot ---"
docker images --format '{{.Repository}}:{{.Tag}}' | sort | sed 's/^/  /'

exit "$failed"
EOF
}

echo "=== SERVER ($SERVER_IP) ==="
run_remote_verify server "$SERVER_IP"
echo
echo "=== CLIENT ($CLIENT_IP) ==="
run_remote_verify client "$CLIENT_IP"
