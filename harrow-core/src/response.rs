use std::borrow::Cow;

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use http::StatusCode;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};

/// The response body type. Both buffered and streaming paths go through
/// `UnsyncBoxBody` so all body types share a uniform `Body` impl without
/// requiring cross-thread sharing on the hot path.
pub type ResponseBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Harrow's response wrapper. Built via chained methods, no builder traits.
pub struct Response {
    inner: http::Response<ResponseBody>,
}

/// Box a `Full<Bytes>` into a `ResponseBody`. The `Infallible` error is
/// mapped away at zero cost since it can never occur.
fn full_to_body(full: Full<Bytes>) -> ResponseBody {
    full.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
        .boxed_unsync()
}

impl Response {
    /// Create a response with the given status and body.
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let content_length = body.len();
        let body = full_to_body(Full::new(body));
        let inner = http::Response::builder()
            .status(status)
            .header(http::header::CONTENT_LENGTH, content_length)
            .body(body)
            .expect("valid response");
        Self { inner }
    }

    /// Create a streaming response from a `Stream` of `Frame<Bytes>`.
    pub fn streaming<S>(status: StatusCode, stream: S) -> Self
    where
        S: Stream<Item = Result<http_body::Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'static,
    {
        let body = StreamBody::new(stream).boxed_unsync();
        let inner = http::Response::builder()
            .status(status)
            .body(body)
            .expect("valid response");
        Self { inner }
    }

    /// 200 OK with empty body.
    pub fn ok() -> Self {
        Self::new(StatusCode::OK, Bytes::new())
    }

    /// 200 OK with a text body.
    pub fn text(body: impl Into<Bytes>) -> Self {
        let mut resp = Self::new(StatusCode::OK, body);
        resp.set_header_static(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        resp
    }

    /// 200 OK with a JSON body.
    #[cfg(feature = "json")]
    pub fn json(value: &impl serde::Serialize) -> Self {
        match harrow_serde::json::serialize(value) {
            Ok(bytes) => {
                let mut resp = Self::new(StatusCode::OK, bytes);
                resp.set_header_static(
                    http::header::CONTENT_TYPE,
                    http::header::HeaderValue::from_static(harrow_serde::json::CONTENT_TYPE),
                );
                resp
            }
            Err(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "serialization error"),
        }
    }

    /// 200 OK with a MessagePack body.
    #[cfg(feature = "msgpack")]
    pub fn msgpack(value: &impl serde::Serialize) -> Self {
        match harrow_serde::msgpack::serialize(value) {
            Ok(bytes) => {
                let mut resp = Self::new(StatusCode::OK, bytes);
                resp.set_header_static(
                    http::header::CONTENT_TYPE,
                    http::header::HeaderValue::from_static(harrow_serde::msgpack::CONTENT_TYPE),
                );
                resp
            }
            Err(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "serialization error"),
        }
    }

    /// Set the status code.
    pub fn status(mut self, status: u16) -> Self {
        *self.inner.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
        self
    }

    /// Set a header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.set_header(name, value);
        self
    }

    /// Append a header value without overwriting existing values.
    /// Needed for headers like `set-cookie` that can appear multiple times.
    pub fn append_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) {
            self.inner.headers_mut().append(name, value);
        }
        self
    }

    fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) {
            self.inner.headers_mut().insert(name, value);
        }
    }

    fn set_header_static(
        &mut self,
        name: http::header::HeaderName,
        value: http::header::HeaderValue,
    ) {
        self.inner.headers_mut().insert(name, value);
    }

    /// The HTTP status code.
    pub fn status_code(&self) -> StatusCode {
        self.inner.status()
    }

    /// Borrow the inner `http::Response` for inspection.
    pub fn inner(&self) -> &http::Response<ResponseBody> {
        &self.inner
    }

    /// Consume and return the inner `http::Response`.
    pub fn into_inner(self) -> http::Response<ResponseBody> {
        self.inner
    }
}

/// Trait for types that can be converted into a `Response`.
/// Implement this on your own types to allow plain-value and `Result<T, E>` handlers.
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for Box<str> {
    fn into_response(self) -> Response {
        String::from(self).into_response()
    }
}

impl IntoResponse for Cow<'static, str> {
    fn into_response(self) -> Response {
        match self {
            Cow::Borrowed(s) => Response::text(s),
            Cow::Owned(s) => Response::text(s),
        }
    }
}

impl IntoResponse for &'static [u8] {
    fn into_response(self) -> Response {
        Bytes::from_static(self).into_response()
    }
}

impl IntoResponse for Vec<u8> {
    fn into_response(self) -> Response {
        Bytes::from(self).into_response()
    }
}

