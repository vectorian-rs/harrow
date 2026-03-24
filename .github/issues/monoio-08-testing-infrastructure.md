# [monoio] Add Comprehensive Testing Infrastructure

## Problem
The monoio server currently has basic tests but lacks:
- Property-based testing (proptest)
- Fuzzing for HTTP parser
- Comprehensive integration test coverage
- Load/stress testing

## Goals
Add production-grade testing infrastructure.

## Tasks

### 1. Proptest Integration
- [ ] Property-based tests for HTTP codec (`codec.rs`)
  - Request parsing invariants
  - Response serialization round-trips
  - Chunked encoding/decoding
- [ ] State machine tests for connection lifecycle

### 2. Fuzzing
- [ ] HTTP request parser fuzzer (`cargo-fuzz`)
  - Target: `codec::try_parse_request`
  - Focus: Malformed headers, edge cases, boundary conditions
- [ ] Integration with CI (corpus collection)

### 3. Integration Tests
- [ ] Concurrent connection stress test
- [ ] Slowloris attack resistance test
- [ ] Large body streaming test (>100MB)
- [ ] Keep-alive connection reuse test
- [ ] Error recovery test (invalid requests followed by valid)
- [ ] Cancellation safety stress test (rapid timeout/cancel cycles)

### 4. Load Testing
- [ ] `harrow-bench` integration for monoio
- [ ] Compare against tokio baseline
- [ ] Memory leak detection under sustained load

### 5. CI Integration
- [ ] Run fuzzer for 5 minutes on PR
- [ ] Property tests in CI
- [ ] Memory leak check with valgrind/miri (if possible)

## Dependencies to Add
```toml
[dev-dependencies]
proptest = "1"
libfuzzer-sys = { version = "0.4", optional = true }
```

## Acceptance Criteria
- [ ] `cargo test -p harrow-server-monoio` includes proptests
- [ ] `cargo fuzz` target exists and runs
- [ ] Integration tests cover edge cases
- [ ] CI runs all test types
- [ ] No regressions in coverage

## Priority
**Medium** - Quality assurance before production use.

## Labels
`testing`, `monoio`, `quality`
