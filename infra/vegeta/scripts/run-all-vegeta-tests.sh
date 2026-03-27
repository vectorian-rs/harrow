#!/bin/bash
# Comprehensive Vegeta test suite for Harrow
# Runs all tests and generates a summary report

set -e

# Configuration
SERVER_URL="${SERVER_URL:-http://localhost:3000}"
DURATION="${DURATION:-5s}"
OUTPUT_DIR="${OUTPUT_DIR:-./results/$(date +%Y%m%d-%H%M%S)}"
VERBOSE="${VERBOSE:-false}"

# Determine target directory based on SERVER_URL
if [[ "$SERVER_URL" == *"tokio-server"* ]] || [[ "$SERVER_URL" == *"monoio-server"* ]]; then
    TARGET_DIR="${TARGET_DIR:-./targets/docker}"
else
    TARGET_DIR="${TARGET_DIR:-./targets/local}"
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Test results storage
declare -a TEST_NAMES
declare -a TEST_RESULTS
declare -a TEST_LATENCIES
declare -a TEST_THROUGHPUTS
declare -a TEST_SUCCESS_RATES

mkdir -p "$OUTPUT_DIR"

echo -e "${BLUE}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║          Harrow Load Testing Suite - Vegeta                 ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo "Server: $SERVER_URL"
echo "Duration per test: $DURATION"
echo "Target directory: $TARGET_DIR"
echo "Results: $OUTPUT_DIR"
echo ""

# Wait for server
echo -n "Waiting for server..."
for i in {1..30}; do
    if curl -sf "$SERVER_URL/health" > /dev/null 2>&1; then
        echo -e " ${GREEN}✓ Ready${NC}"
        break
    fi
    echo -n "."
    sleep 1
    if [ $i -eq 30 ]; then
        echo -e " ${RED}✗ Timeout${NC}"
        exit 1
    fi
done
echo ""

