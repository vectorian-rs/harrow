use bytes::{BufMut, Bytes, BytesMut};
use http::StatusCode;
use http_body_util::Full;

/// Harrow's response wrapper. Built via chained methods, no builder traits.
pub struct Response {
    inner: http::Response<Full<Bytes>>,
}

impl Response {
    /// Create a response with the given status and body.
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body = Full::new(body.into());
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
        let mut buf = BytesMut::with_capacity(128);
        match serde_json::to_writer((&mut buf).writer(), value) {
            Ok(()) => {
                let mut resp = Self::new(StatusCode::OK, buf.freeze());
                resp.set_header_static(
                    http::header::CONTENT_TYPE,
                    http::header::HeaderValue::from_static("application/json"),
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

    /// Consume and return the inner `http::Response`.
    pub fn into_inner(self) -> http::Response<Full<Bytes>> {
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
}
