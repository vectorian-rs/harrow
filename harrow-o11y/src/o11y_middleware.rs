use std::sync::Arc;
use std::time::Instant;

use ro11y::constants::fields;
use tracing::Instrument;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

use crate::O11yConfig;

/// Built-in observability middleware.
///
/// Creates a tracing span with a `trace_id` field that ro11y's OtlpLayer
/// picks up automatically for OTLP export. Generates request IDs via
/// `ro11y::trace_id`, records RED metric events, and echoes the request
/// ID header in the response.
///
/// Reads `Arc<O11yConfig>` from application state. If not present, falls back
/// to `O11yConfig::default()`.
pub async fn o11y_middleware(mut req: Request, next: Next) -> Response {
    let default_config = Arc::new(O11yConfig::default());
    let config = req
        .try_state::<Arc<O11yConfig>>()
        .cloned()
        .unwrap_or(default_config);

    let method = req.method().to_string();
    let path = req.path().to_string();
    let route = req
        .route_pattern()
        .unwrap_or_else(|| req.path())
        .to_string();

    // Extract request ID from header, or generate a new trace ID.
    let request_id = req
        .header(&config.request_id_header)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let id = ro11y::trace_id::generate_trace_id(None);
            ro11y::trace_id::hex_encode(&id)
        });

    req.set_request_id(request_id.clone());

    // Create span with trace_id field — OtlpLayer picks this up automatically.
    let span = tracing::info_span!(
        "http_request",
        { fields::TRACE_ID } = request_id.as_str(),
        { fields::HTTP_METHOD } = %method,
        { fields::HTTP_URI } = %path,
        route = %route,
        request_id = %request_id,
    );

    let start = Instant::now();
    let resp = next.run(req).instrument(span).await;
    let duration = start.elapsed();
    let status = resp.status_code().as_u16();

    tracing::info!(
        { fields::HTTP_METHOD } = %method,
        { fields::HTTP_URI } = %path,
        route = %route,
        { fields::HTTP_STATUS_CODE } = status,
        { fields::HTTP_LATENCY_MS } = duration.as_secs_f64() * 1000.0,
        request_id = %request_id,
        "request completed"
    );

    resp.header(&config.request_id_header, &request_id)
}
