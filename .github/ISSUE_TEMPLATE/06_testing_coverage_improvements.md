# Issue: Implement Proposed Verification Strategies and Expand Test Coverage

## Summary
While the verification strategy is solid (documented in `docs/verification.md`), actual implementation of some proposed tests (like fuzzing targets) should be verified. Integration tests could cover more edge cases in middleware interactions.

## Current State
- `docs/verification.md` outlines comprehensive testing strategy
- Unit tests exist throughout the codebase
- Property-based testing with `proptest` proposed but partially implemented
- Fuzzing targets (`cargo-fuzz`) proposed but need verification
- Kani bounded verification proposed for specific functions

## From `docs/verification.md`

### Areas needing implementation:

#### 1. Path Matching (path.rs) — proptest + fuzz
- [ ] `match_path` and `matches` agreement property
- [ ] Captured params round-trip
- [ ] Glob captures remainder
- [ ] No false positives on literals
- [ ] Trailing-slash symmetry

**Fuzz targets needed:**
- `fuzz_path_match(pattern: &[u8], path: &[u8])`

#### 2. Middleware Dispatch Chain (dispatch.rs) — proptest
- [ ] Handler called exactly once with N global + M route middleware
- [ ] Middleware execute in order
- [ ] Short-circuit behavior verified
- [ ] Fast path vs slow path equivalence

#### 3. Route Table (route.rs) — proptest
- [ ] Registered routes are found
- [ ] Wrong method returns correct indicators
- [ ] `allowed_methods` returns correct set
- [ ] HEAD→GET fallback

#### 4. Query Parsing (request.rs) — fuzz
- [ ] `fuzz_query_pairs` target
- [ ] Adversarial query string handling
- [ ] MAX_QUERY_PAIRS enforcement

#### 5. Rate Limiter GCRA (rate_limit.rs) — proptest + Kani
- [ ] Burst property
- [ ] Rate property
- [ ] Independence of keys
- [ ] Monotonicity of remaining

**Kani verification:**
- [ ] `gcra_check` single-step correctness
- [ ] `ns_to_secs_ceil` correctness

#### 6. Compression (compression.rs) — proptest
- [ ] Round-trip property
- [ ] Encoding negotiation preference order
- [ ] No double-compress

## Missing Integration Tests

### Middleware Interactions
- [ ] Timeout + rate limiting combination
- [ ] Compression with body limit
- [ ] Session with CORS preflight
- [ ] Multiple middleware modifying same request/response

### Edge Cases
- [ ] Empty body with compression
- [ ] Very large headers
- [ ] Unicode in path parameters
- [ ] Malformed request handling

## Acceptance Criteria
- [ ] All fuzz targets from verification.md implemented
- [ ] Proptest properties for path matching, middleware dispatch, routing
- [ ] Kani proofs for GCRA functions
- [ ] Middleware interaction integration tests
- [ ] Edge case test coverage expanded
- [ ] CI integration for fuzzing (nightly or scheduled)

## Priority
High - correctness critical for production use

## Related Files
- `docs/verification.md`
- `harrow-core/fuzz/` (create if not exists)
- `harrow-core/src/` (add proptest tests)
- `.github/workflows/` (CI integration)

## Implementation Notes

### Setting up fuzzing:
```bash
cargo install cargo-fuzz
cd harrow-core
cargo fuzz init
cargo fuzz add path_match
```

### Proptest structure:
```rust
#[cfg(test)]
mod proptests {
    use proptest::prelude::*;
    
    proptest! {
        #[test]
        fn match_path_and_matches_agree(
            pattern in "[a-z/:*]+",
            path in "[a-z/]+",
        ) {
            // Property test
        }
    }
}
```

### Kani setup:
```rust
#[cfg(kani)]
mod verification {
    #[kani::proof]
    fn gcra_check_correctness() {
        // Bounded verification
    }
}
```