impl IntoResponse for Cow<'static, [u8]> {
    fn into_response(self) -> Response {
        match self {
            Cow::Borrowed(bytes) => Bytes::from_static(bytes).into_response(),
            Cow::Owned(bytes) => Bytes::from(bytes).into_response(),
        }
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> Response {
        let mut resp = Response::new(StatusCode::OK, self);
        resp.set_header_static(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/octet-stream"),
        );
        resp
    }
}

impl IntoResponse for BytesMut {
    fn into_response(self) -> Response {
        self.freeze().into_response()
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::ok()
    }
}

impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    fn into_response(self) -> Response {
        match self {
            Ok(r) => r.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_bytes(resp: Response) -> Bytes {
        resp.into_inner()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
    }

    #[tokio::test]
    async fn new_sets_status_and_body() {
        let resp = Response::new(StatusCode::CREATED, "created");
        assert_eq!(resp.status_code(), StatusCode::CREATED);
        assert_eq!(
            resp.inner()
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
        assert_eq!(body_bytes(resp).await, Bytes::from("created"));
    }

    #[tokio::test]
    async fn ok_returns_200_empty() {
        let resp = Response::ok();
        assert_eq!(resp.status_code(), StatusCode::OK);
        assert_eq!(
            resp.inner()
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
        assert_eq!(body_bytes(resp).await, Bytes::new());
    }

    #[tokio::test]
    async fn text_sets_content_type_and_body() {
        let resp = Response::text("hello");
        assert_eq!(resp.status_code(), StatusCode::OK);
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            inner
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("5")
        );
        let body = inner.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from("hello"));
    }

    #[cfg(feature = "json")]
    #[tokio::test]
    async fn json_sets_content_type_and_body() {
        let resp = Response::json(&serde_json::json!({"key": "val"}));
        assert_eq!(resp.status_code(), StatusCode::OK);
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = inner.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed, serde_json::json!({"key": "val"}));
    }

    #[cfg(feature = "msgpack")]
    #[tokio::test]
    async fn msgpack_sets_content_type_and_body() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Msg {
            key: String,
        }

        let msg = Msg { key: "val".into() };
        let resp = Response::msgpack(&msg);
        assert_eq!(resp.status_code(), StatusCode::OK);
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/msgpack"
        );
        let body = inner.into_body().collect().await.unwrap().to_bytes();
        let parsed: Msg = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn status_overrides() {
        let resp = Response::ok().status(404);
        assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn status_invalid_falls_back_to_ok() {
        let resp = Response::ok().status(9999);
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    #[test]
    fn header_sets_value() {
        let resp = Response::ok().header("x-custom", "value");
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-custom").unwrap(), "value");
    }

    #[test]
    fn append_header_adds_multiple_values() {
        let resp = Response::ok()
            .append_header("set-cookie", "a=1")
            .append_header("set-cookie", "b=2");
        let inner = resp.into_inner();
        let values: Vec<&str> = inner
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, vec!["a=1", "b=2"]);
    }

    #[test]
    fn header_chain() {
        let resp = Response::ok().header("x-one", "1").header("x-two", "2");
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("x-one").unwrap(), "1");
        assert_eq!(inner.headers().get("x-two").unwrap(), "2");
    }

    #[test]
    fn into_response_identity() {
        let resp = Response::ok();
        let resp = resp.into_response();
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    #[tokio::test]
    async fn into_response_static_str_sets_text_plain() {
        let resp = "ok".into_response();
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        let body = inner.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from("ok"));
    }

    #[tokio::test]
    async fn into_response_bytes_sets_octet_stream() {
        let resp = Bytes::from_static(b"ok").into_response();
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let body = inner.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from_static(b"ok"));
    }

    #[test]
    fn into_response_result_ok() {
        let result: Result<Response, Response> = Ok(Response::text("ok"));
        let resp = result.into_response();
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    #[test]
    fn into_response_result_plain_ok() {
        let result: Result<&'static str, Response> = Ok("ok");
        let resp = result.into_response();
        assert_eq!(resp.status_code(), StatusCode::OK);
    }

    #[test]
    fn into_response_result_err() {
        let result: Result<Response, Response> =
            Err(Response::new(StatusCode::INTERNAL_SERVER_ERROR, "error"));
        let resp = result.into_response();
        assert_eq!(resp.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn streaming_response_collects_frames() {
        use http_body::Frame;
        let chunks = vec![
            Ok(Frame::data(Bytes::from("hello "))),
            Ok(Frame::data(Bytes::from("world"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let resp = Response::streaming(StatusCode::OK, stream);
        assert_eq!(resp.status_code(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, Bytes::from("hello world"));
    }
}
