//! # Harrow
//!
//! A thin, macro-free HTTP framework over Hyper with opt-in observability.
//!
//! ## Server Backends (Required)
//!
//! Harrow requires you to explicitly select a server backend via Cargo features.
//! There is no default — you must pick exactly one of:
//!
//! - **`tokio`**: Traditional async/await with Tokio + Hyper.
//!   Use for cross-platform compatibility, development on macOS/Windows,
//!   or deployment in containers/Lambda where io_uring is unavailable.
//!
//! - **`monoio`**: High-performance io_uring backend with thread-per-core.
//!   Use for maximum throughput on Linux 6.1+ bare metal or EC2.
//!   Requires custom seccomp profile in containers.
//!
//! - **`meguri`**: Pure io_uring backend using Harrow's own io_uring library.
//!   Exposes advanced io_uring features (registered buffers, buffer rings,
//!   zero-copy send, multishot recv) that Monoio does not support.
//!   Linux only. No Tokio dependency.
//!
//! ### Feature Flag Selection
//!
//! ```toml
//! # Tokio backend (cross-platform)
//! harrow = { version = "0.9", features = ["tokio"] }
//!
//! # io_uring backend via Monoio (Linux 6.1+ only)
//! harrow = { version = "0.9", features = ["monoio"] }
//!
//! # io_uring backend via Meguri (Linux only, advanced features)
//! harrow = { version = "0.9", features = ["meguri"] }
//! ```

pub use harrow_core::client::{Client, TestResponse};
pub use harrow_core::handler;
pub use harrow_core::middleware::{Middleware, Next, map_request, map_response, unless, when};
pub use harrow_core::path::PathPattern;
pub use harrow_core::problem::ProblemDetail;
pub use harrow_core::request::{BodyError, Request};
pub use harrow_core::response::{IntoResponse, Response, ResponseBody};
pub use harrow_core::route::{App, Group, Route, RouteMetadata, RouteSummary, RouteTable};
pub use harrow_core::state::{MissingExtError, MissingStateError, TypeMap};

// Root-level re-exports for single-backend mode.
#[cfg(all(feature = "tokio", not(feature = "monoio"), not(feature = "meguri")))]
pub use harrow_server_tokio::{
    ServerConfig, serve, serve_multi_worker, serve_with_config, serve_with_shutdown,
};

#[cfg(all(feature = "monoio", not(feature = "tokio"), not(feature = "meguri")))]
pub use harrow_server_monoio::{
    ServerConfig, ServerHandle, run, run_with_config, serve, serve_with_config,
    serve_with_shutdown, start, start_with_config,
};

#[cfg(all(feature = "meguri", not(target_os = "linux")))]
compile_error!(
    "the `meguri` server backend requires Linux. io_uring is a Linux kernel \
     feature and is not available on macOS, Windows, or BSD. \
     Use `tokio` for cross-platform development instead."
);

#[cfg(all(
    feature = "meguri",
    not(feature = "tokio"),
    not(feature = "monoio"),
    target_os = "linux"
))]
pub use harrow_server_meguri::{ServerConfig, serve, serve_with_config};

#[cfg(not(any(feature = "tokio", feature = "monoio", feature = "meguri")))]
compile_error!(
    "harrow requires a server backend feature. \
     Enable exactly one of: `tokio` for cross-platform compatibility, \
     `monoio` for io_uring via Monoio (Linux 6.1+), \
     or `meguri` for pure io_uring (Linux). \
     Example: harrow = {{ version = \"0.9\", features = [\"tokio\"] }}"
);

/// Runtime-specific server APIs.
///
/// Use this module when you need to explicitly select a server backend
/// regardless of which feature flags are enabled.
pub mod runtime {
    /// Tokio-based server (Hyper + epoll/kqueue).
    ///
    /// Available when the `tokio` feature is enabled.
    #[cfg(feature = "tokio")]
    pub mod tokio {
        pub use harrow_server_tokio::{
            ServerConfig, serve, serve_with_config, serve_with_shutdown,
        };
    }

