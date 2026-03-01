#!/bin/bash
set -euxo pipefail

# ---------------------------------------------------------------------------
# Cloud-init provisioning for Harrow benchmark instances
# Role: ${role} (server or client)
# ---------------------------------------------------------------------------

exec > /var/log/user-data.log 2>&1

# System packages — Docker for harrow images, Rust deps for mcp-load-tester
dnf install -y docker git jq curl gcc gcc-c++ make openssl-devel pkg-config

# Start Docker
systemctl enable docker
systemctl start docker
usermod -aG docker ec2-user

# OS tuning for benchmarks
cat >> /etc/sysctl.d/99-bench.conf <<'SYSCTL'
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.core.netdev_max_backlog = 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.ip_local_port_range = 1024 65535
SYSCTL
sysctl --system

# Raise file descriptor limits
cat >> /etc/security/limits.d/99-bench.conf <<'LIMITS'
* soft nofile 65535
* hard nofile 65535
LIMITS

# Build as ec2-user
su - ec2-user -c '
  # Build harrow server images via Docker
  cd ~
  git clone --branch ${branch} ${repo_url} harrow
  cd harrow
  docker build --provenance=false --target harrow-server -t harrow-server .
  docker build --provenance=false --target axum-server -t axum-server .

  # Install Rust and build mcp-load-tester (bench client)
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source ~/.cargo/env
  cd ~
  git clone https://github.com/l1x/mcp-load-tester.git
  cd mcp-load-tester
  cargo build --release --bin bench
'

# Signal completion
touch /tmp/user-data-complete
echo "User data provisioning complete for role: ${role}"
