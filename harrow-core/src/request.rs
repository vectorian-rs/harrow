use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use http_body_util::Full;
use http_body_util::combinators::UnsyncBoxBody;
use percent_encoding::percent_decode_str;

use crate::path::PathMatch;
use crate::response::{IntoResponse, Response};
use crate::state::{MissingExtError, TypeMap};

/// Type-erased request body. Allows constructing requests from any body type
/// (hyper `Incoming`, `Full<Bytes>`, etc.) without coupling to a specific impl.
pub type Body = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Maximum number of query pairs parsed to prevent OOM.
const MAX_QUERY_PAIRS: usize = 100;

/// Decode a query string component: `+` → space, then percent-decode.
fn decode_query_component(s: &str) -> String {
    let plus_decoded = s.replace('+', " ");
    percent_decode_str(&plus_decoded)
        .decode_utf8_lossy()
        .into_owned()
}

/// Default max body size: 2 MiB.
pub const DEFAULT_MAX_BODY_SIZE: usize = 2 * 1024 * 1024;

/// Harrow's request wrapper. Provides ergonomic access to path params,
/// query strings, body, and application state without extractor traits.
pub struct Request {
    inner: http::Request<Body>,
    path_match: PathMatch,
    state: Arc<TypeMap>,
    route_pattern: Option<Arc<str>>,
    request_id: Option<String>,
    max_body_size: usize,
}

impl Request {
    pub fn new(
        inner: http::Request<Body>,
        path_match: PathMatch,
        state: Arc<TypeMap>,
        route_pattern: Option<Arc<str>>,
    ) -> Self {
        Self {
            inner,
            path_match,
            state,
            route_pattern,
            request_id: None,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
        }
    }

    /// Set the maximum body size for this request.
    pub fn set_max_body_size(&mut self, limit: usize) {
        self.max_body_size = limit;
    }

    /// The HTTP method.
    pub fn method(&self) -> &Method {
        self.inner.method()
    }

    /// The request URI path.
    pub fn path(&self) -> &str {
        self.inner.uri().path()
    }

    /// The full URI as a string.
    pub fn uri(&self) -> &http::Uri {
        self.inner.uri()
    }

    /// Get a path parameter captured by the route pattern.
    /// Returns an empty string if the parameter does not exist.
    pub fn param(&self, name: &str) -> &str {
        self.path_match.get(name).unwrap_or("")
    }

