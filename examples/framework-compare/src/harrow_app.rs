//! Harrow implementation.
//!
//! Server setup: `serve_with_config` on a current-thread Tokio runtime, with
//! a 512-connection cap, 5 s header-read timeout, and a 4 KiB body limit.
//! `serve_multi_worker` would spread load over all cores, but in Harrow 0.10
//! it has no external shutdown hook, so it cannot drain on SIGTERM yet.
//!
//! Shape: plain `async fn(Request) -> Result<Response, ApiError>` handlers;
//! cross-cutting concerns are middleware that put typed values into request
//! extensions (`AuthenticatedSession`), and handlers read them back. Errors
//! only tag their response (`Response::set_ext`); `request_context` is the one
//! place that renders the error envelope, because it knows the request id.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use harrow::{App, IntoResponse, Next, Request, Response, ServerConfig};
use http::{Method, StatusCode};

use crate::domain::{self, ApiError, AuthenticatedSession, SessionView, Store};

pub fn run(addr: SocketAddr, store: Store) {
    let config = ServerConfig {
        max_connections: 512,
        header_read_timeout: Some(Duration::from_secs(5)),
        max_body_size: domain::MAX_JSON_BODY,
        ..ServerConfig::default()
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(harrow::serve_with_config(app(store), addr, domain::shutdown_signal(), config))
        .expect("server");
}

pub fn app(store: Store) -> App {
    App::new()
        .state(store)
        .max_body_size(domain::MAX_JSON_BODY)
        .middleware(request_context)
        .get("/healthz", |_req| async { Response::text("ok") })
        .group("/api/v1", |api| {
            api.middleware(same_origin_for_posts)
                .middleware(require_session)
                .get("/session/current", current_session)
                .post("/session/logout", logout)
                .get("/companies", list_companies)
                .post("/companies", create_company)
                .get("/companies/:company_id/months/:month/state", month_state)
                .get("/companies/:company_id/documents/:file_id/content", document_content)
                .get("/usage", usage)
                .group("/admin", |admin| admin.middleware(require_admin).get("/users", admin_users))
        })
        .not_found_handler(|_req| async { ApiError::NotFound })
        // celeratax answers a wrong method with the same 404 as an unknown
        // path; `request_context` renders the error with its own status.
        .method_not_allowed_handler(|_req, _allowed| async { ApiError::NotFound })
}

// ---------------------------------------------------------------------------
// Handlers: `Request` in, `Result<Response, ApiError>` out
// ---------------------------------------------------------------------------

async fn current_session(req: Request) -> Result<Response, ApiError> {
    Ok(private(Response::json(&SessionView::from(session(&req)))))
}

async fn list_companies(req: Request) -> Result<Response, ApiError> {
    let companies = store(&req).companies_for(session(&req)).await?;
    Ok(private(Response::json(&companies)))
}

async fn create_company(req: Request) -> Result<Response, ApiError> {
    if !domain::is_json_content_type(req.header("content-type")) {
        return Err(ApiError::BadRequest);
    }
    let (store, session) = (store(&req).clone(), session(&req).clone());
    let body = req.body_bytes().await.map_err(|_| ApiError::BadRequest)?;
    let company = store.create_company(&session, domain::parse_create_company(&body)?).await?;
    Ok(private(Response::json(&company).status(201)))
}

async fn month_state(req: Request) -> Result<Response, ApiError> {
    let company_id = domain::validate_company_id(req.param("company_id"))?;
    let month = domain::validate_month(req.param("month"))?;
    let store = store(&req);
    let company = store.company_for_user(session(&req), company_id).await?;
    Ok(private(Response::json(&store.month_state(&company, month).await?)))
}

async fn document_content(req: Request) -> Result<Response, ApiError> {
    let company_id = domain::validate_company_id(req.param("company_id"))?;
    let file_id = domain::validate_file_id(req.param("file_id"))?;
    let store = store(&req);
    let company = store.company_for_user(session(&req), company_id).await?;
    let pdf = store.document(&company, file_id).await?;
    Ok(Response::new(StatusCode::OK, pdf)
        .header("content-type", "application/pdf")
        .header("content-disposition", &format!("inline; filename=\"{file_id}.pdf\""))
        .header("cache-control", "private, no-store")
        .header("x-content-type-options", "nosniff"))
}

async fn usage(req: Request) -> Result<Response, ApiError> {
    let period = domain::query_param(req.uri().query(), "period").ok_or(ApiError::BadRequest)?;
    let usage = store(&req).usage(session(&req), domain::validate_month(period)?).await?;
    Ok(private(Response::json(&usage)))
}

async fn logout(req: Request) -> Result<Response, ApiError> {
    store(&req).revoke(session(&req)).await?;
    Ok(Response::new(StatusCode::SEE_OTHER, "")
        .header("location", "/")
        .append_header("set-cookie", domain::CLEAR_SESSION_COOKIE)
        .append_header("set-cookie", domain::CLEAR_POW_COOKIE))
}

async fn admin_users(req: Request) -> Result<Response, ApiError> {
    Ok(private(Response::json(&store(&req).admin_list_users().await?)))
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Request id, error envelope, `x-request-id`, and one access-log line.
async fn request_context(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = req
        .header(domain::CLOUDFRONT_REQUEST_ID)
        .map(str::to_owned)
        .unwrap_or_else(domain::new_request_id);
    let method = req.method().clone();

    let mut res = next.run(req).await;
    if let Some(&err) = res.ext::<ApiError>() {
        res = render_error(err, &request_id);
    }
    let res = res.header("x-request-id", &request_id);
    tracing::info!(
        request_id,
        %method,
        status = res.status_code().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request completed"
    );
    res
}

async fn same_origin_for_posts(req: Request, next: Next) -> Response {
    let origins = req.headers().get_all("origin").iter().map(|v| v.as_bytes());
    if req.method() == Method::POST && !domain::same_origin(origins) {
        return ApiError::Forbidden.into_response();
    }
    next.run(req).await
}

async fn require_session(mut req: Request, next: Next) -> Response {
    let token = domain::session_token(req.header("cookie"));
    match store(&req).authenticate(token).await {
        Ok(session) => {
            req.set_ext(session);
            next.run(req).await
        }
        Err(err) => err.into_response(),
    }
}

/// Non-admins get the same 404 as an unknown route.
async fn require_admin(req: Request, next: Next) -> Response {
    if domain::is_admin(&session(&req).email) {
        next.run(req).await
    } else {
        ApiError::NotFound.into_response()
    }
}

// ---------------------------------------------------------------------------
// Glue
// ---------------------------------------------------------------------------

/// Tags the response; `request_context` renders the envelope with the request id.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = Response::new(status(self), "");
        res.set_ext(self);
        res
    }
}

fn render_error(err: ApiError, request_id: &str) -> Response {
    let res = Response::new(status(err), err.body(request_id))
        .header("content-type", "application/json")
        .header("cache-control", "no-store");
    match err.www_authenticate() {
        Some(scheme) => res.header("www-authenticate", scheme),
        None => res,
    }
}

fn status(err: ApiError) -> StatusCode {
    StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn private(res: Response) -> Response {
    domain::PRIVATE_HEADERS.iter().fold(res, |res, (k, v)| res.header(k, v))
}

fn store(req: &Request) -> &Store {
    req.try_state::<Store>().expect("Store is registered in app()")
}

fn session(req: &Request) -> &AuthenticatedSession {
    req.ext::<AuthenticatedSession>().expect("require_session runs before this handler")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_session_in_process() {
        let client = app(Store::seeded()).client();
        let req = http::Request::get("/api/v1/session/current")
            .header("cookie", "ct_session=sess-alice")
            .body(Default::default())
            .unwrap();
        let res = client.request(req).await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<SessionView>().email, "alice@example.com");
        assert!(res.header("x-request-id").is_some());
    }
}
