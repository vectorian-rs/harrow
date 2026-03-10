use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use hyper::body::Incoming;
use percent_encoding::percent_decode_str;

use crate::path::PathMatch;
use crate::state::TypeMap;

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
    inner: http::Request<Incoming>,
    path_match: PathMatch,
    state: Arc<TypeMap>,
    route_pattern: Option<Arc<str>>,
    request_id: Option<String>,
    max_body_size: usize,
}

impl Request {
    pub fn new(
        inner: http::Request<Incoming>,
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
    /// Panics if `T` was not registered via `App::state()`.
    pub fn state<T: Send + Sync + 'static>(&self) -> &T {
        self.state.get::<T>()
    }

    /// Try to access application state of type `T`.
    /// Returns `None` if `T` was not registered via `App::state()`.
    pub fn try_state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.state.try_get::<T>()
    }

    /// The route pattern that matched this request (e.g. `/users/:id`).
    pub fn route_pattern(&self) -> Option<&str> {
        self.route_pattern.as_deref()
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

        if self.max_body_size == 0 {
            // No limit.
            let collected = self
                .inner
                .into_body()
                .collect()
                .await
                .map_err(BodyError::Hyper)?;
            return Ok(collected.to_bytes());
        }

        let limited = http_body_util::Limited::new(self.inner.into_body(), self.max_body_size);
        match limited.collect().await {
            Ok(collected) => Ok(collected.to_bytes()),
            Err(e) => {
                // Limited returns a LengthLimitError if exceeded.
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some()
                {
                    Err(BodyError::TooLarge)
                } else {
                    // Re-wrap as a generic body error string since we can't
                    // recover the original hyper::Error from Box<dyn Error>.
                    Err(BodyError::BodyRead(e.to_string()))
                }
            }
        }
    }

    /// Consume the request and deserialize the JSON body.
    #[cfg(feature = "json")]
    pub async fn body_json<T: serde::de::DeserializeOwned>(self) -> Result<T, BodyError> {
        let bytes = self.body_bytes().await?;
        serde_json::from_slice(&bytes).map_err(BodyError::Json)
    }

    /// Access the underlying `http::Request` headers.
    pub fn headers(&self) -> &http::HeaderMap {
        self.inner.headers()
    }

    /// Access the raw inner `http::Request<Incoming>`.
    /// Escape hatch for advanced use cases.
    pub fn inner(&self) -> &http::Request<Incoming> {
        &self.inner
    }
}

/// Errors that can occur when reading a request body.
#[derive(Debug)]
pub enum BodyError {
    Hyper(hyper::Error),
    /// Body exceeded the configured max size limit.
    TooLarge,
    /// Generic body read error.
    BodyRead(String),
    #[cfg(feature = "json")]
    Json(serde_json::Error),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Hyper(e) => write!(f, "body read error: {e}"),
            BodyError::TooLarge => write!(f, "body too large"),
            BodyError::BodyRead(e) => write!(f, "body read error: {e}"),
            #[cfg(feature = "json")]
            BodyError::Json(e) => write!(f, "json parse error: {e}"),
        }
    }
}

impl std::error::Error for BodyError {}

/// Test utilities for creating `Request` instances.
///
/// Uses a tokio duplex stream with hyper HTTP/1 to produce a real
/// `http::Request<Incoming>`, since `Incoming` has no public constructor.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;

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
        let mut raw = format!("{method} {uri} HTTP/1.1\r\nhost: test\r\n");
        for &(name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(b) = body {
            raw.push_str(&format!("content-length: {}\r\n", b.len()));
        }
        raw.push_str("connection: close\r\n\r\n");
        let mut raw_bytes = raw.into_bytes();
        if let Some(b) = body {
            raw_bytes.extend_from_slice(b);
        }

        let (client, server) = tokio::io::duplex(4096);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Mutex::new(Some(tx));

        tokio::spawn(async move {
            let io = TokioIo::new(server);
            let _ = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        let sender = tx.lock().unwrap().take();
                        async move {
                            if let Some(tx) = sender {
                                let _ = tx.send(req);
                            }
                            Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(
                                Bytes::new(),
                            )))
                        }
                    }),
                )
                .await;
        });

        let mut client = client;
        client.write_all(&raw_bytes).await.unwrap();

        // Keep client alive until hyper processes the request and sends it
        // over the oneshot. Dropping early kills the connection before hyper
        // can call service_fn.
        let inner = rx.await.expect("failed to receive test request");
        drop(client);

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
    async fn state_returns_typed_value() {
        let mut state = TypeMap::new();
        state.insert(42u32);
        let req =
            test_util::make_request("GET", "/", &[], None, PathMatch::default(), state, None).await;
        assert_eq!(*req.state::<u32>(), 42);
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
}
