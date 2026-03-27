#!/bin/bash
# Run all Vegeta tests against Harrow server

set -e

SERVER_URL=${SERVER_URL:-"http://tokio-server:3000"}
DURATION=${DURATION:-"30s"}
RATE=${RATE:-"1000"}
OUTPUT_DIR=${OUTPUT_DIR:-"/results"}

# Wait for server to be ready
echo "Waiting for server at $SERVER_URL..."
for i in {1..30}; do
    if curl -sf "$SERVER_URL/health" > /dev/null 2>&1; then
        echo "Server is ready!"
        break
    fi
    echo "  attempt $i/30..."
    sleep 1
done

# Create output directory
mkdir -p "$OUTPUT_DIR"

echo ""
echo "############################################"
echo "# Harrow Load Test Suite                   #"
echo "# Duration: $DURATION                       #"
echo "# Base Rate: $RATE req/s                   #"
echo "############################################"
echo ""

# Function to run a test
run_test() {
    local name=$1
    local target=$2
    local rate=${3:-$RATE}
    
    echo ""
    echo "Test: $name"
    echo "----------------------------------------"
    
    RESULT_FILE="$OUTPUT_DIR/${name}-$(date +%Y%m%d-%H%M%S).bin"
    
    vegeta attack \
        -targets="$target" \
        -duration="$DURATION" \
        -rate="$rate" \
        -output="$RESULT_FILE"
    
    vegeta report "$RESULT_FILE"
}

# Test 1: Basic GET endpoints
run_test "basic-get" "/targets/basic-get.txt" "$RATE"

# Test 2: Path parameters
run_test "path-params" "/targets/path-params.txt" "$RATE"

# Test 3: JSON body handling (lower rate due to body parsing)
run_test "json-body" "/targets/json-body.txt" "500"

# Test 4: Mixed HTTP methods
run_test "mixed-methods" "/targets/mixed-methods.txt" "$RATE"

# Test 5: CPU intensive (low rate)
run_test "cpu-intensive" "/targets/cpu-intensive.txt" "100"

# Test 6: Error handling (404s)
run_test "404-errors" "/targets/404-errors.txt" "$RATE"

echo ""
echo "############################################"
echo "# All tests complete!                      #"
echo "# Results in: $OUTPUT_DIR"
echo "############################################"

# Generate summary
echo ""
echo "=== Test Summary ==="
for result in "$OUTPUT_DIR"/*.bin; do
    if [ -f "$result" ]; then
        echo ""
        echo "--- $(basename "$result") ---"
        vegeta report "$result" | grep -E "(Requests|Success|Latency|Throughput)" | head -6
    fi
done
