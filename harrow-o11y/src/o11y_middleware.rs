use std::sync::Arc;
use std::time::Instant;

use tracing::Instrument;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

use crate::{request_id, O11yConfig};

/// Built-in observability middleware.
/// Adds a tracing span, request ID, and records latency + status metrics.
///
/// Reads `Arc<O11yConfig>` from application state. If not present, falls back
/// to `O11yConfig::default()`.
#[cfg_attr(feature = "profiling", inline(never))]
pub async fn o11y_middleware(mut req: Request, next: Next) -> Response {
    // Read config from state, fall back to defaults if not wired.
    let default_config = Arc::new(O11yConfig::default());
    let config = req
        .try_state::<Arc<O11yConfig>>()
        .cloned()
        .unwrap_or(default_config);

    // Extract owned locals before moving `req` into `next.run()`.
    let method = req.method().to_string();
    let path = req.path().to_string();
    let route = req.route_pattern().unwrap_or_else(|| req.path()).to_string();

    // Request ID handling.
    let request_id = if config.request_id_enabled {
        let id = req
            .header(&config.request_id_header)
            .map(|s| s.to_string())
            .unwrap_or_else(request_id::generate);
        req.set_request_id(id.clone());
        Some(id)
    } else {
        None
    };

    // Build the tracing span.
    let span = if config.tracing_enabled {
        tracing::info_span!(
            "http_request",
            method = %method,
            path = %path,
            route = %route,
            request_id = request_id.as_deref().unwrap_or("-"),
        )
    } else {
        tracing::Span::none()
    };

    let start = Instant::now();

    // Propagate span across the async boundary via `.instrument()`.
    let resp = next.run(req).instrument(span).await;

    let duration = start.elapsed();
    let status = resp.status_code().as_u16();

    if config.tracing_enabled {
        tracing::info!(
            method = %method,
            path = %path,
            route = %route,
            status = status,
            duration_ms = duration.as_secs_f64() * 1000.0,
            request_id = request_id.as_deref().unwrap_or("-"),
            "request completed"
        );
    }

    if config.metrics_enabled {
        crate::record_request(&route, &method, status, duration);
    }

    match request_id {
        Some(id) => resp.header(&config.request_id_header, &id),
        None => resp,
    }
}
