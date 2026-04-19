use harrow_core::handler::HandlerFuture;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

/// CORS configuration. All fields have permissive defaults.
pub struct CorsConfig {
    /// Allowed origins. Empty means `*` (any origin).
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods. Defaults to common methods.
    pub allowed_methods: Vec<String>,
    /// Allowed request headers. Empty means `*`.
    pub allowed_headers: Vec<String>,
    /// Response headers to expose to the browser.
    pub expose_headers: Vec<String>,
    /// `Access-Control-Max-Age` in seconds for preflight caching.
    pub max_age: Option<u64>,
    /// Whether to send `Access-Control-Allow-Credentials: true`.
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            allowed_methods: vec![
                "GET".into(),
                "POST".into(),
                "PUT".into(),
                "DELETE".into(),
                "PATCH".into(),
                "OPTIONS".into(),
            ],
            allowed_headers: Vec::new(),
            expose_headers: Vec::new(),
            max_age: Some(86400),
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    pub fn allowed_origins(mut self, origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    pub fn allowed_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_methods = methods.into_iter().map(Into::into).collect();
        self
    }

    pub fn allowed_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.expose_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    pub fn max_age(mut self, seconds: u64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.allow_credentials = allow;
        self
    }
}

/// Returns a middleware that applies CORS headers based on `config`.
///
/// Handles preflight `OPTIONS` requests automatically with a 204 response.
/// For all other requests, the appropriate CORS headers are appended.
pub fn cors_middleware(config: CorsConfig) -> CorsMiddleware {
    CorsMiddleware {
        config: std::sync::Arc::new(config),
    }
}

pub struct CorsMiddleware {
    config: std::sync::Arc<CorsConfig>,
}

impl Middleware for CorsMiddleware {
    fn call(&self, req: Request, next: Next) -> HandlerFuture {
        let config = std::sync::Arc::clone(&self.config);
        Box::pin(async move {
            let origin = req.header("origin").map(|s| s.to_string());
            let is_preflight = req.method() == http::Method::OPTIONS
                && req.header("access-control-request-method").is_some();

            if is_preflight {
                let mut resp = Response::new(http::StatusCode::NO_CONTENT, "");
                resp = apply_cors_headers(resp, &config, origin.as_deref());
                // Preflight-specific headers.
                let methods = config.allowed_methods.join(", ");
                resp = resp.header("access-control-allow-methods", &methods);
                let headers = if config.allowed_headers.is_empty() {
                    req.header("access-control-request-headers")
                        .unwrap_or("*")
                        .to_string()
                } else {
                    config.allowed_headers.join(", ")
                };
                resp = resp.header("access-control-allow-headers", &headers);
                if let Some(max_age) = config.max_age {
                    resp = resp.header("access-control-max-age", &max_age.to_string());
                }
                return resp;
            }

            let resp = next.run(req).await;
            apply_cors_headers(resp, &config, origin.as_deref())
        })
    }
}

