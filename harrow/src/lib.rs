//! # Harrow
//!
//! A thin, macro-free HTTP framework over Hyper with built-in observability.

pub use harrow_core::handler;
pub use harrow_core::middleware::{Middleware, Next};
pub use harrow_core::path::PathPattern;
pub use harrow_core::request::{BodyError, Request};
pub use harrow_core::response::{IntoResponse, Response};
pub use harrow_core::route::{App, Group, Route, RouteMetadata, RouteTable};
pub use harrow_core::state::TypeMap;

pub use harrow_server::{serve, serve_with_shutdown};

#[cfg(feature = "timeout")]
pub use harrow_core::timeout::timeout_middleware;

#[cfg(feature = "o11y")]
pub mod o11y {
    pub use harrow_o11y::O11yConfig;
    pub use harrow_o11y::o11y_middleware::o11y_middleware;
}

#[cfg(feature = "o11y")]
mod o11y_ext {
    use std::sync::Arc;

    use harrow_o11y::O11yConfig;
    use harrow_o11y::o11y_middleware::o11y_middleware;

    use crate::App;

    /// Extension trait that wires `O11yConfig` into application state,
    /// initialises the ro11y telemetry subscriber, and registers the
    /// o11y middleware in one call.
    pub trait AppO11yExt {
        fn o11y(self, config: O11yConfig) -> Self;
    }

    /// Holds the ro11y `TelemetryGuard` so the OTLP exporter stays alive
    /// for the lifetime of the application.
    struct TelemetryGuardHolder(#[allow(dead_code)] ro11y::TelemetryGuard);

    impl AppO11yExt for App {
        fn o11y(self, config: O11yConfig) -> Self {
            let guard = ro11y::init(ro11y::TelemetryConfig {
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
            });

            self.state(Arc::new(TelemetryGuardHolder(guard)))
                .state(Arc::new(config))
                .middleware(o11y_middleware)
        }
    }
}

#[cfg(feature = "o11y")]
pub use o11y_ext::AppO11yExt;