    /// Parse query string into key-value pairs.
    ///
    /// Percent-decodes keys and values, treats `+` as space.
    /// Capped at 100 pairs to prevent OOM from pathological inputs.
    pub fn query_pairs(&self) -> HashMap<String, String> {
        self.inner
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|s| !s.is_empty())
            .take(MAX_QUERY_PAIRS)
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = decode_query_component(parts.next()?);
                let val = decode_query_component(parts.next().unwrap_or(""));
                Some((key, val))
            })
            .collect()
    }

    /// Look up a single query parameter by name without allocating a HashMap.
    ///
    /// Percent-decodes keys and values, treats `+` as space.
    /// Returns the first match.
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.inner
            .uri()
            .query()?
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = decode_query_component(parts.next()?);
                let val = decode_query_component(parts.next().unwrap_or(""));
                Some((key, val))
            })
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// Get a request header value as a string.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.headers().get(name)?.to_str().ok()
    }

    /// Access application state of type `T`.
    /// Returns `Err(MissingStateError)` if `T` was not registered via `App::state()`.
    pub fn require_state<T: Send + Sync + 'static>(
        &self,
    ) -> Result<&T, crate::state::MissingStateError> {
        self.state.require::<T>()
    }

    /// Try to access application state of type `T`.
    /// Returns `None` if `T` was not registered via `App::state()`.
    pub fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.state.try_get::<T>()
    }

    /// Insert per-request data. Used by middleware to pass data to handlers.
    pub fn set_ext<T: Clone + Send + Sync + 'static>(&mut self, val: T) {
        self.inner.extensions_mut().insert(val);
    }

    /// Get per-request data inserted by middleware.
    pub fn ext<T: Clone + Send + Sync + 'static>(&self) -> Option<&T> {
        self.inner.extensions().get::<T>()
    }

    /// Get per-request data, returning an error if missing.
    pub fn require_ext<T: Clone + Send + Sync + 'static>(&self) -> Result<&T, MissingExtError> {
        self.ext::<T>().ok_or(MissingExtError {
            type_name: std::any::type_name::<T>(),
        })
    }

    /// The route pattern that matched this request (e.g. `/users/:id`).
    pub fn route_pattern(&self) -> Option<&str> {
        self.route_pattern.as_deref()
    }

    /// Cheap `Arc<str>` clone of the route pattern. Used by o11y middleware
    /// to hold the route label across `next.run(req)`.
    pub fn route_pattern_arc(&self) -> Option<Arc<str>> {
        self.route_pattern.clone()
    }

    /// The request ID assigned by the o11y middleware.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Set the request ID. Called by o11y middleware before passing to handlers.
    pub fn set_request_id(&mut self, id: String) {
        self.request_id = Some(id);
    }

    /// Consume the request and collect the body as bytes.
    ///
    /// Enforces the max body size limit. Returns `BodyError::TooLarge` if
    /// the body exceeds the configured limit.
    pub async fn body_bytes(self) -> Result<Bytes, BodyError> {
        use http_body_util::BodyExt;

        let limit = self.max_body_size;
        let mut body = self.inner.into_body();
        let mut buf = bytes::BytesMut::new();

        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| BodyError::BodyRead(e.to_string()))?;
            if let Ok(data) = frame.into_data() {
                if limit > 0 && buf.len() + data.len() > limit {
                    return Err(BodyError::TooLarge);
                }
                buf.extend_from_slice(&data);
            }
        }

        Ok(buf.freeze())
    }

    /// Consume the request and deserialize the JSON body.
    #[cfg(feature = "json")]
    pub async fn body_json<T: serde::de::DeserializeOwned>(self) -> Result<T, BodyError> {
        let bytes = self.body_bytes().await?;
        harrow_serde::json::deserialize(&bytes).map_err(BodyError::Json)
    }

    /// Consume the request and deserialize the MessagePack body.
    #[cfg(feature = "msgpack")]
    pub async fn body_msgpack<T: serde::de::DeserializeOwned>(self) -> Result<T, BodyError> {
        let bytes = self.body_bytes().await?;
        harrow_serde::msgpack::deserialize(&bytes).map_err(BodyError::MsgPack)
    }

    /// Access the underlying `http::Request` headers.
    pub fn headers(&self) -> &http::HeaderMap {
        self.inner.headers()
    }

    /// Access the raw inner `http::Request<Body>`.
    /// Escape hatch for advanced use cases.
    pub fn inner(&self) -> &http::Request<Body> {
        &self.inner
    }

    /// Mutable access to the raw inner `http::Request<Body>`.
    /// Used by WebSocket upgrade to extract the `OnUpgrade` handle.
    pub fn inner_mut(&mut self) -> &mut http::Request<Body> {
        &mut self.inner
    }
}

/// Errors that can occur when reading a request body.
#[derive(Debug)]
pub enum BodyError {
    /// Body exceeded the configured max size limit.
    TooLarge,
    /// Generic body read error.
    BodyRead(String),
    #[cfg(feature = "json")]
    Json(harrow_serde::json::Error),
    #[cfg(feature = "msgpack")]
    MsgPack(harrow_serde::msgpack::DecodeError),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::TooLarge => write!(f, "body too large"),
            BodyError::BodyRead(e) => write!(f, "body read error: {e}"),
            #[cfg(feature = "json")]
            BodyError::Json(e) => write!(f, "json parse error: {e}"),
            #[cfg(feature = "msgpack")]
            BodyError::MsgPack(e) => write!(f, "msgpack parse error: {e}"),
        }
    }
}

impl std::error::Error for BodyError {}

impl IntoResponse for BodyError {
    fn into_response(self) -> Response {
        use http::StatusCode;
        match &self {
            BodyError::TooLarge => {
                crate::problem::ProblemDetail::new(StatusCode::PAYLOAD_TOO_LARGE)
                    .detail(self.to_string())
                    .into_response()
            }
            _ => crate::problem::ProblemDetail::new(StatusCode::BAD_REQUEST)
                .detail(self.to_string())
                .into_response(),
        }
    }
}

impl From<BodyError> for Response {
    fn from(err: BodyError) -> Self {
        err.into_response()
    }
}

/// Convert a `hyper::body::Incoming` into a harrow `Body`.
/// Called at the server boundary to box the hyper-specific body type.
#[cfg(feature = "hyper-compat")]
pub fn box_incoming(incoming: hyper::body::Incoming) -> Body {
    use http_body_util::BodyExt;
    incoming
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        .boxed_unsync()
}

