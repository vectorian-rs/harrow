#!/bin/bash
# Compare Tokio vs Monoio backends using Vegeta
# Usage: ./compare-backends.sh [duration] [rate]

set -e

DURATION="${1:-60s}"
RATE="${2:-5000}"
OUTPUT_DIR="${OUTPUT_DIR:-./results}"
TARGET_DIR="${TARGET_DIR:-./targets/local}"

mkdir -p "$OUTPUT_DIR"

echo "############################################"
echo "# Backend Comparison: Tokio vs Monoio      #"
echo "# Duration: $DURATION per backend          #"
echo "# Rate: $RATE req/s                       #"
echo "############################################"

# Function to test a backend
test_backend() {
    local name=$1
    local url=$2
    
    echo ""
    echo "========================================="
    echo "Testing: $name"
    echo "URL: $url"
    echo "========================================="
    
    # Wait for server
    for i in {1..30}; do
        if curl -sf "$url/health" > /dev/null 2>&1; then
            echo "Server ready!"
            break
        fi
        sleep 1
    done
    
    # Run test
    RESULT_FILE="$OUTPUT_DIR/${name}-$(date +%Y%m%d-%H%M%S).bin"
    
    # Use simple target with proper URL
    echo "GET $url/" | vegeta attack -duration="$DURATION" -rate="$RATE" -output="$RESULT_FILE"
    
    echo ""
    echo "--- $name Results ---"
    vegeta report "$RESULT_FILE"
    
    echo "$name: $(vegeta report "$RESULT_FILE" | grep -E 'Success|Latency mean|Throughput')" >> "$OUTPUT_DIR/comparison.txt"
}

# Test Tokio backend (requires server running on port 3000)
test_backend "tokio" "http://localhost:3000"

# Test Monoio backend (requires server running on port 3001)
test_backend "monoio" "http://localhost:3001"

# Final comparison
echo ""
echo "############################################"
echo "# Backend Comparison Summary               #"
echo "############################################"
cat "$OUTPUT_DIR/comparison.txt"
