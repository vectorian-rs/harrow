# HTTP/2 Support Design Document

## Overview

This document describes the unified HTTP/1.1 and HTTP/2 abstraction layer for `harrow-server-monoio`.

## Goals

1. **Transparent Protocol Handling**: Applications shouldn't need to know if the client is using H1 or H2
2. **Shared Code**: Common request handling (routing, middleware, serialization) works for both protocols
3. **Zero-Copy Where Possible**: Leverage monoio's io_uring for efficient I/O
4. **Cancellation Safety**: All operations must be cancellation-safe (critical for io_uring)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Server Layer                                 │
│  ┌─────────────┐  ┌─────────────────┐  ┌─────────────────────────┐ │
│  │ TCP Accept  │  │ Protocol Detect │  │ Connection Handler      │ │
│  └──────┬──────┘  └────────┬────────┘  └────────────┬────────────┘ │
└─────────┼──────────────────┼───────────────────────┼──────────────┘
          │                  │                       │
          ▼                  ▼                       ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     Protocol Abstraction Layer                       │
│                                                                      │
│  ┌──────────────────────────┐      ┌──────────────────────────┐    │
│  │   H1Connection           │      │   H2Connection           │    │
│  │   (connection.rs → h1.rs)│      │   (monoio-http h2)       │    │
│  │                          │      │                          │    │
│  │  - Sequential requests   │      │  - Multiplexed streams   │    │
│  │  - Keep-alive            │      │  - Flow control          │    │
│  │  - Chunked encoding      │      │  - Server push (opt)     │    │
│  └──────────┬───────────────┘      └──────────┬───────────────┘    │
│             │                                  │                    │
│             └──────────────┬───────────────────┘                    │
│                            │                                        │
│                            ▼                                        │
│              ┌─────────────────────────┐                            │
│              │  Unified Request/Body   │                            │
│              │  (harrow_core::Request) │                            │
│              └───────────┬─────────────┘                            │
└──────────────────────────┼──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      Harrow Core Layer                               │
│                                                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐ │
│  │   Router    │  │ Middleware  │  │  Handler    │  │  Response   │ │
│  │             │  │   Stack     │  │             │  │   Builder   │ │
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘ │
└─────────────────────────────────────────────────────────────────────┘
```

## Key Abstractions

### 1. Protocol Detection

At connection establishment, we need to detect whether the client is speaking H1 or H2:

```rust
/// HTTP/2 connection preface (24 bytes)
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

async fn detect_protocol(stream: &TcpStream) -> ProtocolVersion {
    // Read first 24 bytes
    let mut buf = [0u8; 24];
    let (n, _) = stream.peek(&mut buf).await; // Or read + buffer
    
    if &buf[..n] == H2_PREFACE {
        ProtocolVersion::Http2
    } else {
        ProtocolVersion::Http11
    }
}
```

### 2. Connection Handler Trait

Both H1 and H2 implement a common trait:

```rust
pub(crate) trait ProtocolConnection: 'static {
    fn run(self) -> impl Future<Output = Result<(), Error>> + 'static;
}
```

### 3. Request/Response Bridge

Harrow uses `http::Request<Body>` where `Body = BoxBody<Bytes, Error>`. Both protocols must convert to/from this type:

**HTTP/1.1**:
- Read body into `Bytes` (already does this)
- Wrap with `Full::new(bytes).boxed()`

**HTTP/2**:
- Stream body from DATA frames
- Collect into `Bytes` (for now) or stream via `StreamBody` adapter
- Flow control: release capacity as we read

## HTTP/2 Specifics

### Stream Handling

Each HTTP/2 stream is an independent request-response:

```rust
async fn handle_stream(
    request: http::Request<RecvStream>,
    respond: SendResponse,
) -> Result<()> {
    // 1. Read body with flow control
    let body = read_body(&mut request).await?;
    
    // 2. Convert to Harrow request
    let req = convert_request(request, body)?;
    
    // 3. Dispatch through Harrow
    let response = dispatch(shared, req).await;
    
    // 4. Send response
    send_response(&mut respond, response).await?;
    
    Ok(())
}
```

### Flow Control

HTTP/2 has connection-level and stream-level flow control windows:

```rust
// As we receive DATA frames, we must release capacity
while let Some(data) = body.data().await {
    let data = data?;
    let len = data.len();
    
    // Process data...
    
    // Release flow control
    body.flow_control().release_capacity(len)?;
}
```

### Concurrency

HTTP/2 allows many concurrent streams. The handler spawns each stream:

```rust
while let Some((request, respond)) = connection.accept().await {
    monoio::spawn(handle_stream(request, respond, shared.clone()));
}
```

## Refactoring Plan

### Phase 1: Extract H1 Module
1. Move H1-specific code from `connection.rs` to `h1.rs`
2. Keep shared types in `protocol.rs`
3. Update `lib.rs` to use new structure

### Phase 2: Add H2 Module  
1. Add `monoio-http` dependency
2. Implement `H2Connection` using `monoio_http::h2`
3. Add protocol detection in accept loop

### Phase 3: Unified Body Handling
1. Create `StreamingBody` adapter for H2
2. Support true streaming (not just collect-to-bytes)
3. Integrate with Harrow's response body types

### Phase 4: Testing
1. H2 client tests (using `monoio_http::h2::client`)
2. Protocol negotiation tests
3. Flow control stress tests
4. Concurrent stream tests

## Open Questions

### 1. Server Push
Should Harrow expose HTTP/2 server push? If so, how?

Options:
- A. Don't support it (simplest)
- B. Add `Response::push_promise()` method (H2 only, fails on H1)
- C. Transparent push via Link headers (complicated)

**Recommendation**: Start with Option A. Add later if needed.

### 2. Body Streaming
Currently Harrow collects the entire body before dispatch. For H2 with large streams or gRPC, we might want true streaming.

Options:
- A. Keep current behavior (collect to Bytes)
- B. Add streaming body adapter
- C. Add gRPC-specific handling

**Recommendation**: Start with A, add streaming later.

### 3. Connection Metrics
H2 has many streams per connection. How do we track metrics?

Options:
- A. Connection-level only (current)
- B. Per-stream metrics (expensive)
- C. Both with aggregation

**Recommendation**: A for now, add stream-level later if needed.

## Implementation Status

| Component | Status | Notes |
|-----------|--------|-------|
| `protocol.rs` | ✅ Design complete | Abstraction layer defined |
| `h1.rs` | 📝 Skeleton | Needs implementation moved from `connection.rs` |
| `h2.rs` | 📝 Skeleton | Needs `monoio-http` integration |
| Protocol detection | 📝 Design | Needs implementation |
| Tests | ❌ Not started | Add after implementation |

## Dependencies

Add to `Cargo.toml`:

```toml
[dependencies]
monoio-http = "0.3.12"
```

No additional dependencies needed - `monoio-http` re-uses the same `http`, `bytes`, etc. versions.
