# Vegeta Load Testing - Quick Start

## Run All Tests (One Command)

```bash
# Terminal 1: Start the server
cd /Users/l1x/code/home/projectz/harrow
cargo run --example vegeta_target_tokio \
  --features tokio,timeout,request-id,cors,session,json

# Terminal 2: Run all tests
./infra/vegeta/scripts/run-all-vegeta-tests.sh
```

## Test Coverage (25 Tests)

### HTTP Methods (9 tests)
- GET, POST, PUT, PATCH, DELETE on `/echo`
- GET probes: `/health`, `/live`, `/ready`

### CRUD Operations (5 tests)
- POST `/users` - Create
- GET `/users/:id` - Read
- PUT `/users/:id` - Update
- PATCH `/users/:id` - Partial update
- DELETE `/users/:id` - Delete

### Path Parameters (2 tests)
- Simple: `/users/:id`
- Nested: `/users/:user_id/posts/:post_id`

### Middleware (5 tests)
- Request ID middleware
- CORS middleware
- Session: Get, Increment, Destroy

### Error Handling (2 tests)
- 404 Not Found
- 405 Method Not Allowed

### Performance (2 tests)
- CPU Intensive: `/cpu`
- Timeout Test: `/slow`

## Output

```
╔══════════════════════════════════════════════════════════════╗
║                      TEST SUMMARY                            ║
╚══════════════════════════════════════════════════════════════╝

Test Name                        Status      Latency   Throughput    Success
───────────────────────────────────────────────────────────────────────────────
GET Root                           PASS       312µs      3000 req/s    100%
GET Health                         PASS       298µs      3000 req/s    100%
POST Create User                   PASS       425µs       600 req/s    100%
...
Total: 25 | Pass: 25 | Fail: 0

Results saved to: ./results/20260327-210833
```

## Configuration

```bash
# Custom duration (default: 5s)
DURATION=10s ./scripts/run-all-vegeta-tests.sh

# Custom server URL
SERVER_URL=http://localhost:8080 ./scripts/run-all-vegeta-tests.sh

# Verbose output
VERBOSE=true ./scripts/run-all-vegeta-tests.sh
```
