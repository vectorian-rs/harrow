#!/usr/bin/env bash
set -euo pipefail

output="$(terraform -chdir=infra/ec2-spot output -json)"

server_ip="$(jq -r '.server_public_ip.value' <<<"$output")"
client_ip="$(jq -r '.client_public_ip.value' <<<"$output")"
server_private_ip="$(jq -r '.server_private_ip.value' <<<"$output")"

case "${1:-}" in
  --exports)
    printf 'BENCH_SERVER_IP=%q\n' "$server_ip"
    printf 'BENCH_CLIENT_IP=%q\n' "$client_ip"
    printf 'BENCH_SERVER_PRIVATE_IP=%q\n' "$server_private_ip"
    ;;
  --server)
    printf '%s\n' "$server_ip"
    ;;
  --client)
    printf '%s\n' "$client_ip"
    ;;
  --server-private)
    printf '%s\n' "$server_private_ip"
    ;;
  *)
    printf 'usage: %s [--exports|--server|--client|--server-private]\n' "$0" >&2
    exit 1
    ;;
esac
