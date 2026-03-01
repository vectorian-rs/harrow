# Harrow Benchmark Infrastructure

Terraform setup for two-instance AWS benchmarking: a dedicated server and client in the same AZ with a cluster placement group for minimal network jitter.

## Prerequisites

- [Terraform](https://www.terraform.io/downloads) >= 1.5
- AWS CLI configured with appropriate credentials
- An SSH key pair registered in AWS EC2
- Docker (for local image builds)

## Quick Start

```bash
cd infra

# Initialize
terraform init

# Plan (review what will be created)
terraform plan -var="key_name=your-ssh-key"

# Apply
terraform apply -var="key_name=your-ssh-key"
```

## What Gets Created

- **Placement group** (cluster strategy) — same rack, minimal jitter
- **Security group** — SSH from your IP, ports 3000-3100 between instances
- **2 spot instances** (c7g.xlarge by default) — Graviton3, ~$0.04/hr each
  - Both build harrow Docker images and mcp-load-tester automatically via user-data

## Docker Images

The server binaries are packaged as Docker containers using a multi-target Dockerfile:

```bash
# Build locally
docker build --provenance=false --target harrow-server -t harrow-server .
docker build --provenance=false --target axum-server -t axum-server .

# Run locally (test)
docker run -p 3090:3000 harrow-server
docker run -p 3091:3000 axum-server
```

Images use `gcr.io/distroless/cc-debian13` as the runtime base (~<100MB final size).

## After `terraform apply`

Wait for provisioning to complete (~5-8 minutes for Docker build + mcp-load-tester compilation):

```bash
# Check if provisioning is done
ssh -i ~/.ssh/YOUR_KEY.pem ec2-user@$(terraform output -raw server_public_ip) \
  'test -f /tmp/user-data-complete && echo READY || echo PROVISIONING'
```

Then run the benchmark:

```bash
# 1. SSH into server, start both frameworks via Docker
ssh -i ~/.ssh/YOUR_KEY.pem ec2-user@$(terraform output -raw server_public_ip)
# On server:
docker run -d --network=host --name harrow \
  harrow-server /harrow-server --bind 0.0.0.0 --port 3090
docker run -d --network=host --name axum \
  axum-server /axum-server --bind 0.0.0.0 --port 3091

# 2. SSH into client, run comparison
ssh -i ~/.ssh/YOUR_KEY.pem ec2-user@$(terraform output -raw client_public_ip)
# On client:
SERVER_IP=$(terraform output -raw server_private_ip)
~/mcp-load-tester/target/release/bench \
  --url http://$SERVER_IP:3090/ --connections 128 --duration 30s
~/mcp-load-tester/target/release/bench \
  --url http://$SERVER_IP:3091/ --connections 128 --duration 30s
```

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
