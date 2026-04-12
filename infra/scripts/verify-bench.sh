#!/usr/bin/env bash
set -euo pipefail

SERVER_IP="${1:?Usage: verify-bench.sh SERVER_IP CLIENT_IP}"
CLIENT_IP="${2:?Usage: verify-bench.sh SERVER_IP CLIENT_IP}"
SSH="ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5"

FMT='{{.Repository}}:{{.Tag}}  {{.Size}}'

echo "=== SERVER ($SERVER_IP) ==="
$SSH "alpine@$SERVER_IP" "
echo '--- kernel ---'
uname -r
echo '--- sysctl ---'
sysctl net.core.somaxconn net.core.netdev_max_backlog net.ipv4.tcp_max_syn_backlog \
       net.core.rmem_max net.core.wmem_max net.ipv4.ip_local_port_range \
       net.ipv4.tcp_tw_reuse vm.swappiness kernel.perf_event_paranoid fs.file-max
echo '--- ulimit ---'
ulimit -n
echo '--- cpu ---'
nproc
echo '--- docker images ---'
docker images --format '$FMT' | sort
"

echo ""
echo "=== CLIENT ($CLIENT_IP) ==="
$SSH "alpine@$CLIENT_IP" "
echo '--- kernel ---'
uname -r
echo '--- sysctl ---'
sysctl net.core.somaxconn net.ipv4.ip_local_port_range net.ipv4.tcp_tw_reuse fs.file-max
echo '--- ulimit ---'
ulimit -n
echo '--- cpu ---'
nproc
echo '--- docker images ---'
docker images --format '$FMT' | sort
"
