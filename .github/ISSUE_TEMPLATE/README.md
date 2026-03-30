# GitHub Issues for Harrow Framework

This directory contains templates for tracking improvements to the Harrow HTTP framework based on code review feedback.

## Validated Concerns (Create These Issues)

### 1. [Error Handling Consistency](01_error_handling_consistency.md)
**Status:** Valid - needs improvement

ProblemDetail exists but general error handling could be standardized with common error types and helper functions.

**Priority:** Medium

---

### 2. [Middleware Documentation](02_middleware_documentation.md)
**Status:** Valid - documentation gap

The `Next` type and middleware patterns need better documentation with comprehensive examples.

**Priority:** High

---

### 3. [Route Group Debugging](03_route_group_debugging.md)
**Status:** Valid - usability concern

Middleware composition in complex hierarchies needs debugging/introspection tools.

**Priority:** Medium

---

### 4. [Body Parsing Helpers](04_body_parsing_helpers.md)
**Status:** Valid with constraints

Optional body parsing helpers would improve ergonomics while maintaining "no extractors" philosophy.

**Priority:** Medium
**Note:** Must remain opt-in, not a generic extractor system

---

### 5. [Server Configuration Granularity](05_server_config_granularity.md)
**Status:** Valid - production need

ServerConfig needs more Hyper/Tokio-level tuning options (HTTP/2, keep-alive, TCP options).

**Priority:** Low-Medium

---

### 6. [Testing Coverage Improvements](06_testing_coverage_improvements.md)
**Status:** Valid - implementation gap

Verification strategy documented but fuzzing targets and proptest properties need implementation.

**Priority:** High

---

## Code Quality Observations

### Positive Aspects (No Issues Needed)
- ✅ Consistent coding style and patterns
- ✅ Good use of Rust's type system and ownership model
- ✅ Appropriate use of Arc for shared state
- ✅ Clear separation between sync and async code
- ✅ Good error propagation patterns

### Potential Concerns (Already Mitigated)
- ⚠️ Some unsafe blocks in Hyper integration - **Expected for FFI boundaries**
- ⚠️ Complex lifetime patterns in middleware chain - **Necessary for performance**
- ⚠️ Recursive middleware dispatch could theoretically stack overflow - **Mitigated by reasonable middleware counts**

## Creating Issues

To create these issues on GitHub:

```bash
# Using GitHub CLI
gh issue create --title "Standardize Error Handling Patterns Beyond ProblemDetail" \
  --body-file .github/ISSUE_TEMPLATE/01_error_handling_consistency.md \
  --label "enhancement,good first issue"

# Or manually copy the content from each .md file
```

## Priority Order for Implementation

1. **Testing Coverage** - Critical for production confidence
2. **Middleware Documentation** - Improves onboarding
3. **Error Handling** - Improves developer experience
4. **Body Parsing Helpers** - Reduces boilerplate
5. **Route Group Debugging** - Helps complex applications
6. **Server Configuration** - Production tuning (lower priority)

## Tracking

- [ ] 01_error_handling_consistency.md - Created
- [ ] 02_middleware_documentation.md - Created
- [ ] 03_route_group_debugging.md - Created
- [ ] 04_body_parsing_helpers.md - Created
- [ ] 05_server_config_granularity.md - Created
- [ ] 06_testing_coverage_improvements.md - Created