/// Create a `Body` from a `Full<Bytes>`. Useful for constructing test requests
/// and for the `Client`.
pub fn full_body(body: Full<Bytes>) -> Body {
    use http_body_util::BodyExt;
    body.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
        .boxed_unsync()
}

/// Test utilities for creating `Request` instances.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;

    /// Create a harrow `Request` for testing.
    pub(crate) async fn make_request(
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        path_match: PathMatch,
        state: TypeMap,
        route_pattern: Option<&str>,
    ) -> Request {
        let body_bytes = body.map(Bytes::copy_from_slice).unwrap_or_default();
        let mut builder = http::Request::builder().method(method).uri(uri);
        for &(name, value) in headers {
            builder = builder.header(name, value);
        }
        if body.is_some() {
            builder = builder.header("content-length", body_bytes.len().to_string());
        }
        let inner = builder
            .body(crate::request::full_body(Full::new(body_bytes)))
            .expect("valid request");
        Request::new(
            inner,
            path_match,
            Arc::new(state),
            route_pattern.map(Arc::from),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::PathPattern;

    #[tokio::test]
    async fn method_returns_request_method() {
        let req = test_util::make_request(
            "POST",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.method(), http::Method::POST);
    }

    #[tokio::test]
    async fn path_returns_uri_path() {
        let req = test_util::make_request(
            "GET",
            "/users/42",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.path(), "/users/42");
    }

    #[tokio::test]
    async fn param_returns_captured_value() {
        let pm = PathPattern::parse("/users/:id")
            .match_path("/users/42")
            .unwrap();
        let req =
            test_util::make_request("GET", "/users/42", &[], None, pm, TypeMap::new(), None).await;
        assert_eq!(req.param("id"), "42");
    }

    #[tokio::test]
    async fn param_returns_empty_for_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.param("nonexistent"), "");
    }

    #[tokio::test]
    async fn query_pairs_parses_query() {
        let req = test_util::make_request(
            "GET",
            "/search?q=rust&page=2",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let pairs = req.query_pairs();
        assert_eq!(pairs.get("q").unwrap(), "rust");
        assert_eq!(pairs.get("page").unwrap(), "2");
    }

    #[tokio::test]
    async fn query_pairs_empty_for_no_query() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.query_pairs().is_empty());
    }

    #[tokio::test]
    async fn query_pairs_percent_decodes() {
        let req = test_util::make_request(
            "GET",
            "/search?q=hello%20world&tag=%E2%9C%93",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let pairs = req.query_pairs();
        assert_eq!(pairs.get("q").unwrap(), "hello world");
        assert_eq!(pairs.get("tag").unwrap(), "\u{2713}"); // ✓
    }

    #[tokio::test]
    async fn query_pairs_plus_as_space() {
        let req = test_util::make_request(
            "GET",
            "/search?q=hello+world",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let pairs = req.query_pairs();
        assert_eq!(pairs.get("q").unwrap(), "hello world");
    }

    #[tokio::test]
    async fn query_param_finds_single_value() {
        let req = test_util::make_request(
            "GET",
            "/search?q=rust&page=2",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.query_param("q"), Some("rust".to_string()));
        assert_eq!(req.query_param("page"), Some("2".to_string()));
        assert_eq!(req.query_param("missing"), None);
    }

    #[tokio::test]
    async fn query_param_decodes() {
        let req = test_util::make_request(
            "GET",
            "/search?name=hello%20world",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.query_param("name"), Some("hello world".to_string()));
    }

    #[tokio::test]
    async fn query_pairs_bounded() {
        // Build a query string with 200 pairs — only first 100 should be kept.
        let qs: String = (0..200)
            .map(|i| format!("k{i}=v{i}"))
            .collect::<Vec<_>>()
            .join("&");
        let uri = format!("/test?{qs}");
        let req = test_util::make_request(
            "GET",
            &uri,
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let pairs = req.query_pairs();
        assert_eq!(pairs.len(), 100);
    }

    #[tokio::test]
    async fn header_returns_value() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[("x-custom", "hello")],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert_eq!(req.header("x-custom"), Some("hello"));
    }

    #[tokio::test]
    async fn header_returns_none_for_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.header("x-nonexistent").is_none());
    }

    #[tokio::test]
    async fn require_state_returns_ok() {
        let mut state = TypeMap::new();
        state.insert(42u32);
        let req =
            test_util::make_request("GET", "/", &[], None, PathMatch::default(), state, None).await;
        assert_eq!(*req.require_state::<u32>().unwrap(), 42);
    }

    #[tokio::test]
    async fn require_state_returns_err_for_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.require_state::<u32>().is_err());
    }

    #[tokio::test]
    async fn try_state_returns_none_for_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.try_state::<u32>().is_none());
    }

    #[tokio::test]
    async fn route_pattern_returns_pattern() {
        let req = test_util::make_request(
            "GET",
            "/users/42",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            Some("/users/:id"),
        )
        .await;
        assert_eq!(req.route_pattern(), Some("/users/:id"));
    }

    #[tokio::test]
    async fn request_id_initially_none() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.request_id().is_none());
    }

    #[tokio::test]
    async fn set_request_id_stores_id() {
        let mut req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_request_id("abc-123".to_string());
        assert_eq!(req.request_id(), Some("abc-123"));
    }

    #[tokio::test]
    async fn body_bytes_reads_body() {
        let req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(b"hello body"),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let body = req.body_bytes().await.unwrap();
        assert_eq!(body, bytes::Bytes::from("hello body"));
    }

    #[tokio::test]
    async fn body_bytes_enforces_size_limit() {
        let mut req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(b"this body is too large"),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_max_body_size(5); // limit to 5 bytes
        let result = req.body_bytes().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BodyError::TooLarge));
    }

    #[tokio::test]
    async fn body_bytes_allows_within_limit() {
        let mut req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(b"hello"),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_max_body_size(100);
        let body = req.body_bytes().await.unwrap();
        assert_eq!(body, bytes::Bytes::from("hello"));
    }

    #[tokio::test]
    async fn body_bytes_no_limit_when_zero() {
        let mut req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(b"any size is fine"),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_max_body_size(0); // no limit
        let body = req.body_bytes().await.unwrap();
        assert_eq!(body, bytes::Bytes::from("any size is fine"));
    }

    #[tokio::test]
    async fn body_bytes_exact_limit_succeeds() {
        let data = b"12345";
        let mut req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(data),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_max_body_size(5); // exactly the body size
        let body = req.body_bytes().await.unwrap();
        assert_eq!(body, bytes::Bytes::from(&data[..]));
    }

    #[tokio::test]
    async fn body_bytes_default_limit_is_2mib() {
        let req = test_util::make_request(
            "POST",
            "/",
            &[],
            Some(b"small"),
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        // Default max_body_size should be 2 MiB
        assert_eq!(req.max_body_size, DEFAULT_MAX_BODY_SIZE);
        assert_eq!(req.max_body_size, 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn body_error_too_large_display() {
        let err = BodyError::TooLarge;
        assert_eq!(err.to_string(), "body too large");
    }

    #[tokio::test]
    async fn body_error_body_read_display() {
        let err = BodyError::BodyRead("connection reset".to_string());
        assert_eq!(err.to_string(), "body read error: connection reset");
    }

    #[tokio::test]
    async fn set_ext_and_ext_round_trips() {
        let mut req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_ext(42u32);
        assert_eq!(req.ext::<u32>(), Some(&42));
    }

    #[tokio::test]
    async fn ext_returns_none_when_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        assert!(req.ext::<u32>().is_none());
    }

    #[tokio::test]
    async fn require_ext_returns_ok_when_present() {
        let mut req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_ext("hello".to_string());
        assert_eq!(req.require_ext::<String>().unwrap(), "hello");
    }

    #[tokio::test]
    async fn require_ext_returns_err_when_missing() {
        let req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let err = req.require_ext::<u64>().unwrap_err();
        assert!(err.to_string().contains("was not set by middleware"));
    }

    #[tokio::test]
    async fn set_ext_overwrites_previous_value() {
        let mut req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_ext(1u32);
        req.set_ext(2u32);
        assert_eq!(req.ext::<u32>(), Some(&2));
    }

    #[tokio::test]
    async fn ext_multiple_types_coexist() {
        let mut req = test_util::make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        req.set_ext(42u32);
        req.set_ext("hello".to_string());
        assert_eq!(req.ext::<u32>(), Some(&42));
        assert_eq!(req.ext::<String>().unwrap(), "hello");
    }
}
