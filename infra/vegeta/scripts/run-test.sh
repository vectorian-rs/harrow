#!/bin/bash
# Run a single Vegeta test
# Usage: ./run-test.sh <target-file> <duration> <rate>

set -e

TARGET_FILE=${1:-"/targets/basic-get.txt"}
DURATION=${2:-"30s"}
RATE=${3:-"1000"}
OUTPUT_DIR=${4:-"/results"}

# Extract test name from filename
TEST_NAME=$(basename "$TARGET_FILE" .txt)
RESULT_FILE="$OUTPUT_DIR/${TEST_NAME}-$(date +%Y%m%d-%H%M%S).bin"
REPORT_FILE="$OUTPUT_DIR/${TEST_NAME}-report.txt"

echo "========================================="
echo "Running Vegeta Test: $TEST_NAME"
echo "Duration: $DURATION"
echo "Rate: $RATE req/s"
echo "Target: $TARGET_FILE"
echo "========================================="

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

# Run vegeta attack
vegeta attack \
    -targets="$TARGET_FILE" \
    -duration="$DURATION" \
    -rate="$RATE" \
    -output="$RESULT_FILE"

# Generate report
echo ""
echo "Test complete. Generating report..."
echo ""

echo "=== Latency Report ===" > "$REPORT_FILE"
vegeta report -type=latency "$RESULT_FILE" >> "$REPORT_FILE"

echo "" >> "$REPORT_FILE"
echo "=== Histogram ===" >> "$REPORT_FILE"
vegeta report -type=histogram "$RESULT_FILE" >> "$REPORT_FILE"

echo "" >> "$REPORT_FILE"
echo "=== Text Report ===" >> "$REPORT_FILE"
vegeta report "$RESULT_FILE" >> "$REPORT_FILE"

# Print to stdout
cat "$REPORT_FILE"

echo ""
echo "Results saved to:"
echo "  Binary: $RESULT_FILE"
echo "  Report: $REPORT_FILE"
