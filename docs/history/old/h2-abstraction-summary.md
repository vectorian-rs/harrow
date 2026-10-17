# Unified H1/H2 Abstraction Layer - Summary

## Design Overview

The abstraction layer consists of three new modules:

### 1. `protocol.rs` - Core Abstractions
**Purpose**: Define the interface between protocol implementations and the server.

**Key Types**:
- `ProtocolVersion` - Enum for H1/H2 detection result
- `ProtocolConnection` trait - Common interface for both protocols
- `ProtocolConfig` - Shared configuration struct
- `body_from_bytes()` - Utility for creating Harrow bodies
- `ProtocolError` - Unified error types

### 2. `h1.rs` - HTTP/1.1 Implementation  
**Purpose**: Refactored H1 handling (moved from `connection.rs`).

**Key Types**:
- `H1Connection` - Sequential request-response handler
- `H1ResponseSender` - Response handle for H1

**Features**:
- Keep-alive connections
- Content-Length and chunked encoding
- Cancellation-safe I/O (using `Canceller`)
- Buffer pooling integration

### 3. `h2.rs` - HTTP/2 Implementation
**Purpose**: New H2 support using `monoio-http`.

**Key Types**:
- `H2Connection` - Multiplexed stream handler
- `H2ResponseSender` - Response handle for H2
- `H2Builder` - Configuration builder

**Features**:
- Concurrent streams (100 default)
- Flow control (connection + stream level)
- Server push capability (optional)
- Prior knowledge support

## Module Structure

```
harrow-server-monoio/src/
├── lib.rs              # Updated to use new modules
├── protocol.rs         # NEW: Abstraction layer
├── h1.rs               # NEW: H1 implementation (refactored from connection.rs)
├── h2.rs               # NEW: H2 implementation
├── connection.rs       # MODIFIED: Now dispatches to h1.rs or h2.rs
├── buffer.rs           # (unchanged) Buffer pool
├── cancel.rs           # (unchanged) Cancellation safety
├── codec.rs            # (unchanged) H1 codec
├── kernel_check.rs     # (unchanged) Kernel version check
└── o11y.rs             # (unchanged) Observability
```

## Connection Flow

```
1. Server accepts TCP connection
   ↓
2. Protocol detection (peek first 24 bytes)
   ↓
3. Create appropriate handler:
   - H2 preface detected → H2Connection
   - Otherwise → H1Connection  
   ↓
4. Run connection handler:
   
   H1Connection::run():
   - Sequential loop
   - Read request → Dispatch → Send response
   - Repeat until keep-alive ends
   
   H2Connection::run():
   - Handshake (H2 preface + SETTINGS)
   - Accept streams loop
   - Spawn task per stream
   - Each stream: Read → Dispatch → Send response
   ↓
5. Connection closes, metrics recorded
```

## Key Design Decisions

### 1. Protocol Detection Strategy
```rust
// HTTP/2 has a mandatory 24-byte preface
if first_bytes == b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" {
    ProtocolVersion::Http2
} else {
    ProtocolVersion::Http11
}
```

**Why**: Simple, reliable, no config needed for "prior knowledge" mode.

### 2. Body Handling

**Current** (both H1 and H2):
- Collect entire body into `Bytes`
- Wrap with `Full::new(bytes).boxed()`
- Pass to Harrow dispatch

**Future** (streaming):
- Create `StreamingBody<B>` adapter
- Implements `http_body::Body` trait
- Streams chunks as they arrive

### 3. Concurrency Model

**H1**: Sequential - one request at a time per connection
```rust
loop {
    let req = read_request().await?;
    let resp = dispatch(req).await;
    send_response(resp).await?;
}
```

**H2**: Concurrent - spawn each stream
```rust
loop {
    let (req, respond) = connection.accept().await?;
    monoio::spawn(handle_stream(req, respond));
}
```

### 4. Cancellation Safety

Both protocols use the same pattern:

```rust
let canceller = Canceller::new();
let handle = canceller.handle();

monoio::select! {
    r = stream.cancelable_read(buf, handle) => r,
    _ = timeout => {
        canceller.cancel();
        let (_, buf) = read_fut.await; // Reclaim buffer!
        return Err(Timeout);
    }
}
```

## API Changes

### Public API
**No changes** - `serve()`, `serve_with_shutdown()`, `serve_with_config()` remain the same.

Users automatically get H2 support when clients use it.

### Configuration Additions (Future)
```rust
pub struct ServerConfig {
    // Existing fields...
    
    // New H2-specific options
    pub h2_max_concurrent_streams: u32,      // Default: 100
    pub h2_initial_window_size: u32,          // Default: 64KB
    pub h2_connection_window_size: u32,       // Default: 1MB
    pub h2_enable_push: bool,                 // Default: false
    pub h2_max_frame_size: u32,               // Default: 16KB
}
```

## Testing Strategy

1. **Unit Tests** (per module)
   - `protocol.rs`: Protocol detection, error types
   - `h1.rs`: Request parsing, response encoding
   - `h2.rs`: Stream handling, flow control

2. **Integration Tests**
   - H1 client → H1 server
   - H2 client → H2 server  
   - Mixed (H1 client → H2 server via ALPN - future)

3. **Stress Tests**
   - Many concurrent H2 streams
   - Large bodies with flow control
   - Connection churn

## Dependencies

Add to `Cargo.toml`:
```toml
[dependencies]
monoio-http = "0.3.12"
```

This brings in:
- `monoio-http` - H2 implementation
- `monoio-codec` - Framed codecs (shared)
- `monoio-compat` - Compatibility utilities

## Migration Path

### Phase 1: Refactor H1 (No behavior change)
1. Create `protocol.rs` with basic types
2. Create `h1.rs` with refactored H1 code
3. Update `connection.rs` to call into `h1.rs`
4. **Test**: All existing tests pass

### Phase 2: Add H2
1. Add `monoio-http` dependency
2. Implement `h2.rs`
3. Add protocol detection
4. **Test**: New H2 tests pass

### Phase 3: Polish
1. Add configuration options
2. Add streaming body support
3. Performance tuning
4. **Test**: Full test suite

## Open Questions for Review

1. **Should we support H2C (cleartext H2) without prior knowledge?**
   - HTTP/1.1 upgrade mechanism
   - More complex, less commonly used

2. **Should we expose H2-specific features to handlers?**
   - Server push
   - Stream priorities
   - Flow control windows

3. **How do we handle H2-specific errors?**
   - PROTOCOL_ERROR
   - FLOW_CONTROL_ERROR
   - STREAM_CLOSED

4. **Should metrics distinguish H1 vs H2?**
   - Per-protocol connection counts
   - Per-protocol request rates
