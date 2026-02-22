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
    pub use harrow_o11y::o11y_middleware::o11y_middleware;
    pub use harrow_o11y::request_id;
    pub use harrow_o11y::{record_request, O11yConfig};
}

#[cfg(feature = "o11y")]
mod o11y_ext {
    use std::sync::Arc;

    use harrow_o11y::o11y_middleware::o11y_middleware;
    use harrow_o11y::O11yConfig;

    use crate::App;

    /// Extension trait that wires `O11yConfig` into application state
    /// and registers the o11y middleware in one call.
    pub trait AppO11yExt {
        fn o11y(self, config: O11yConfig) -> Self;
    }

    impl AppO11yExt for App {
        fn o11y(self, config: O11yConfig) -> Self {
            self.state(Arc::new(config)).middleware(o11y_middleware)
        }
    }
}

#[cfg(feature = "o11y")]
pub use o11y_ext::AppO11yExt;
