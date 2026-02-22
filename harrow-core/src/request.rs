use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use hyper::body::Incoming;

use crate::path::PathMatch;
use crate::state::TypeMap;

/// Harrow's request wrapper. Provides ergonomic access to path params,
/// query strings, body, and application state without extractor traits.
pub struct Request {
    inner: http::Request<Incoming>,
    path_match: PathMatch,
    state: Arc<TypeMap>,
    route_pattern: Option<String>,
    request_id: Option<String>,
}

impl Request {
    pub fn new(
        inner: http::Request<Incoming>,
        path_match: PathMatch,
        state: Arc<TypeMap>,
        route_pattern: Option<String>,
    ) -> Self {
        Self {
            inner,
            path_match,
            state,
            route_pattern,
            request_id: None,
        }
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
        self.path_match
            .get(name)
            .unwrap_or("")
    }

    /// Parse query string into key-value pairs.
    pub fn query_pairs(&self) -> HashMap<String, String> {
        self.inner
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?;
                let val = parts.next().unwrap_or("");
                Some((key.to_string(), val.to_string()))
            })
            .collect()
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
    pub async fn body_bytes(self) -> Result<Bytes, hyper::Error> {
        use http_body_util::BodyExt;
        let collected = self.inner.into_body().collect().await?;
        Ok(collected.to_bytes())
    }

    /// Consume the request and deserialize the JSON body.
    #[cfg(feature = "json")]
    pub async fn body_json<T: serde::de::DeserializeOwned>(self) -> Result<T, BodyError> {
        let bytes = self.body_bytes().await.map_err(BodyError::Hyper)?;
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
    #[cfg(feature = "json")]
    Json(serde_json::Error),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Hyper(e) => write!(f, "body read error: {e}"),
            #[cfg(feature = "json")]
            BodyError::Json(e) => write!(f, "json parse error: {e}"),
        }
    }
}

impl std::error::Error for BodyError {}