fn apply_cors_headers(resp: Response, config: &CorsConfig, origin: Option<&str>) -> Response {
    let allow_origin = if config.allowed_origins.is_empty() {
        // Wildcard — but if credentials are enabled, must echo origin.
        if config.allow_credentials {
            origin.unwrap_or("*").to_string()
        } else {
            "*".to_string()
        }
    } else {
        match origin {
            Some(o) if config.allowed_origins.iter().any(|a| a == o) => o.to_string(),
            _ => return resp, // Origin not allowed — skip CORS headers entirely.
        }
    };

    let mut resp = resp.header("access-control-allow-origin", &allow_origin);

    if config.allow_credentials {
        resp = resp.header("access-control-allow-credentials", "true");
    }

    if !config.expose_headers.is_empty() {
        let expose = config.expose_headers.join(", ");
        resp = resp.header("access-control-expose-headers", &expose);
    }

    // Vary on Origin when not using wildcard.
    if !config.allowed_origins.is_empty() || config.allow_credentials {
        resp = resp.header("vary", "Origin");
    }

    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    async fn make_request(method: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = http::Request::builder().method(method).uri("/");
        for &(name, value) in headers {
            builder = builder.header(name, value);
        }
        let inner = builder
            .body(harrow_core::request::full_body(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
            .unwrap();
        Request::new(inner, PathMatch::default(), Arc::new(TypeMap::new()), None)
    }

    fn ok_next() -> Next {
        Next::new(|_req| Box::pin(async { Response::text("ok") }))
    }

    #[tokio::test]
    async fn default_config_sets_wildcard_origin() {
        let mw = cors_middleware(CorsConfig::default());
        let req = make_request("GET", &[("origin", "https://example.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn preflight_returns_204() {
        let mw = cors_middleware(CorsConfig::default());
        let req = make_request(
            "OPTIONS",
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "POST"),
            ],
        )
        .await;
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::NO_CONTENT);
        let inner = resp.into_inner();
        assert!(
            inner
                .headers()
                .get("access-control-allow-methods")
                .is_some()
        );
        assert!(inner.headers().get("access-control-max-age").is_some());
    }

    #[tokio::test]
    async fn allowed_origin_is_echoed() {
        let mw = cors_middleware(CorsConfig::default().allowed_origins(["https://good.com"]));
        let req = make_request("GET", &[("origin", "https://good.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-origin").unwrap(),
            "https://good.com"
        );
    }

    #[tokio::test]
    async fn disallowed_origin_gets_no_cors_headers() {
        let mw = cors_middleware(CorsConfig::default().allowed_origins(["https://good.com"]));
        let req = make_request("GET", &[("origin", "https://evil.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert!(inner.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn credentials_echoes_origin_not_wildcard() {
        let mw = cors_middleware(CorsConfig::default().allow_credentials(true));
        let req = make_request("GET", &[("origin", "https://example.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-origin").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            inner
                .headers()
                .get("access-control-allow-credentials")
                .unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn expose_headers_are_set() {
        let mw = cors_middleware(CorsConfig::default().expose_headers(["x-custom", "x-other"]));
        let req = make_request("GET", &[("origin", "https://example.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner
                .headers()
                .get("access-control-expose-headers")
                .unwrap(),
            "x-custom, x-other"
        );
    }

    #[tokio::test]
    async fn preflight_includes_custom_methods() {
        let mw = cors_middleware(CorsConfig::default().allowed_methods(["GET", "POST", "PATCH"]));
        let req = make_request(
            "OPTIONS",
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "PATCH"),
            ],
        )
        .await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-methods").unwrap(),
            "GET, POST, PATCH"
        );
    }

    #[tokio::test]
    async fn preflight_includes_custom_allowed_headers() {
        let mw = cors_middleware(
            CorsConfig::default().allowed_headers(["authorization", "content-type"]),
        );
        let req = make_request(
            "OPTIONS",
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "POST"),
            ],
        )
        .await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-headers").unwrap(),
            "authorization, content-type"
        );
    }

    #[tokio::test]
    async fn preflight_max_age_value() {
        let mw = cors_middleware(CorsConfig::default().max_age(3600));
        let req = make_request(
            "OPTIONS",
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "GET"),
            ],
        )
        .await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-max-age").unwrap(),
            "3600"
        );
    }

    #[tokio::test]
    async fn vary_header_set_when_origins_restricted() {
        let mw = cors_middleware(CorsConfig::default().allowed_origins(["https://good.com"]));
        let req = make_request("GET", &[("origin", "https://good.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("vary").unwrap(), "Origin");
    }

    #[tokio::test]
    async fn options_without_request_method_is_not_preflight() {
        let mw = cors_middleware(CorsConfig::default());
        // OPTIONS without access-control-request-method → not a preflight
        let req = make_request("OPTIONS", &[("origin", "https://example.com")]).await;
        let resp = mw.call(req, ok_next()).await;
        // Should get 200 from the next handler (ok_next returns text "ok")
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn request_without_origin_gets_wildcard() {
        let mw = cors_middleware(CorsConfig::default());
        let req = make_request("GET", &[]).await;
        let resp = mw.call(req, ok_next()).await;
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }
}
