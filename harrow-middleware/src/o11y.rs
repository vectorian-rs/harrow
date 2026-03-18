use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use rolly::constants::fields;
use tracing::Instrument;

use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

use harrow_o11y::O11yConfig;

// --- Fast request-ID generation (atomic counter, no RNG) ----------------

/// URL-safe alphabet (64 characters = 6 bits per character).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Hex digits for trace ID encoding.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Global monotonic counter — one relaxed atomic increment per request.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default config — allocated once, reused forever.
static DEFAULT_CONFIG: LazyLock<Arc<O11yConfig>> =
    LazyLock::new(|| Arc::new(O11yConfig::default()));

/// Generate a unique 11-character URL-safe request ID.
///
/// Atomic counter + base64 encoding.
/// No RNG, no syscalls — one relaxed atomic fetch-add and bit ops.
#[inline]
fn generate_request_id() -> String {
    let n = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut buf = [0u8; 11];
    let mut i = 0;
    while i < 11 {
        buf[i] = ALPHABET[((n >> (i * 6)) & 0x3F) as usize];
        i += 1;
    }
    // SAFETY: every byte comes from ALPHABET which is pure ASCII.
    unsafe { String::from_utf8_unchecked(buf.to_vec()) }
}

/// Derive a W3C-compliant 128-bit trace ID from a request ID using blake3.
///
/// blake3 XOF → 16 bytes → 32-char hex string.
/// Deterministic: same request ID always produces the same trace ID.
#[inline]
fn derive_trace_id(request_id: &str) -> String {
    let mut trace_bytes = [0u8; 16];
    let mut hasher = blake3::Hasher::new();
    hasher.update(request_id.as_bytes());
    hasher.finalize_xof().fill(&mut trace_bytes);

    let mut hex_buf = [0u8; 32];
    for (i, &b) in trace_bytes.iter().enumerate() {
        hex_buf[i * 2] = HEX[(b >> 4) as usize];
        hex_buf[i * 2 + 1] = HEX[(b & 0x0F) as usize];
    }
    // SAFETY: all bytes come from HEX which is ASCII.
    unsafe { String::from_utf8_unchecked(hex_buf.to_vec()) }
}