# Function to run a test with proper command building
run_test() {
    local name="$1"
    local method="$2"
    local endpoint="$3"
    local rate="${4:-1000}"
    local body_file="${5:-}"
    local headers="${6:-}"
    
    TEST_NAMES+=("$name")
    
    echo -e "${YELLOW}▶ $name${NC}"
    echo "  Method: $method, Endpoint: $endpoint, Rate: $rate req/s"
    
    local result_file="$OUTPUT_DIR/$(echo "$name" | tr ' ' '_' | tr '[:upper:]' '[:lower:]').bin"
    local json_report="$OUTPUT_DIR/$(echo "$name" | tr ' ' '_' | tr '[:upper:]' '[:lower:]').json"
    
    # Build command as array (safer than string eval)
    local cmd_args=(-duration="$DURATION" -rate="$rate" -output="$result_file")
    
    if [ -n "$body_file" ] && [ -f "$body_file" ]; then
        cmd_args+=(-body="$body_file" -header="Content-Type: application/json")
    fi
    
    if [ -n "$headers" ]; then
        cmd_args+=(-header="$headers")
    fi
    
    if echo "$method $SERVER_URL$endpoint" | vegeta attack "${cmd_args[@]}" 2>/dev/null; then
        # Generate JSON report for reliable parsing
        vegeta report -type=json "$result_file" > "$json_report" 2>/dev/null || true
        
        # Extract metrics using jq if available, fallback to text parsing
        local latency="N/A"
        local throughput="N/A"
        local success="N/A"
        
        if command -v jq &> /dev/null && [ -f "$json_report" ]; then
            latency=$(jq -r '.latencies.mean // "N/A"' "$json_report" 2>/dev/null)
            throughput=$(jq -r '.throughput // "N/A"' "$json_report" 2>/dev/null)
            success=$(jq -r '.success // "N/A"' "$json_report" 2>/dev/null)
        else
            # Fallback to text parsing
            local report=$(vegeta report "$result_file" 2>/dev/null || true)
            latency=$(echo "$report" | grep -oE 'mean[^,]+' | head -1 | cut -d' ' -f2 || echo "N/A")
            throughput=$(echo "$report" | grep -oE 'throughput[^,]+' | head -1 | cut -d' ' -f2 || echo "N/A")
            success=$(echo "$report" | grep -oE 'Success[^%]+' | head -1 | cut -d' ' -f2 || echo "N/A")
        fi
        
        TEST_RESULTS+=("PASS")
        TEST_LATENCIES+=("$latency")
        TEST_THROUGHPUTS+=("$throughput")
        TEST_SUCCESS_RATES+=("$success")
        
        echo -e "  ${GREEN}✓ PASS${NC} - Latency: ${latency}, Throughput: ${throughput} req/s, Success: ${success}"
    else
        TEST_RESULTS+=("FAIL")
        TEST_LATENCIES+=("N/A")
        TEST_THROUGHPUTS+=("0")
        TEST_SUCCESS_RATES+=("0%")
        echo -e "  ${RED}✗ FAIL${NC}"
    fi
    echo ""
}

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  HTTP METHODS TESTS${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
run_test "GET Root" "GET" "/" 1000
run_test "GET Health" "GET" "/health" 1000
run_test "GET Liveness" "GET" "/live" 1000
run_test "GET Readiness" "GET" "/ready" 1000

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  CRUD OPERATIONS${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo '{"name":"Test User","email":"test@example.com"}' > /tmp/create_user.json
echo '{"name":"Updated User"}' > /tmp/update_user.json
echo '{"name":"Patched Name"}' > /tmp/patch_user.json

run_test "POST Create User" "POST" "/users" 200 /tmp/create_user.json
run_test "GET User" "GET" "/users/123" 500
run_test "PUT Update User" "PUT" "/users/123" 200 /tmp/update_user.json
run_test "PATCH User" "PATCH" "/users/123" 200 /tmp/patch_user.json
run_test "DELETE User" "DELETE" "/users/123" 200

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  PATH PARAMETERS${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
run_test "Simple Path Param" "GET" "/users/abc123" 500
run_test "Nested Path Params" "GET" "/users/abc/posts/456" 500

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  ECHO (All Methods)${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo '{"test":"data"}' > /tmp/echo_body.json
run_test "GET Echo" "GET" "/echo" 300
run_test "POST Echo" "POST" "/echo" 200 /tmp/echo_body.json
run_test "PUT Echo" "PUT" "/echo" 200 /tmp/echo_body.json
run_test "PATCH Echo" "PATCH" "/echo" 200 /tmp/echo_body.json
run_test "DELETE Echo" "DELETE" "/echo" 300

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  MIDDLEWARE TESTS${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
run_test "Request ID Middleware" "GET" "/middleware-test" 500
run_test "CORS Preflight" "OPTIONS" "/" 200 "" "Origin: http://example.com"

# Note: Session tests under Vegeta don't maintain cookies between requests,
# so each request creates a fresh session. This tests middleware overhead
# but not actual session persistence.
run_test "Session Get" "GET" "/session" 300
run_test "Session Increment" "POST" "/session/increment" 200
run_test "Session Destroy" "DELETE" "/session" 200

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  ERROR HANDLING${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
run_test "404 Not Found" "GET" "/nonexistent" 500
run_test "405 Method Not Allowed" "POST" "/health" 300

echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BLUE}  PERFORMANCE TESTS${NC}"
echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
run_test "CPU Intensive" "GET" "/cpu" 100
run_test "Timeout Test" "GET" "/slow" 20

# Generate Summary Report
echo ""
echo -e "${BLUE}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║                      TEST SUMMARY                            ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
printf "%-30s %8s %15s %12s %12s\n" "Test Name" "Status" "Latency" "Throughput" "Success"
echo "────────────────────────────────────────────────────────────────────────────────"

for i in "${!TEST_NAMES[@]}"; do
    status_color="$GREEN"
    if [ "${TEST_RESULTS[$i]}" = "FAIL" ]; then
        status_color="$RED"
    fi
    printf "%-30s ${status_color}%8s${NC} %15s %12s %12s\n" \
        "${TEST_NAMES[$i]:0:30}" \
        "${TEST_RESULTS[$i]}" \
        "${TEST_LATENCIES[$i]}" \
        "${TEST_THROUGHPUTS[$i]}" \
        "${TEST_SUCCESS_RATES[$i]}"
done

echo ""
echo "────────────────────────────────────────────────────────────────────────────────"

# Count results
PASS_COUNT=$(printf '%s\n' "${TEST_RESULTS[@]}" | grep -c "PASS" || true)
FAIL_COUNT=$(printf '%s\n' "${TEST_RESULTS[@]}" | grep -c "FAIL" || true)
TOTAL_COUNT=${#TEST_NAMES[@]}

echo ""
echo -e "Total: $TOTAL_COUNT | ${GREEN}Pass: $PASS_COUNT${NC} | ${RED}Fail: $FAIL_COUNT${NC}"
echo ""
echo "Results saved to: $OUTPUT_DIR"
echo ""

# Generate detailed report file
REPORT_FILE="$OUTPUT_DIR/summary-report.txt"
echo "Harrow Vegeta Test Report" > "$REPORT_FILE"
echo "Generated: $(date)" >> "$REPORT_FILE"
echo "Server: $SERVER_URL" >> "$REPORT_FILE"
echo "Duration per test: $DURATION" >> "$REPORT_FILE"
echo "" >> "$REPORT_FILE"
echo "Test Results:" >> "$REPORT_FILE"
echo "─────────────" >> "$REPORT_FILE"

for i in "${!TEST_NAMES[@]}"; do
    echo "" >> "$REPORT_FILE"
    echo "Test: ${TEST_NAMES[$i]}" >> "$REPORT_FILE"
    echo "  Status: ${TEST_RESULTS[$i]}" >> "$REPORT_FILE"
    echo "  Latency: ${TEST_LATENCIES[$i]}" >> "$REPORT_FILE"
    echo "  Throughput: ${TEST_THROUGHPUTS[$i]}" >> "$REPORT_FILE"
    echo "  Success Rate: ${TEST_SUCCESS_RATES[$i]}" >> "$REPORT_FILE"
done

echo "Detailed report: $REPORT_FILE"

# Exit with error if any test failed
if [ $FAIL_COUNT -gt 0 ]; then
    exit 1
fi
