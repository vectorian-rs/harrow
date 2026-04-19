use std::future::Future;
use std::pin::Pin;

use crate::request::Request;
use crate::response::Response;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Response>>>;

/// The concrete handler function type. A boxed async function from Request to Response.
/// No traits to implement, no generics to satisfy.
pub type HandlerFn = Box<dyn Fn(Request) -> HandlerFuture>;

/// Wrap a plain async function into a boxed `HandlerFn`.
///
/// ```ignore
/// async fn my_handler(req: Request) -> Response {
///     Response::ok()
/// }
/// let handler: HandlerFn = harrow_core::handler::wrap(my_handler);
/// ```
pub fn wrap<F, Fut, T>(f: F) -> HandlerFn
where
    F: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = T> + 'static,
    T: crate::response::IntoResponse + 'static,
{
    Box::new(move |req| {
        let fut = f(req);
        Box::pin(async move { fut.await.into_response() })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::PathMatch;
    use crate::request::test_util::make_request;
    use crate::state::TypeMap;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn wrap_creates_callable_handler() {
        async fn hello(_req: Request) -> Response {
            Response::text("hello")
        }

        let handler = wrap(hello);
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = handler(req).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn wrap_handler_receives_request_data() {
        async fn echo_path(req: Request) -> Response {
            Response::text(req.path().to_string())
        }

        let handler = wrap(echo_path);
        let req = make_request(
            "GET",
            "/hello/world",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = handler(req).await;
        let body = resp
            .into_inner()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, bytes::Bytes::from("/hello/world"));
    }

    #[tokio::test]
    async fn wrap_handler_can_return_result() {
        async fn fallible(_req: Request) -> Result<Response, Response> {
            Err(Response::new(http::StatusCode::BAD_REQUEST, "bad"))
        }

        let handler = wrap(fallible);
        let req = make_request(
            "GET",
            "/",
            &[],
            None,
            PathMatch::default(),
            TypeMap::new(),
            None,
        )
        .await;
        let resp = handler(req).await;
        assert_eq!(resp.status_code(), http::StatusCode::BAD_REQUEST);
    }
}
