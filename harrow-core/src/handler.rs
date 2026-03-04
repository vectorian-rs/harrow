use std::future::Future;
use std::pin::Pin;

use crate::request::Request;
use crate::response::Response;

/// The concrete handler function type. A boxed async function from Request to Response.
/// No traits to implement, no generics to satisfy.
pub type HandlerFn =
    Box<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

/// Wrap a plain async function into a boxed `HandlerFn`.
///
/// ```ignore
/// async fn my_handler(req: Request) -> Response {
///     Response::ok()
/// }
/// let handler: HandlerFn = harrow_core::handler::wrap(my_handler);
/// ```
pub fn wrap<F, Fut>(f: F) -> HandlerFn
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    Box::new(move |req| Box::pin(f(req)))
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
            "GET", "/", &[], None, PathMatch::default(), TypeMap::new(), None,
        ).await;
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
            "GET", "/hello/world", &[], None, PathMatch::default(), TypeMap::new(), None,
        ).await;
        let resp = handler(req).await;
        let body = resp.into_inner().into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, bytes::Bytes::from("/hello/world"));
    }

    #[tokio::test]
    async fn wrap_handler_accesses_state() {
        async fn get_count(req: Request) -> Response {
            let count = req.state::<u32>();
            Response::text(count.to_string())
        }

        let handler = wrap(get_count);
        let mut state = TypeMap::new();
        state.insert(42u32);
        let req = make_request(
            "GET", "/", &[], None, PathMatch::default(), state, None,
        ).await;
        let resp = handler(req).await;
        let body = resp.into_inner().into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, bytes::Bytes::from("42"));
    }
}
