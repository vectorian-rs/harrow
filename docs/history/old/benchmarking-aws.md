# Benchmarking Harrow on AWS

Two-instance setup: dedicated client and server in the same AZ for reproducible latency measurements.

## Instance Selection

- **Type**: `c7g.medium` (1 vCPU) or `c7g.xlarge` (4 vCPU) — Graviton3, consistent performance, no burstable surprises
- **Same AZ**, same **placement group** (cluster strategy) — eliminates network jitter
- **Amazon Linux 2023** — minimal, good kernel defaults

## Provision

```bash
# Create placement group for low-latency
aws ec2 create-placement-group \
  --group-name harrow-bench \
  --strategy cluster

# Launch both instances
for role in server client; do
  aws ec2 run-instances \
    --image-id resolve:ssm:/aws/service/al2023/ami-kernel-default/arm64/latest \
    --instance-type c7g.xlarge \
    --placement GroupName=harrow-bench \
    --key-name your-key \
    --security-group-ids sg-xxx \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=harrow-$role}]" \
    --query 'Instances[0].InstanceId' \
    --output text
done
```

Security group needs port 22 (SSH) and port 3000 (harrow) open between the two instances.

## Server Setup

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Clone and build release
git clone https://github.com/l1x/harrow.git
cd harrow
cargo build --release --example hello

# OS tuning
sudo sysctl -w net.core.somaxconn=65535
sudo sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sudo sysctl -w net.core.netdev_max_backlog=65535
sudo ulimit -n 65535

# Run — bind to 0.0.0.0 so client can reach it
# (set HARROW_ADDR or change the example to bind 0.0.0.0:3000)
RUST_LOG=error ./target/release/examples/hello
```

## Client Setup

```bash
# Install wrk2 (latency-accurate fork of wrk)
sudo dnf install -y git gcc openssl-devel
git clone https://github.com/giltene/wrk2.git
cd wrk2 && make -j$(nproc)

# Also install hey for quick sanity checks
curl -L https://hey-release.s3.us-east-2.amazonaws.com/hey_linux_arm64 -o hey
chmod +x hey
```

## What to Measure

```bash
SERVER=<private-ip>:3000

# 1. Sanity check
curl http://$SERVER/greet/bench

# 2. Latency at fixed rate (wrk2) — most important
#    100 RPS, then 1K, 10K, 50K — find where latency degrades
./wrk -t4 -c100 -d30s -R1000  --latency http://$SERVER/greet/bench
./wrk -t4 -c100 -d30s -R10000 --latency http://$SERVER/greet/bench
./wrk -t4 -c100 -d30s -R50000 --latency http://$SERVER/greet/bench

# 3. Max throughput (saturate)
./wrk -t4 -c200 -d30s http://$SERVER/greet/bench

# 4. Connection scaling — same rate, vary connections
./wrk -t4 -c10   -d30s -R10000 --latency http://$SERVER/greet/bench
./wrk -t4 -c100  -d30s -R10000 --latency http://$SERVER/greet/bench
./wrk -t4 -c1000 -d30s -R10000 --latency http://$SERVER/greet/bench
```

## What to Capture

- **p50, p99, p999 latency** at each rate — the shape of this curve tells you everything
- **Max throughput** before errors appear
- **CPU and memory** on server (`htop`, `pidstat 1`)
- Compare with and without o11y middleware to see its cost in isolation

## Notes

- The example currently binds `127.0.0.1:3000` — change to `0.0.0.0:3000` or make configurable via env var before running remotely
- Run with `RUST_LOG=error` to disable per-request tracing output during benchmarks
- Let each wrk2 run for at least 30s to get stable percentiles
- Discard the first run (cold JIT, TCP slow start) and use subsequent runs for reporting