    /// Monoio-based server (io_uring + thread-per-core).
    ///
    /// Available when the `monoio` feature is enabled.
    /// Falls back to epoll on non-Linux platforms, but io_uring is required
    /// for the intended performance characteristics.
    ///
    /// # Requirements
    /// - Linux kernel 6.1+ for full io_uring support
    /// - Custom seccomp profile if running in containers
    #[cfg(feature = "monoio")]
    pub mod monoio {
        pub use harrow_server_monoio::kernel_check::{IoDriver, detect_io_driver};
        pub use harrow_server_monoio::{
            ServerConfig, ServerHandle, run, run_with_config, serve, serve_with_config,
            serve_with_shutdown, start, start_with_config,
        };
    }

    /// Meguri-based server (pure io_uring, no Tokio).
    ///
    /// Available when the `meguri` feature is enabled.
    ///
    /// This is Harrow's own io_uring implementation, exposing advanced
    /// features that Monoio does not support: registered buffers, buffer
    /// rings, zero-copy send, multishot recv, operation chaining.
    ///
    /// # Requirements
    /// - Linux kernel 5.6+ (basic io_uring)
    /// - Linux kernel 5.11+ for EXT_ARG timeout optimization
    /// - Linux kernel 6.0+ for SEND_ZC
    #[cfg(all(feature = "meguri", target_os = "linux"))]
    pub mod meguri {
        pub use harrow_server_meguri::{ServerConfig, serve, serve_with_config};
    }
}

#[cfg(feature = "ws")]
pub mod ws {
    pub use harrow_core::ws::{Message, Utf8Bytes, close_code};
    pub use harrow_server_tokio::ws::{WebSocket, WsConfig, upgrade, upgrade_with_config};
}

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
    HeaderKeyExtractor, KeyExtractor, RateLimitBackend, RateLimitHeaderStyle, RateLimitMiddleware,
    RateLimitOutcome, rate_limit_middleware,
};

#[cfg(feature = "session")]
pub use harrow_middleware::session::{
    SameSite, Session, SessionConfig, SessionMiddleware, SessionStore, session_middleware,
};

#[cfg(feature = "openapi")]
pub use harrow_core::openapi::OpenApiInfo;

#[cfg(feature = "openapi")]
mod openapi_ext {
    use bytes::Bytes;

    use harrow_core::openapi::OpenApiInfo;

    use crate::App;

    /// Extension trait that registers a `GET {path}/openapi.json` endpoint
    /// serving the pre-built OpenAPI spec as JSON.
    pub trait AppOpenApiExt {
        fn openapi(self, path: &str, info: OpenApiInfo) -> Self;
    }

    impl AppOpenApiExt for App {
        fn openapi(self, path: &str, info: OpenApiInfo) -> Self {
            let json: Bytes = self.route_table().to_openapi_json(&info).into();
            let endpoint = format!("{}/openapi.json", path.trim_end_matches('/'));

            self.get(&endpoint, move |_req| {
                let json = json.clone();
                async move {
                    crate::Response::text(json)
                        .header("content-type", "application/json")
                }
            })
        }
    }
}

#[cfg(feature = "openapi")]
pub use openapi_ext::AppOpenApiExt;

#[cfg(feature = "o11y")]
pub mod o11y {
    pub use harrow_middleware::o11y::o11y_middleware;
    pub use harrow_o11y::O11yConfig;
    pub use rolly_tokio::{
        InitError, TelemetryGuard, TelemetryConfig, LayerConfig, TelemetrySink, NullSink,
        build_layer,
    };

