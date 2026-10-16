//! Harrow implementation.
//!
//! Server setup: `serve_with_config` on a current-thread Tokio runtime, with
//! a 512-connection cap, 5 s header-read timeout, and a 4 KiB body limit.
//! `serve_multi_worker` would spread load over all cores, but in Harrow 0.10
//! it has no external shutdown hook, so it cannot drain on SIGTERM yet.
//!
//! Shape: plain `async fn(Request)` handlers; cross-cutting concerns are
//! middleware that put typed values into request extensions (`RequestId`,
//! `AuthenticatedSession`), and handlers read them back.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::{Duration, Instant};

use harrow::{App, Next, Request, Response, ServerConfig};
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
                .get("/session/current", api_fn(current_session))
                .post("/session/logout", api_fn(logout))
                .get("/companies", api_fn(list_companies))
                .post("/companies", api_fn(create_company))
                .get("/companies/:company_id/months/:month/state", api_fn(month_state))
                .get("/companies/:company_id/documents/:file_id/content", api_fn(document_content))
                .get("/usage", api_fn(usage))
                .group("/admin", |admin| admin.middleware(require_admin).get("/users", api_fn(admin_users)))
        })
        .not_found_handler(api_fn(not_found))
        // celeratax answers a wrong method with the same 404 as an unknown path.
        .method_not_allowed_handler(|req, _allowed| api_fn(not_found)(req))
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

async fn not_found(_req: Request) -> Result<Response, ApiError> {
    Err(ApiError::NotFound)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RequestId(String);

/// Request id, response header, and one access-log line per request.
async fn request_context(mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = req
        .header(domain::CLOUDFRONT_REQUEST_ID)
        .map(str::to_owned)
        .unwrap_or_else(domain::new_request_id);
    let method = req.method().clone();
    req.set_ext(RequestId(request_id.clone()));

    let res = next.run(req).await.header("x-request-id", &request_id);
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
        return error_response(ApiError::Forbidden, &req);
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
        Err(err) => error_response(err, &req),
    }
}

/// Non-admins get the same 404 as an unknown route.
async fn require_admin(req: Request, next: Next) -> Response {
    if domain::is_admin(&session(&req).email) {
        next.run(req).await
    } else {
        error_response(ApiError::NotFound, &req)
    }
}

// ---------------------------------------------------------------------------
// Glue
// ---------------------------------------------------------------------------

/// Adapt a fallible handler: render `ApiError` with the request's id.
fn api_fn<F, Fut>(handler: F) -> impl Fn(Request) -> Pin<Box<dyn Future<Output = Response>>> + 'static
where
    F: Fn(Request) -> Fut + Copy + 'static,
    Fut: Future<Output = Result<Response, ApiError>> + 'static,
{
    move |req| {
        let request_id = request_id(&req).to_owned();
        Box::pin(async move {
            handler(req).await.unwrap_or_else(|err| render_error(err, &request_id))
        })
    }
}

fn error_response(err: ApiError, req: &Request) -> Response {
    render_error(err, request_id(req))
}

fn render_error(err: ApiError, request_id: &str) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let res = Response::new(status, err.body(request_id))
        .header("content-type", "application/json")
        .header("cache-control", "no-store");
    match err.www_authenticate() {
        Some(scheme) => res.header("www-authenticate", scheme),
        None => res,
    }
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

fn request_id(req: &Request) -> &str {
    req.ext::<RequestId>().map_or("", |id| id.0.as_str())
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
