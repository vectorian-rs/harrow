# API Architecture

This folder documents Harrow's public API shape: request-first handlers,
explicit helper methods, and intentionally small abstractions.

- [Request Helpers](./request-helpers.md): explicit request-first handler model and helper methods for params, query, body, and state.
- [Explicit Extractors](./explicit-extractors.md): rationale for avoiding extractor-heavy handler signatures and keeping extraction explicit.

For product identity, see [What Harrow Is](../../overview/what-is-harrow.md).
