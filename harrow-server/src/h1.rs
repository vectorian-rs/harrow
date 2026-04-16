use std::sync::Arc;

use harrow_codec_h1::{CodecError, ParsedRequest};
use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;
use harrow_core::response::{Response, ResponseBody};

pub fn request_exceeds_body_limit(content_length: Option<u64>, max_body_size: usize) -> bool {
    max_body_size > 0
        && content_length.is_some_and(|content_length| {
            usize::try_from(content_length).map_or(true, |len| len > max_body_size)
        })
}

pub fn build_request(
    parsed: &ParsedRequest,
    body: Body,
) -> Result<http::Request<Body>, http::Error> {
    let mut builder = http::Request::builder()
        .method(&parsed.method)
        .uri(&parsed.uri)
        .version(parsed.version);

    for (name, value) in parsed.headers.iter() {
        builder = builder.header(name, value);
    }

    builder.body(body)
}

pub async fn dispatch_parsed_request(
    shared: Arc<SharedState>,
    parsed: &ParsedRequest,
    body: Body,
) -> http::Response<ResponseBody> {
    match build_request(parsed, body) {
        Ok(request) => dispatch(shared, request).await,
        Err(err) => Response::new(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("request build error: {err}"),
        )
        .into_inner(),
    }
}

pub fn response_body_permitted(is_head_request: bool, status: http::StatusCode) -> bool {
    !is_head_request
        && !status.is_informational()
        && status != http::StatusCode::NO_CONTENT
        && status != http::StatusCode::NOT_MODIFIED
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ErrorResponse {
    BadRequest,
    RequestHeadersTooLarge,
    RequestTimeout,
    PayloadTooLarge,
}

impl ErrorResponse {
    pub fn from_codec_error(err: &CodecError) -> Self {
        match err {
            CodecError::Incomplete | CodecError::Invalid(_) => Self::BadRequest,
            CodecError::BodyTooLarge => Self::PayloadTooLarge,
        }
    }

    pub fn status(self) -> http::StatusCode {
        match self {
            Self::BadRequest | Self::RequestHeadersTooLarge => http::StatusCode::BAD_REQUEST,
            Self::RequestTimeout => http::StatusCode::REQUEST_TIMEOUT,
            Self::PayloadTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
        }
    }

    pub fn status_u16(self) -> u16 {
        self.status().as_u16()
    }

    pub fn body(self) -> &'static str {
        match self {
            Self::BadRequest => "bad request",
            Self::RequestHeadersTooLarge => "request headers too large",
            Self::RequestTimeout => "request timeout",
            Self::PayloadTooLarge => "payload too large",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResponseBodyMode {
    None,
    Fixed,
    Chunked,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResponseWritePlan {
    pub mode: ResponseBodyMode,
}

impl ResponseWritePlan {
    pub fn new(headers: &http::HeaderMap, is_head_request: bool, status: http::StatusCode) -> Self {
        let mode = if !response_body_permitted(is_head_request, status) {
            ResponseBodyMode::None
        } else if headers.contains_key(http::header::CONTENT_LENGTH) {
            ResponseBodyMode::Fixed
        } else {
            ResponseBodyMode::Chunked
        };

        Self { mode }
    }

    pub fn should_write_body(self) -> bool {
        self.mode != ResponseBodyMode::None
    }

    pub fn is_chunked(self) -> bool {
        self.mode == ResponseBodyMode::Chunked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    fn sample_request() -> ParsedRequest {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("localhost"),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );

        ParsedRequest {
            method: http::Method::POST,
            uri: "/hello?name=world".parse().unwrap(),
            version: http::Version::HTTP_11,
            headers,
            header_len: 0,
            keep_alive: true,
            content_length: Some(5),
            chunked: false,
            expect_continue: false,
        }
    }

    #[test]
    fn request_exceeds_limit_handles_large_values() {
        assert!(request_exceeds_body_limit(Some(11), 10));
        assert!(!request_exceeds_body_limit(Some(10), 10));
        assert!(!request_exceeds_body_limit(None, 10));
        assert!(request_exceeds_body_limit(Some(u64::MAX), 10));
    }

    #[test]
    fn build_request_preserves_head_parts() {
        let parsed = sample_request();
        let body = Full::new(Bytes::from_static(b"hello"))
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
            .boxed_unsync();

        let request = build_request(&parsed, body).unwrap();
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(request.uri(), &"/hello?name=world");
        assert_eq!(request.version(), http::Version::HTTP_11);
        assert_eq!(
            request.headers().get(http::header::HOST).unwrap(),
            "localhost"
        );
    }

    #[test]
    fn response_body_rule_matches_http_bodyless_cases() {
        assert!(!response_body_permitted(true, http::StatusCode::OK));
        assert!(!response_body_permitted(false, http::StatusCode::CONTINUE));
        assert!(!response_body_permitted(
            false,
            http::StatusCode::NO_CONTENT
        ));
        assert!(!response_body_permitted(
            false,
            http::StatusCode::NOT_MODIFIED
        ));
        assert!(response_body_permitted(false, http::StatusCode::OK));
    }

    #[test]
    fn response_write_plan_handles_body_modes() {
        let empty_headers = http::HeaderMap::new();
        assert_eq!(
            ResponseWritePlan::new(&empty_headers, true, http::StatusCode::OK).mode,
            ResponseBodyMode::None
        );
        assert_eq!(
            ResponseWritePlan::new(&empty_headers, false, http::StatusCode::OK).mode,
            ResponseBodyMode::Chunked
        );

        let mut fixed_headers = http::HeaderMap::new();
        fixed_headers.insert(http::header::CONTENT_LENGTH, "5".parse().unwrap());
        let plan = ResponseWritePlan::new(&fixed_headers, false, http::StatusCode::OK);
        assert_eq!(plan.mode, ResponseBodyMode::Fixed);
        assert!(plan.should_write_body());
        assert!(!plan.is_chunked());
    }

    #[test]
    fn error_response_maps_codec_and_wire_details() {
        assert_eq!(
            ErrorResponse::from_codec_error(&CodecError::Incomplete),
            ErrorResponse::BadRequest
        );
        assert_eq!(
            ErrorResponse::from_codec_error(&CodecError::Invalid("x".into())),
            ErrorResponse::BadRequest
        );
        assert_eq!(
            ErrorResponse::from_codec_error(&CodecError::BodyTooLarge),
            ErrorResponse::PayloadTooLarge
        );
        assert_eq!(
            ErrorResponse::RequestTimeout.status(),
            http::StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(
            ErrorResponse::RequestHeadersTooLarge.body(),
            "request headers too large"
        );
    }
}