    /// Initialize the global tracing subscriber with rolly's OTLP exporter.
    ///
    /// Returns a [`TelemetryGuard`] that **must** be held for the lifetime of
    /// the application — dropping it flushes and shuts down the exporter.
    ///
    /// Call this **once**, early in `main`, before constructing the [`App`].
    /// Then use [`.o11y()`](super::AppO11yExt::o11y) to register the
    /// middleware and config without touching the subscriber.
    ///
    /// # Panics
    ///
    /// Panics if a global tracing subscriber is already set or the OTLP
    /// exporter cannot start. For the fallible version, use
    /// [`try_init_telemetry`].
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use harrow::o11y::{O11yConfig, init_telemetry};
    ///
    /// let config = O11yConfig::default().service_name("my-app");
    /// let _guard = init_telemetry(config.clone());
    ///
    /// let app = harrow::App::new().o11y(config);
    /// ```
    pub fn init_telemetry(config: O11yConfig) -> TelemetryGuard {
        rolly_tokio::init_global_once(config.into())
    }

    /// Fallible version of [`init_telemetry`].
    ///
    /// Returns [`InitError::SubscriberAlreadySet`] if a global subscriber is
    /// already installed, or [`InitError::Exporter`] if the OTLP exporter
    /// cannot start.
    pub fn try_init_telemetry(config: O11yConfig) -> Result<TelemetryGuard, InitError> {
        rolly_tokio::try_init_global(config.into())
    }
}

#[cfg(feature = "o11y")]
mod o11y_ext {
    use std::sync::Arc;

    use harrow_middleware::o11y::o11y_middleware;
    use harrow_o11y::O11yConfig;

    use crate::App;

    /// Extension trait that wires observability into a Harrow application.
    pub trait AppO11yExt {
        /// Full observability setup: initializes the global tracing subscriber
        /// via rolly **and** registers the o11y middleware + config.
        ///
        /// This is the one-liner for simple applications that don't manage
        /// their own tracing subscriber.
        ///
        /// If a global subscriber is already set, the rolly subscriber is
        /// skipped with a warning — the middleware is still registered.
        /// For apps that intentionally own their subscriber, prefer
        /// [`o11y_middleware`](Self::o11y_middleware) instead to avoid the
        /// warning.
        fn o11y(self, config: O11yConfig) -> Self;

        /// Register only the o11y middleware and `O11yConfig` state.
        ///
        /// Does **not** touch the global tracing subscriber. Use this when
        /// the application manages its own subscriber (e.g. via
        /// `tracing_subscriber::fmt().init()` or by composing
        /// [`harrow::o11y::build_layer`](super::o11y::build_layer) into a
        /// custom registry).
        fn o11y_middleware(self, config: O11yConfig) -> Self;
    }

    /// Holds the rolly `TelemetryGuard` so the OTLP exporter stays alive
    /// for the lifetime of the application.
    struct TelemetryGuardHolder(#[allow(dead_code)] rolly_tokio::TelemetryGuard);

    impl AppO11yExt for App {
        fn o11y(self, config: O11yConfig) -> Self {
            let app = match rolly_tokio::try_init_global(config.clone().into()) {
                Ok(guard) => self.state(Arc::new(TelemetryGuardHolder(guard))),
                Err(rolly_tokio::InitError::SubscriberAlreadySet(_)) => {
                    tracing::warn!(
                        "harrow .o11y(): global tracing subscriber already set, \
                         skipping rolly subscriber initialization. \
                         OTLP export will only work if the existing subscriber \
                         includes rolly's layer (see harrow::o11y::build_layer). \
                         Consider using .o11y_middleware() instead."
                    );
                    self
                }
                Err(e) => {
                    panic!("harrow .o11y(): failed to start telemetry exporter: {e}");
                }
            };

            app.o11y_middleware(config)
        }

        fn o11y_middleware(self, config: O11yConfig) -> Self {
            self.state(Arc::new(config))
                .middleware(o11y_middleware)
        }
    }
}

#[cfg(feature = "o11y")]
pub use o11y_ext::AppO11yExt;
