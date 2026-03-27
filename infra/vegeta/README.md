# Vegeta Load Testing for Harrow

This directory contains Docker-based load testing using [Vegeta](https://github.com/tsenart/vegeta) via the [peterevans/vegeta](https://github.com/peter-evans/vegeta-docker) image.

## Quick Start

```bash
cd infra/vegeta

# Run tests against Tokio backend
docker-compose up --build

# Results will be saved to ./results/
```

## Test Coverage

The test suite covers:

| Test | Target File | Description |
|------|-------------|-------------|
| Basic GET | `basic-get.txt` | Root, health, liveness, readiness endpoints |
| Path Params | `path-params.txt` | Dynamic route parameters (`/users/:id`) |
| JSON Body | `json-body.txt` | POST with JSON parsing and response |
| Mixed Methods | `mixed-methods.txt` | GET, POST, PUT, DELETE |
| CPU Intensive | `cpu-intensive.txt` | Computation-heavy handlers |
| 404 Errors | `404-errors.txt` | Error handling and 404 responses |

## Architecture

```
┌─────────────────┐      ┌──────────────────┐      ┌─────────────┐
│  vegeta         │──────▶  tokio-server    │      │   monoio    │
│  (official      │      │  (port 3000)     │      │  (port 3001)│
│   image)        │      └──────────────────┘      └─────────────┘
└─────────────────┘
```

## Manual Testing

### Start the server locally

```bash
# Tokio backend
cargo run --example vegeta_target_tokio --features tokio,timeout,request-id,cors,json,o11y

# Monoio backend (Linux only)
cargo run --example vegeta_target_monoio --features monoio,json --no-default-features
```

### Run Vegeta locally

```bash
# Install vegeta
go install github.com/tsenart/vegeta@latest

# Simple test
echo "GET http://localhost:3000/" | vegeta attack -duration=30s -rate=1000 | vegeta report

# With target file
vegeta attack -targets=infra/vegeta/targets/basic-get.txt -duration=30s -rate=1000 | vegeta report

# Generate latency histogram
vegeta attack -targets=targets/basic-get.txt -duration=30s -rate=1000 -output=results.bin
vegeta report -type=latency results.bin
vegeta report -type=histogram results.bin
```

## Docker Usage

### Run all tests

```bash
cd infra/vegeta
docker-compose up --build
```

### Interactive shell with Vegeta

```bash
docker-compose run vegeta-shell

# Inside container
vegeta attack -targets=/targets/basic-get.txt -duration=30s -rate=1000 | vegeta report
```

### Compare backends

```bash
# Terminal 1: Start Tokio server
cargo run --example vegeta_target_tokio --features tokio,json

# Terminal 2: Run vegeta against it
docker run --rm --network=host \
  -v $(pwd)/infra/vegeta/targets:/targets:ro \
  peterevans/vegeta:latest \
  sh -c 'echo "GET http://localhost:3000/" | vegeta attack -duration=30s -rate=1000 | vegeta report'
```

### Custom test parameters

```bash
# Longer duration, higher rate
docker-compose run -e DURATION=120s -e RATE=5000 vegeta
```

## Interpreting Results

Example output:

```
Requests      [total, rate, throughput]  30000, 1000.03, 999.83
Duration      [total, attack, wait]      30.005s, 29.999s, 5.983ms
Latencies     [mean, 50, 95, 99, max]    2.341ms, 1.892ms, 5.234ms, 12.456ms, 45.231ms
Bytes In      [total, mean]              570000, 19.00
Bytes Out     [total, mean]              0, 0.00
Success       [ratio]                    100.00%
Status Codes  [code:count]               200:30000
```

Key metrics:
- **Throughput**: Actual requests/second processed
- **Latencies**: Response time distribution (mean, p50, p95, p99)
- **Success ratio**: Percentage of non-error responses
- **Status Codes**: HTTP response code distribution

## Troubleshooting

### Monoio container fails to start

io_uring is blocked by default in many container runtimes. Run with:

```bash
docker run --privileged your-image
```

Or in docker-compose:

```yaml
services:
  monoio-server:
    privileged: true
```

### Connection refused

Ensure the server healthcheck passes before running tests:

```bash
curl http://localhost:3000/health
```

## CI Integration

Example GitHub Actions workflow:

```yaml
- name: Run Vegeta Tests
  run: |
    cd infra/vegeta
    docker-compose up --build --abort-on-container-exit
    
- name: Upload Results
  uses: actions/upload-artifact@v4
  with:
    name: vegeta-results
    path: infra/vegeta/results/
```

## Resources

- [Vegeta GitHub](https://github.com/tsenart/vegeta)
- [Vegeta Docker Image](https://github.com/peter-evans/vegeta-docker)
- [Vegeta Usage Guide](https://github.com/tsenart/vegeta#usage)
