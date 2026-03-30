# Issue: Expose More Hyper/Tokio-Level Server Configuration Options

## Summary
`ServerConfig` provides good defaults but could expose more Hyper/Tokio-level tuning options. Consider advanced configurations for HTTP/2, keep-alive, etc.

## Current State
From `harrow-server-tokio/src/lib.rs`:
```rust
pub struct ServerConfig {
    /// Maximum number of concurrent connections. Default: 8192.
    pub max_connections: usize,
    /// Timeout for reading HTTP headers from a new connection. Default: Some(5s).
    pub header_read_timeout: Option<Duration>,
    /// Maximum lifetime of a single connection. Default: Some(5 min).
    pub connection_timeout: Option<Duration>,
    /// Time to wait for in-flight requests to complete during shutdown. Default: 30s.
    pub drain_timeout: Duration,
}
```

## Concerns
1. **Limited HTTP/2 configuration**: No way to tune HTTP/2 specific settings
2. **No keep-alive tuning**: Keep-alive timeout and max requests not configurable
3. **Missing TCP socket options**: No way to set TCP_NODELAY, buffer sizes, etc.
4. **No TLS configuration helpers**: TLS setup is external to the server config

## Proposed Configuration Additions

```rust
pub struct ServerConfig {
    // Existing fields...
    
    // HTTP/2 specific
    pub http2: Http2Config,
    
    // Keep-alive settings
    pub keep_alive: KeepAliveConfig,
    
    // TCP socket options
    pub tcp: TcpConfig,
    
    // TLS configuration (optional)
    pub tls: Option<TlsConfig>,
}

pub struct Http2Config {
    /// Maximum number of concurrent streams per connection
    pub max_concurrent_streams: u32,
    /// Initial connection-level flow control window
    pub initial_connection_window_size: u32,
    /// Initial stream-level flow control window
    pub initial_stream_window_size: u32,
    /// Maximum frame size
    pub max_frame_size: u32,
    /// Enable HTTP/2 server push
    pub enable_push: bool,
}

pub struct KeepAliveConfig {
    /// Keep-alive timeout
    pub timeout: Duration,
    /// Maximum requests per keep-alive connection
    pub max_requests: usize,
}

pub struct TcpConfig {
    /// Enable TCP_NODELAY
    pub nodelay: bool,
    /// TCP send buffer size
    pub send_buffer_size: Option<usize>,
    /// TCP receive buffer size
    pub recv_buffer_size: Option<usize>,
}
```

## Hyper Builder Integration
The `serve_with_config` function uses:
```rust
let mut builder = hyper_util::server::conn::auto::Builder::new(
    hyper_util::rt::TokioExecutor::new(),
);
```

We should expose more of this builder's configuration options.

## Acceptance Criteria
- [ ] HTTP/2 configuration options exposed
- [ ] Keep-alive timeout and max requests configurable
- [ ] TCP socket options (NODELAY, buffer sizes)
- [ ] Backward compatible (all new fields optional with sensible defaults)
- [ ] Documentation for tuning recommendations
- [ ] Tests verifying configuration is applied

## Priority
Low-Medium - useful for production tuning, but defaults work for most cases

## Related Files
- `harrow-server-tokio/src/lib.rs`
- `docs/connection-safety.md`
- `docs/performance.md`

## Notes
This should maintain backward compatibility - all new configuration fields should be optional with sensible defaults.
