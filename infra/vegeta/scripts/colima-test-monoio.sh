#!/bin/bash
# Test Harrow monoio (io_uring) server in Colima VM
# This script manages Colima lifecycle for io_uring testing

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COLIMA_CONFIG="$PROJECT_ROOT/infra/vegeta/colima-monoio.yml"
COMPOSE_FILE="$PROJECT_ROOT/infra/vegeta/docker-compose.colima.yml"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if Colima is installed
check_colima() {
    if ! command -v colima &> /dev/null; then
        log_error "Colima is not installed"
        echo "Install with: brew install colima"
        exit 1
    fi
    log_success "Colima found: $(colima version | head -1)"
}

# Check Colima status
check_colima_status() {
    if colima status 2>/dev/null | grep -q "Running"; then
        return 0
    else
        return 1
    fi
}

# Start Colima with io_uring support
start_colima() {
    log_info "Starting Colima with io_uring support..."
    
    if check_colima_status; then
        log_warn "Colima is already running"
        read -p "Stop and restart with io-uring config? (y/N) " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            colima stop
        else
            log_info "Using existing Colima instance"
            return 0
        fi
    fi
    
    # Start with custom config
    colima start --config "$COLIMA_CONFIG" --profile monoio
    
    # Wait for Colima to be ready
    log_info "Waiting for Colima to be ready..."
    for i in {1..30}; do
        if colima status --profile monoio 2>/dev/null | grep -q "Running"; then
            break
        fi
        sleep 1
    done
    
    log_success "Colima is running"
    
    # Check kernel version
    log_info "Checking kernel version..."
    colima ssh --profile monoio -- uname -r
    
    # Check for io_uring support
    log_info "Checking io_uring support..."
    if colima ssh --profile monoio -- "grep CONFIG_IO_URING /boot/config-* 2>/dev/null || echo 'checking /proc...'"; then
        if colima ssh --profile monoio -- "ls /proc/sys/kernel/io_uring* 2>/dev/null || echo 'io_uring sysctl not exposed'"; then
            log_success "io_uring appears to be available"
        fi
    fi
}

# Build and test monoio server
test_monoio() {
    log_info "Building monoio server in Colima..."
    
    cd "$PROJECT_ROOT"
    
    # Set Docker context to Colima
    eval $(colima nerdctl --profile monoio env 2>/dev/null || echo "# Using docker context")
    
    # Build and run tests
    export COMPOSE_PROJECT_NAME=harrow-monoio-test
    
    log_info "Starting services..."
    docker-compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true
    docker-compose -f "$COMPOSE_FILE" up --build -d monoio-server
    
    log_info "Waiting for monoio server..."
    sleep 10
    
    # Check if server is healthy
    if docker-compose -f "$COMPOSE_FILE" ps | grep -q "healthy"; then
        log_success "Monoio server is healthy"
    else
        log_warn "Server health check pending, checking logs..."
        docker-compose -f "$COMPOSE_FILE" logs --tail=20 monoio-server
    fi
    
    # Run Vegeta tests
    log_info "Running Vegeta load tests..."
    docker-compose -f "$COMPOSE_FILE" up vegeta
    
    # Copy results
    log_info "Copying results..."
    mkdir -p "$PROJECT_ROOT/infra/vegeta/results/colima"
    docker cp "$(docker-compose -f "$COMPOSE_FILE" ps -q vegeta):/results/." "$PROJECT_ROOT/infra/vegeta/results/colima/" 2>/dev/null || true
    
    # Cleanup
    log_info "Cleaning up..."
    docker-compose -f "$COMPOSE_FILE" down -v
    
    log_success "Test complete! Results in: infra/vegeta/results/colima/"
}

# Run local test (native binary in Colima)
test_monoio_local() {
    log_info "Building monoio server natively..."
    
    cd "$PROJECT_ROOT"
    
    # Build for Linux target
    if ! command -v cross &> /dev/null; then
        log_info "Installing cross for cross-compilation..."
        cargo install cross
    fi
    
    log_info "Building with cross..."
    cross build --target aarch64-unknown-linux-gnu \
        --example vegeta_target_monoio \
        --features monoio,json --no-default-features -p harrow
    
    # Copy binary to Colima
    log_info "Copying binary to Colima..."
    colima cp --profile monoio \
        "$PROJECT_ROOT/target/aarch64-unknown-linux-gnu/debug/examples/vegeta_target_monoio" \
        colima:/tmp/server
    
    # Run server in Colima
    log_info "Starting server in Colima..."
    colima ssh --profile monoio -- "chmod +x /tmp/server && BIND_ADDR=0.0.0.0:3000 /tmp/server" &
    
    # Forward port
    log_info "Setting up port forwarding..."
    colima ssh --profile monoio -- -L 3000:localhost:3000 -N &
    FORWARD_PID=$!
    
    # Wait for server
    sleep 5
    
    # Run local Vegeta
    log_info "Running Vegeta tests..."
    cd "$PROJECT_ROOT/infra/vegeta"
    SERVER_URL=http://localhost:3000 ./scripts/run-all-vegeta-tests.sh
    
    # Cleanup
    kill $FORWARD_PID 2>/dev/null || true
    colima ssh --profile monoio -- "pkill -f /tmp/server" 2>/dev/null || true
}

# Stop Colima
stop_colima() {
    log_info "Stopping Colima..."
    colima stop --profile monoio 2>/dev/null || true
    log_success "Colima stopped"
}

# Main command handler
case "${1:-help}" in
    start)
        check_colima
        start_colima
        ;;
    test|docker)
        check_colima
        start_colima
        test_monoio
        ;;
    test-local|native)
        check_colima
        start_colima
        test_monoio_local
        ;;
    stop)
        stop_colima
        ;;
    status)
        colima status --profile monoio
        ;;
    ssh)
        colima ssh --profile monoio
        ;;
    help|*)
        echo "Harrow Monoio (io_uring) Testing with Colima"
        echo ""
        echo "Usage: $0 <command>"
        echo ""
        echo "Commands:"
        echo "  start       Start Colima VM with io_uring support"
        echo "  test        Run tests using Docker Compose (recommended)"
        echo "  test-local  Run tests with native binary (faster)"
        echo "  stop        Stop Colima VM"
        echo "  status      Check Colima status"
        echo "  ssh         SSH into Colima VM"
        echo ""
        echo "Examples:"
        echo "  $0 start          # Start the VM"
        echo "  $0 test           # Run full Docker-based tests"
        echo "  $0 test-local     # Run native binary tests (faster)"
        echo "  $0 stop           # Stop the VM"
        ;;
esac
