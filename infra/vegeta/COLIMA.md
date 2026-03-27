# Testing Monoio with Colima

[Colima](https://github.com/abiosoft/colima) runs Linux containers on macOS, allowing us to test io_uring features locally.

## Prerequisites

```bash
# Install Colima
brew install colima docker docker-compose

# Start default Colima instance (if not already running)
colima start
```

## Quick Test (Docker Method)

```bash
cd infra/vegeta

# 1. Start Colima with privileged mode (required for io_uring)
colima start --privileged --cpu 4 --memory 8

# 2. Use Colima's Docker context
docker context use colima

# 3. Build and run monoio server
docker-compose -f docker-compose.colima.yml up --build monoio-server

# 4. In another terminal, run Vegeta tests
docker-compose -f docker-compose.colima.yml up vegeta
```

## Using the Helper Script

```bash
cd infra/vegeta

# Start Colima VM
./scripts/colima-test-monoio.sh start

# Run full test suite (Docker-based)
./scripts/colima-test-monoio.sh test

# Or run native binary tests (faster)
./scripts/colima-test-monoio.sh test-local

# Stop VM when done
./scripts/colima-test-monoio.sh stop
```

## Manual Steps

### 1. Start Colima VM

```bash
colima start --privileged \
  --cpu 4 \
  --memory 8 \
  --disk 60 \
  --profile monoio
```

### 2. Check Kernel Version

```bash
colima ssh --profile monoio -- uname -r
# Should be 6.1+ for full io_uring support
```

### 3. Build Monoio Server

```bash
# Set Docker context to Colima
eval $(colima nerdctl env)

# Build the server
cd infra/vegeta
docker-compose -f docker-compose.colima.yml build monoio-server
```

### 4. Run Server

```bash
docker-compose -f docker-compose.colima.yml up monoio-server
```

### 5. Test with Vegeta

```bash
# From your Mac (with port forwarding)
colima ssh --profile monoio -- -L 3000:localhost:3000 -N &

# Run Vegeta locally
echo "GET http://localhost:3000/" | vegeta attack -duration=10s -rate=1000 | vegeta report
```

## io_uring Verification

Check if io_uring is available in the Colima VM:

```bash
colima ssh --profile monoio

# Check kernel config
grep CONFIG_IO_URING /boot/config-*

# Check if io_uring syscalls are available
# (monoio will fail fast with clear error if not)
```

## Troubleshooting

### io_uring not available

If Colima's kernel doesn't support io_uring, Monoio will fall back to epoll (via `FusionDriver`). Check logs:

```bash
docker-compose -f docker-compose.colima.yml logs monoio-server
```

Look for:
- `io_uring available` - Using io_uring
- `falling back to epoll` - Using epoll fallback

### Permission denied

Make sure to run with `--privileged` flag:

```bash
colima stop
colima start --privileged
```

### Slow performance

Colima runs in a VM, so expect some overhead. For best results:
- Allocate sufficient CPU: `--cpu 4`
- Allocate sufficient memory: `--memory 8`
- Use volume mounts wisely (avoid heavy I/O)

## Comparison: Tokio vs Monoio in Colima

```bash
# Test Tokio (baseline)
docker-compose up tokio-server
docker-compose up vegeta

# Test Monoio (io_uring)
docker-compose -f docker-compose.colima.yml up monoio-server
docker-compose -f docker-compose.colima.yml up vegeta
```

Expected results:
- **Tokio**: Good performance, works everywhere
- **Monoio**: Similar or better performance (depending on kernel), requires Linux 6.1+

## Cleanup

```bash
# Stop containers
docker-compose -f docker-compose.colima.yml down

# Stop Colima
colima stop --profile monoio

# Delete Colima VM (if needed)
colima delete --profile monoio
```
