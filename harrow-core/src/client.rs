use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;
use http_body_util::BodyExt;
use http_body_util::Full;

use crate::dispatch::{SharedState, dispatch};
use crate::request::full_body;
use crate::response::ResponseBody;

/// A test client that dispatches requests through the full middleware + routing
/// pipeline without TCP. Created via `App::client()`.
pub struct Client {
    shared: Arc<SharedState>,
}

impl Client {
    pub(crate) fn new(shared: Arc<SharedState>) -> Self {
        Self { shared }
    }

    /// Send a request through the full pipeline.
    pub async fn request(&self, req: http::Request<Full<Bytes>>) -> TestResponse {
        let boxed = req.map(full_body);
        let http_resp = dispatch(Arc::clone(&self.shared), boxed).await;
        TestResponse::from_http(http_resp).await
    }

    /// GET shorthand.
    pub async fn get(&self, path: &str) -> TestResponse {
        let req = http::Request::get(path)
            .body(Full::new(Bytes::new()))
            .unwrap();
        self.request(req).await
    }

    /// POST shorthand.
    pub async fn post(&self, path: &str, body: impl Into<Bytes>) -> TestResponse {
        let req = http::Request::post(path)
            .body(Full::new(body.into()))
            .unwrap();
        self.request(req).await
    }

    /// PUT shorthand.
    pub async fn put(&self, path: &str, body: impl Into<Bytes>) -> TestResponse {
        let req = http::Request::put(path)
            .body(Full::new(body.into()))
            .unwrap();
        self.request(req).await
    }

    /// DELETE shorthand.
    pub async fn delete(&self, path: &str) -> TestResponse {
        let req = http::Request::delete(path)
            .body(Full::new(Bytes::new()))
            .unwrap();
        self.request(req).await
    }

    /// PATCH shorthand.
    pub async fn patch(&self, path: &str, body: impl Into<Bytes>) -> TestResponse {
        let req = http::Request::patch(path)
            .body(Full::new(body.into()))
            .unwrap();
        self.request(req).await
    }

    /// HEAD shorthand.
    pub async fn head(&self, path: &str) -> TestResponse {
        let req = http::Request::head(path)
            .body(Full::new(Bytes::new()))
            .unwrap();
        self.request(req).await
    }
}

/// Ergonomic response wrapper for test assertions.
pub struct TestResponse {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

impl TestResponse {
    async fn from_http(resp: http::Response<ResponseBody>) -> Self {
        let (parts, body) = resp.into_parts();
        let collected = body.collect().await.expect("body collection failed");
        Self {
            status: parts.status,
            headers: parts.headers,
            body: collected.to_bytes(),
        }
    }

    /// The HTTP status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// All response headers.
    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }

    /// Get a single header value as a string.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The response body as a UTF-8 string. Returns empty string on invalid UTF-8.
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }

    /// The raw response body bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.body
    }

    /// Deserialize the response body as JSON.
    #[cfg(feature = "json")]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        harrow_serde::json::deserialize(&self.body).expect("invalid JSON in response body")
    }

    /// Deserialize the response body as MessagePack.
    #[cfg(feature = "msgpack")]
    pub fn msgpack<T: serde::de::DeserializeOwned>(&self) -> T {
        harrow_serde::msgpack::deserialize(&self.body)
            .expect("invalid MessagePack in response body")
    }
}
