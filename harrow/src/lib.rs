//! # Harrow
//!
//! A thin, macro-free HTTP framework over Hyper with built-in observability.

pub use harrow_core::client::{Client, TestResponse};
pub use harrow_core::handler;
pub use harrow_core::middleware::{Middleware, Next};
pub use harrow_core::path::PathPattern;
pub use harrow_core::request::{BodyError, Request};
pub use harrow_core::response::{IntoResponse, Response, ResponseBody};
pub use harrow_core::route::{App, Group, Route, RouteMetadata, RouteTable};
pub use harrow_core::state::{MissingStateError, TypeMap};

pub use harrow_server::{ServerConfig, serve, serve_with_config, serve_with_shutdown};

#[cfg(feature = "timeout")]
pub use harrow_middleware::timeout::timeout_middleware;

#[cfg(feature = "request-id")]
pub use harrow_middleware::request_id::{request_id_middleware, request_id_middleware_with_header};

#[cfg(feature = "cors")]
pub use harrow_middleware::cors::{CorsConfig, cors_middleware};

#[cfg(feature = "body-limit")]
pub use harrow_middleware::body_limit::body_limit_middleware;

#[cfg(feature = "catch-panic")]
pub use harrow_middleware::catch_panic::catch_panic_middleware;

#[cfg(feature = "compression")]
pub use harrow_middleware::compression::compression_middleware;

#[cfg(feature = "rate-limit")]
pub use harrow_middleware::rate_limit::{
    HeaderKeyExtractor, InMemoryBackend, KeyExtractor, RateLimitBackend, RateLimitHeaderStyle,
    RateLimitMiddleware, RateLimitOutcome, rate_limit_middleware,
};

#[cfg(feature = "o11y")]
pub mod o11y {
    pub use harrow_middleware::o11y::o11y_middleware;
    pub use harrow_o11y::O11yConfig;
}

#[cfg(feature = "o11y")]
mod o11y_ext {
    use std::sync::Arc;

    use harrow_middleware::o11y::o11y_middleware;
    use harrow_o11y::O11yConfig;

    use crate::App;

    /// Extension trait that wires `O11yConfig` into application state,
    /// initialises the rolly telemetry subscriber, and registers the
    /// o11y middleware in one call.
    pub trait AppO11yExt {
        fn o11y(self, config: O11yConfig) -> Self;
    }

    /// Holds the rolly `TelemetryGuard` so the OTLP exporter stays alive
    /// for the lifetime of the application.
    struct TelemetryGuardHolder(#[allow(dead_code)] rolly::TelemetryGuard);

    impl AppO11yExt for App {
        fn o11y(self, config: O11yConfig) -> Self {
            let guard = rolly::init(rolly::TelemetryConfig {
                service_name: config.service_name.clone(),
                service_version: config.service_version.clone(),
                environment: config.environment.clone(),
                otlp_traces_endpoint: config.otlp_traces_endpoint.clone(),
                otlp_logs_endpoint: config.otlp_logs_endpoint.clone(),
                otlp_metrics_endpoint: config.otlp_metrics_endpoint.clone(),
                log_to_stderr: true,
                use_metrics_interval: None,
                metrics_flush_interval: None,
                sampling_rate: None,
                backpressure_strategy: rolly::BackpressureStrategy::Drop,
            });

            self.state(Arc::new(TelemetryGuardHolder(guard)))
                .state(Arc::new(config))
                .middleware(o11y_middleware)
        }
    }
}

#[cfg(feature = "o11y")]
pub use o11y_ext::AppO11yExt;
