# Harrow Benchmark Infrastructure

Terraform + Ansible setup for two-instance AWS benchmarking: a dedicated server and client in the same AZ with a cluster placement group for minimal network jitter.

**OS:** Alpine Linux (ARM64)
**Provisioning:** Ansible (idempotent, re-runnable)
**Containers:** distroless cc-debian13 (unchanged)

## Prerequisites

- [Terraform](https://www.terraform.io/downloads) >= 1.5
- [Ansible](https://docs.ansible.com/ansible/latest/installation_guide/) >= 2.15
- AWS CLI configured with appropriate credentials
- An SSH key pair registered in AWS EC2

## Quick Start

```bash
cd infra

# Launch instances
terraform init
terraform apply -var="key_name=your-key"

# Generate Ansible inventory from Terraform outputs
terraform output -raw ansible_inventory > ansible/inventory.ini

# Provision both instances
cd ansible
ansible-playbook playbooks/bench.yml
```

## What Gets Created

- **Placement group** (cluster strategy) — same rack, minimal jitter
- **Security group** — SSH from your IP, ports 3000-3100 between instances
- **2 spot instances** (c7g.xlarge by default) — Graviton3, ~$0.04/hr each

## Alpine Linux Notes

- SSH user: `alpine` (not `ec2-user`)
- Privilege escalation: `doas` (not `sudo`)
- Init system: OpenRC (not systemd)
- ARM64 Alpine instances take ~4 min to stop (known ACPI issue)

## Ansible Provisioning

Provisioning is split into two roles:

### `os` role — System tuning
- **sysctl** — Network buffers (16MB rmem/wmem), connection backlog (65535), ephemeral port range (1024-65535), 2M file descriptors, IPv6 disabled, TCP TIME_WAIT reuse
- **limits** — 128K nofile for all users
- **chrony** — Amazon Time Sync (169.254.169.123) with tight polling (minpoll/maxpoll 4)

### `bench` role — Benchmark software
- Docker + harrow/axum server images (`--provenance=false`)
- Rust toolchain via rustup
- mcp-load-tester bench binary

### Re-provisioning

Just re-run the playbook — it's idempotent:

```bash
cd infra/ansible
ansible-playbook playbooks/bench.yml
```

## Running Benchmarks

```bash
# 1. SSH into server, start both frameworks via Docker
ssh -i ~/.ssh/YOUR_KEY.pem alpine@$(terraform output -raw server_public_ip)
docker run -d --network=host --name harrow \
  harrow-server /harrow-server --bind 0.0.0.0 --port 3090
docker run -d --network=host --name axum \
  axum-server /axum-server --bind 0.0.0.0 --port 3091

# 2. SSH into client, run comparison
ssh -i ~/.ssh/YOUR_KEY.pem alpine@$(terraform output -raw client_public_ip)
SERVER_IP=$(terraform output -raw server_private_ip)
~/mcp-load-tester/target/release/bench \
  --url http://$SERVER_IP:3090/ --connections 128 --duration 30s
~/mcp-load-tester/target/release/bench \
  --url http://$SERVER_IP:3091/ --connections 128 --duration 30s
```

## Sysctl Tuning Rationale

| Setting | Value | Why |
|---------|-------|-----|
| `net.core.rmem_max` / `wmem_max` | 16MB | Large socket buffers for high-throughput benchmarks |
| `net.ipv4.tcp_rmem` / `tcp_wmem` | 4K-87K-16M | Auto-tuning range matching buffer maximums |
| `net.core.somaxconn` | 65535 | Accept queue depth for burst connection handling |
| `net.ipv4.tcp_max_syn_backlog` | 65535 | SYN queue for incoming connections |
| `net.core.netdev_max_backlog` | 65535 | NIC → kernel queue depth |
| `net.ipv4.tcp_tw_reuse` | 1 | Reuse TIME_WAIT sockets for rapid reconnection |
| `net.ipv4.tcp_fin_timeout` | 30 | Faster FIN_WAIT2 cleanup |
| `net.ipv4.ip_local_port_range` | 1024-65535 | Maximum ephemeral ports for client connections |
| `fs.file-max` | 2097152 | System-wide file descriptor limit |
| `vm.swappiness` | 5 | Near-zero swap preference for consistent latency |
| `vm.max_map_count` | 262144 | Sufficient memory mappings for Rust allocators |
| `net.ipv6.conf.*.disable_ipv6` | 1 | Remove IPv6 overhead on IPv4-only benchmark traffic |

**Not set:** `tcp_syncookies` — intentionally disabled; syncookies break TCP window scaling and SACK, which harms benchmark accuracy.

## Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `key_name` | (required) | AWS SSH key pair name |
| `instance_type` | `c7g.xlarge` | EC2 instance type (ARM64) |
| `region` | `us-east-1` | AWS region |
| `repo_url` | `https://github.com/l1x/harrow.git` | Repository to clone |
| `branch` | `main` | Git branch to build |

## Cleanup

```bash
terraform destroy -var="key_name=your-ssh-key"
```

Spot instances are `one-time` requests, so they won't respawn after termination.

## Cost

With `c7g.xlarge` spot instances: ~$0.04/hr per instance ($0.08/hr total).
A full benchmark run takes ~30 minutes, costing roughly $0.04 total.

## Verification Checklist

After provisioning, SSH as `alpine` and verify:

```bash
cat /etc/alpine-release          # Alpine version
sysctl net.core.somaxconn        # → 65535
sysctl net.core.rmem_max         # → 16777216
sysctl fs.file-max               # → 2097152
ulimit -n                        # → 128000
chronyc sources                  # → 169.254.169.123
docker ps                        # → running
```
