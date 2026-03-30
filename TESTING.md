# Harrow Testing Guide

This document explains how to test Harrow at different levels of complexity.

## Quick Local Development

### Unit Tests
```bash
cargo test
```

### Specific Crate Tests
```bash
cargo test -p harrow-core
cargo test -p harrow-middleware
cargo test -p harrow-server-tokio
```

### Examples
```bash
# Tokio example
cargo run --example hello --features tokio

# Monoio example (Linux 6.1+ only)
cargo run --example monoio_hello --features monoio
```

## Local Performance Testing

For quick performance validation during development:

### Using Vegeta (Recommended)
```bash
# Start server in background
cargo run --example hello --features tokio &
SERVER_PID=$!

# Wait for server to start
sleep 2

# Run load test
vegeta attack -duration=10s -rate=1000 http://localhost:3000/ | vegeta report

# Cleanup
kill $SERVER_PID
```

### Using wrk (Alternative)
```bash
cargo run --example hello --features tokio &
SERVER_PID=$!
sleep 2
wrk -t12 -c400 -d30s http://localhost:3000/
kill $SERVER_PID
```

### Using cargo bench (Criterion)
```bash
cargo bench
```

## Docker Testing

### Build and Run
```bash
# Build image
docker build -t harrow-server-tokio .

# Run container
docker run -p 3000:3000 harrow-server-tokio

# Test endpoint
curl http://localhost:3000/
```

### Multi-platform Build
```bash
# Build for multiple platforms
docker buildx build --platform linux/amd64,linux/arm64 -t harrow-server-tokio:latest --load .
```

## Full Benchmark Suite (Advanced)

For official performance benchmarking, Harrow provides advanced tooling:

### Local Benchmarks
```bash
# Run all criterion benchmarks
cargo bench

# Run specific benchmarks
cargo bench --bench full_stack
cargo bench --bench middleware_chain
```

### Remote Benchmarks (EC2)
See `infra/README.md` for instructions on setting up full EC2 benchmarking infrastructure using Terraform and Ansible.

## Continuous Integration

In CI environments, Harrow runs:
1. `cargo test --all --release`
2. `cargo bench -- --save-baseline baselines` (performance tracking)
3. Docker build and basic container tests

## Troubleshooting

### Common Issues

**"Address already in use"**
- Kill existing processes: `pkill -f harrow` or `pkill -f "cargo run"`

**"Connection refused"**
- Ensure server is running and bound to correct interface
- Check logs for startup errors

**Performance tests fluctuate**
- Close other applications
- Consider isolating CPU cores for benchmarking
- Run multiple times and take median

## Recommended Workflow

1. **During development**: `cargo test` + manual example testing
2. **Before PR**: Full test suite + local performance check
3. **For release**: Complete benchmark suite on dedicated hardware