/// Built-in observability middleware.
///
/// Creates a tracing span with standard HTTP fields that rolly's OtlpLayer
/// picks up automatically for OTLP export.
///
/// - **Request ID**: from incoming header (e.g. CloudFront `x-amz-cf-id`) or
///   generated via atomic counter (11 chars, no RNG). Echoed in the response.
/// - **Trace ID**: derived from the request ID via blake3 (128-bit, 32-char hex,
///   W3C compliant). Deterministic — same request ID always yields the same trace.
///
/// Reads `Arc<O11yConfig>` from application state; falls back to a static
/// default when absent.
pub async fn o11y_middleware(mut req: Request, next: Next) -> Response {
    let config = req
        .try_state::<Arc<O11yConfig>>()
        .cloned()
        .unwrap_or_else(|| Arc::clone(&DEFAULT_CONFIG));

    // Extract or generate request ID.
    let request_id = req
        .header(&config.request_id_header)
        .map(|s| s.to_string())
        .unwrap_or_else(generate_request_id);

    // Derive W3C trace ID from request ID.
    let trace_id = derive_trace_id(&request_id);

    // Build span — borrows req fields as &str (zero allocation).
    let span = {
        let method = req.method().as_str();
        let path = req.path();
        let route = req.route_pattern().unwrap_or_else(|| req.path());
        tracing::info_span!(
            "http_request",
            { fields::TRACE_ID } = trace_id.as_str(),
            { fields::HTTP_METHOD } = method,
            { fields::HTTP_URI } = path,
            route = route,
            request_id = request_id.as_str(),
            { fields::HTTP_STATUS_CODE } = tracing::field::Empty,
            { fields::HTTP_LATENCY_MS } = tracing::field::Empty,
        )
    };

    req.set_request_id(request_id.clone());

    let start = Instant::now();
    let span_handle = span.clone();
    let resp = next.run(req).instrument(span).await;

    // Record response fields into the span — no separate event.
    span_handle.record(fields::HTTP_STATUS_CODE, resp.status_code().as_u16());
    span_handle.record(
        fields::HTTP_LATENCY_MS,
        start.elapsed().as_secs_f64() * 1000.0,
    );

    resp.header(&config.request_id_header, &request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;

    async fn make_request_with_state(headers: &[(&str, &str)], state: TypeMap) -> Request {
        let mut builder = http::Request::builder().method("GET").uri("/test");
        for &(name, value) in headers {
            builder = builder.header(name, value);
        }
        let inner = builder
            .body(harrow_core::request::full_body(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
            .unwrap();
        Request::new(inner, PathMatch::default(), Arc::new(state), None)
    }

    fn ok_next() -> Next {
        Next::new(|_req| Box::pin(async { Response::ok() }))
    }

    // Ensure tracing subscriber is installed for tests (no-op if already set).
    fn init_tracing() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    }

    #[tokio::test]
    async fn generates_request_id_when_absent() {
        init_tracing();
        let req = make_request_with_state(&[], TypeMap::new()).await;
        let resp = Middleware::call(&o11y_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        let rid = inner
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(rid.len(), 11);
        assert!(rid.is_ascii());
    }

    #[tokio::test]
    async fn preserves_request_id_from_header() {
        init_tracing();
        let req =
            make_request_with_state(&[("x-request-id", "incoming-id-123")], TypeMap::new()).await;
        let resp = Middleware::call(&o11y_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("x-request-id").unwrap(),
            "incoming-id-123"
        );
    }

    #[tokio::test]
    async fn uses_config_from_state() {
        init_tracing();
        let config = O11yConfig::default().request_id_header("x-trace-id");
        let mut state = TypeMap::new();
        state.insert(Arc::new(config));
        let req = make_request_with_state(&[("x-trace-id", "custom-trace")], state).await;
        let resp = Middleware::call(&o11y_middleware, req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-trace-id").unwrap(), "custom-trace");
        // Default header should not be set.
        assert!(inner.headers().get("x-request-id").is_none());
    }

    #[tokio::test]
    async fn falls_back_to_default_config() {
        init_tracing();
        // Empty state — no O11yConfig registered.
        let req = make_request_with_state(&[], TypeMap::new()).await;
        let resp = Middleware::call(&o11y_middleware, req, ok_next()).await;
        // Should not panic and should use default "x-request-id".
        let inner = resp.into_inner();
        assert!(inner.headers().get("x-request-id").is_some());
    }

    #[tokio::test]
    async fn sets_request_id_on_request_object() {
        init_tracing();
        // Capture the request_id from inside the handler.
        let captured = Arc::new(std::sync::Mutex::new(None::<String>));
        let captured_clone = Arc::clone(&captured);
        let next = Next::new(move |req: Request| {
            let captured = Arc::clone(&captured_clone);
            Box::pin(async move {
                *captured.lock().unwrap() = req.request_id().map(String::from);
                Response::ok()
            })
        });
        let req = make_request_with_state(&[], TypeMap::new()).await;
        let resp = Middleware::call(&o11y_middleware, req, next).await;
        let inner = resp.into_inner();
        let response_rid = inner
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let captured_rid = captured.lock().unwrap().clone().unwrap();
        assert_eq!(captured_rid, response_rid);
    }

    #[tokio::test]
    async fn generated_ids_are_unique() {
        use std::collections::HashSet;
        let mut ids = HashSet::new();
        for _ in 0..1000 {
            let id = generate_request_id();
            assert!(ids.insert(id), "duplicate request ID generated");
        }
    }

    #[tokio::test]
    async fn derive_trace_id_is_valid_w3c() {
        let trace = derive_trace_id("test-request-id");
        assert_eq!(trace.len(), 32);
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn derive_trace_id_is_deterministic() {
        let a = derive_trace_id("same-input");
        let b = derive_trace_id("same-input");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn derive_trace_id_differs_for_different_inputs() {
        let a = derive_trace_id("request-1");
        let b = derive_trace_id("request-2");
        assert_ne!(a, b);
    }
}
