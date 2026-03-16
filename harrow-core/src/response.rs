use bytes::Bytes;
use futures_util::Stream;
use http::StatusCode;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};

/// The response body type. Both buffered and streaming paths go through `BoxBody`
/// so that all body types share a uniform `Body` impl for hyper's `serve_connection`.
pub type ResponseBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Harrow's response wrapper. Built via chained methods, no builder traits.
pub struct Response {
    inner: http::Response<ResponseBody>,
}

/// Box a `Full<Bytes>` into a `ResponseBody`. The `Infallible` error is
/// mapped away at zero cost since it can never occur.
fn full_to_body(full: Full<Bytes>) -> ResponseBody {
    full.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
        .boxed()
}

impl Response {
    /// Create a response with the given status and body.
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body = full_to_body(Full::new(body.into()));
        let inner = http::Response::builder()
            .status(status)
            .body(body)
            .expect("valid response");
        Self { inner }
    }

    /// Create a streaming response from a `Stream` of `Frame<Bytes>`.
    pub fn streaming<S>(status: StatusCode, stream: S) -> Self
    where
        S: Stream<
                Item = Result<hyper::body::Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>,
            > + Send
            + Sync
            + 'static,
    {
        let body = StreamBody::new(stream).boxed();
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
    pub fn text(body: impl Into<String>) -> Self {
        let body: String = body.into();
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
/// Implement this on your error types to enable `Result<Response, E>` handlers.
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl<E: IntoResponse> IntoResponse for Result<Response, E> {
    fn into_response(self) -> Response {
        match self {
            Ok(r) => r,
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
        assert_eq!(body_bytes(resp).await, Bytes::from("created"));
    }

    #[tokio::test]
    async fn ok_returns_200_empty() {
        let resp = Response::ok();
        assert_eq!(resp.status_code(), StatusCode::OK);
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

        let msg = Msg {
            key: "val".into(),
        };
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

    #[test]
    fn into_response_result_ok() {
        let result: Result<Response, Response> = Ok(Response::text("ok"));
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
        use hyper::body::Frame;
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
