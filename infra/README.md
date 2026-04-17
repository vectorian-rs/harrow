# Harrow Benchmark Infrastructure

Terraform + Ansible setup for two AWS benchmark nodes (server + client) in the same AZ and placement group.

- OS: Alpine Linux (ARM64)
- Provisioning: Ansible (idempotent)
- Runtime: Docker

## Prerequisites

- Terraform >= 1.5
- Ansible >= 2.15
- AWS CLI configured for your account
- EC2 SSH key pair already created in AWS

## Canonical Workflow

Use this exact flow:

```bash
export AWS_PROFILE=datadeft-dev

# 1) Create/update infra
terraform -chdir=infra init
terraform -chdir=infra apply -var="key_name=your-key"

# 2) Render inventory from Terraform outputs
terraform -chdir=infra output -raw ansible_inventory > infra/ansible/inventory.ini

# 3) Provision hosts (run from infra/ansible so ansible.cfg + roles_path resolve)
cd infra/ansible
ansible-playbook playbooks/bench.yml
```

Re-provisioning is the same last step:

```bash
cd infra/ansible
ansible-playbook playbooks/bench.yml
```

## What Ansible Configures

`os` role:
- `/etc/sysctl.d/99-bench.conf` network + kernel tuning
- `/etc/security/limits.d/99-bench.conf` with `nofile=32000`
- `chrony` with Amazon Time Sync

`bench` role:
- Docker + perf tooling packages
- ECR login
- Bench images (`harrow-perf-server`, `axum-perf-server`, `spinr`)
- Vector image + `~/vector.toml` on client
- Docker daemon default `nofile=32000`

## Quick Smoke Run

```bash
export AWS_PROFILE=datadeft-dev
SERVER_PUB=$(terraform -chdir=infra output -raw server_public_ip)
CLIENT_PUB=$(terraform -chdir=infra output -raw client_public_ip)
SERVER_PRIV=$(terraform -chdir=infra output -raw server_private_ip)

# Start server container
ssh -i ~/.ssh/your-key.pem alpine@"$SERVER_PUB" \
  'docker rm -f harrow 2>/dev/null || true; docker run -d --name harrow --network host harrow-perf-server'

# Run load from client
ssh -i ~/.ssh/your-key.pem alpine@"$CLIENT_PUB" \
  "docker run --rm --network host spinr load-test --max-throughput -c 128 -d 30 -w 5 -j http://$SERVER_PRIV:3090/bare/text"
```

## Variables

| Variable | Default | Description |
|---|---|---|
| `key_name` | required | EC2 SSH key pair name |
| `region` | `eu-west-1` | AWS region |
| `instance_type` | `c8g.12xlarge` | EC2 instance type |
| `spot_price` | `""` | Spot max price override |
| `repo_url` | `https://github.com/l1x/harrow.git` | Reserved variable |
| `branch` | `main` | Reserved variable |

## Verification Checklist

```bash
# automated (reads infra/ansible/inventory.ini)
mise run bench:verify

# manual spot checks on each host
sysctl net.core.somaxconn  # 65535
sysctl fs.file-max         # 2097152
ulimit -n                  # 32000
chronyc sources
docker info
```

## Cleanup

```bash
export AWS_PROFILE=datadeft-dev
terraform -chdir=infra destroy -var="key_name=your-key"
```
