use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;

/// Middleware that enforces a request timeout.
///
/// If the downstream handler chain does not complete within `duration`,
/// the request is cancelled and a **408 Request Timeout** is returned.
pub struct TimeoutMiddleware {
    duration: Duration,
}

/// Create a [`TimeoutMiddleware`] that aborts requests exceeding `duration`.
pub fn timeout_middleware(duration: Duration) -> TimeoutMiddleware {
    TimeoutMiddleware { duration }
}

impl Middleware for TimeoutMiddleware {
    fn call(
        &self,
        req: Request,
        next: Next,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let duration = self.duration;
        Box::pin(async move {
            match tokio::time::timeout(duration, next.run(req)).await {
                Ok(response) => response,
                Err(_elapsed) => Response::new(http::StatusCode::REQUEST_TIMEOUT, "request timeout"),
            }
        })
    }
}
