//! Tako implementation.
//!
//! Server setup: Tako's default `Server` (hyper on a multi-threaded Tokio
//! runtime) with a 512-connection cap, 5 s header-read timeout, 30 s drain on
//! SIGTERM/Ctrl-C via `ServerHandle::shutdown_on_signal`. The deployment guide
//! names this "the shape most services need"; the `per-thread` server
//! (SO_REUSEPORT, one current-thread runtime per core) is a throughput
//! optimisation that this I/O-light API behind CloudFront does not need, and
//! it pulls in an extra crate and feature flag.
//!
//! Shape: auth is an extractor (`Session`, `Admin`), the request body is a
//! typed extractor (`NewCompany`), and handlers return
//! `Result<impl Responder, ApiError>`. One global middleware owns the request
//! id, the access log, and the error envelope: it is the only place that
//! knows the request id, so `ApiError`'s `Responder` only tags the response
//! and the middleware renders the body. Tako's own plain-text rejections
//! (path/query parse errors, 405, 413) are rewritten into the same envelope.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use tako::extractors::body::collect_body;
use tako::extractors::{Entries, FromRequest, FromRequestParts};
use tako::prelude::*;
use tako::redirect;
use tako::{Server, ServerConfig};

use crate::domain::{self, ApiError, AuthenticatedSession, CreateCompany, SessionView, Store};

pub fn run(addr: SocketAddr, store: Store) {
    let config = ServerConfig {
        max_connections: Some(512),
        header_read_timeout: Some(Duration::from_secs(5)),
        drain_timeout: Duration::from_secs(30),
        ..ServerConfig::default()
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
            let handle = Server::builder().config(config).build().try_spawn_http(listener, router(store)).expect("server");
            handle.shutdown_on_signal().await.expect("signal handler");
        });
}

pub fn router(store: Store) -> Router {
    let mut router = Router::new();
    router.with_state(store).body_limit(domain::MAX_JSON_BODY).layer(request_context);
    router.get("/healthz", || async { "ok" });
    router.scope("/api/v1", |api| {
        api.get("/session/current", current_session);
        api.post("/session/logout", logout).middleware(same_origin);
        api.get("/companies", list_companies);
        api.post("/companies", create_company).middleware(same_origin);
        api.get("/companies/{company_id}/months/{month}/state", month_state);
        api.get("/companies/{company_id}/documents/{file_id}/content", document_content);
        api.get("/usage", usage);
        api.get("/admin/users", admin_users);
    });
    router.fallback(|_req: Request| async { ApiError::NotFound });
    router
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn current_session(Session(session): Session) -> Private<Json<SessionView>> {
    Private(Json(SessionView::from(&session)))
}

async fn list_companies(
    Session(session): Session,
    State(store): State<Store>,
) -> Result<Private<Json<Vec<domain::Company>>>, ApiError> {
    Ok(Private(Json(store.companies_for(&session).await?)))
}

async fn create_company(
    Session(session): Session,
    State(store): State<Store>,
    NewCompany(input): NewCompany,
) -> Result<impl Responder, ApiError> {
    let company = store.create_company(&session, input).await?;
    Ok(Private((StatusCode::CREATED, Json(company))))
}

#[derive(Deserialize)]
struct MonthPath {
    company_id: String,
    month: String,
}

async fn month_state(
    Session(session): Session,
    State(store): State<Store>,
    Path(p): Path<MonthPath>,
) -> Result<Private<Json<domain::MonthState>>, ApiError> {
    let company = store.company_for_user(&session, domain::validate_company_id(&p.company_id)?).await?;
    Ok(Private(Json(store.month_state(&company, domain::validate_month(&p.month)?).await?)))
}

#[derive(Deserialize)]
struct DocumentPath {
    company_id: String,
    file_id: String,
}

async fn document_content(
    Session(session): Session,
    State(store): State<Store>,
    Path(p): Path<DocumentPath>,
) -> Result<Response, ApiError> {
    let company_id = domain::validate_company_id(&p.company_id)?;
    let file_id = domain::validate_file_id(&p.file_id)?;
    let company = store.company_for_user(&session, company_id).await?;
    let pdf = store.document(&company, file_id).await?;
    let mut res = pdf.into_response();
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let disposition = format!("inline; filename=\"{file_id}.pdf\"");
    headers.insert(header::CONTENT_DISPOSITION, disposition.parse().map_err(|_| ApiError::Internal)?);
    Ok(res)
}

#[derive(Deserialize)]
struct UsageQuery {
    period: String,
}

async fn usage(
    Session(session): Session,
    State(store): State<Store>,
    Query(q): Query<UsageQuery>,
) -> Result<Private<Json<domain::Usage>>, ApiError> {
    Ok(Private(Json(store.usage(&session, domain::validate_month(&q.period)?).await?)))
}

async fn logout(Session(session): Session, State(store): State<Store>) -> Result<Response, ApiError> {
    store.revoke(&session).await?;
    let mut res = redirect::see_other("/").into_response();
    for cookie in [domain::CLEAR_SESSION_COOKIE, domain::CLEAR_POW_COOKIE] {
        res.headers_mut().append(header::SET_COOKIE, HeaderValue::from_static(cookie));
    }
    Ok(res)
}

async fn admin_users(_admin: Admin, State(store): State<Store>) -> Result<Private<Json<Vec<String>>>, ApiError> {
    Ok(Private(Json(store.admin_list_users().await?)))
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// An authenticated session from the `ct_session` cookie.
struct Session(AuthenticatedSession);

async fn authenticate(store: &Store, headers: &HeaderMap) -> Result<Session, ApiError> {
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    store.authenticate(domain::session_token(cookie)).await.map(Session)
}

impl<'a> FromRequestParts<'a> for Session {
    type Error = ApiError;
    const ENTRIES: Entries = Entries::STATE;

    async fn from_request_parts(parts: &'a mut http::request::Parts) -> Result<Self, ApiError> {
        let State(store) = State::<Store>::from_request_parts(parts).await.map_err(|_| ApiError::Internal)?;
        authenticate(&store, &parts.headers).await
    }
}

impl<'a> FromRequest<'a> for Session {
    type Error = ApiError;
    const ENTRIES: Entries = Entries::STATE;

    async fn from_request(req: &'a mut Request) -> Result<Self, ApiError> {
        let State(store) = State::<Store>::from_request(req).await.map_err(|_| ApiError::Internal)?;
        authenticate(&store, req.headers()).await
    }
}

/// A session whose user is an admin. Others get the same 404 as an unknown route.
struct Admin(#[allow(dead_code)] AuthenticatedSession);

impl<'a> FromRequestParts<'a> for Admin {
    type Error = ApiError;
    const ENTRIES: Entries = Entries::STATE;

    async fn from_request_parts(parts: &'a mut http::request::Parts) -> Result<Self, ApiError> {
        let Session(session) = Session::from_request_parts(parts).await?;
        if domain::is_admin(&session.email) { Ok(Admin(session)) } else { Err(ApiError::NotFound) }
    }
}

/// `application/json` body, size-limited by the router, validated by `domain`.
struct NewCompany(CreateCompany);

impl<'a> FromRequest<'a> for NewCompany {
    type Error = ApiError;
    const ENTRIES: Entries = Entries::BODY_LIMIT;

    async fn from_request(req: &'a mut Request) -> Result<Self, ApiError> {
        let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        if !domain::is_json_content_type(content_type) {
            return Err(ApiError::BadRequest);
        }
        let body = collect_body(req).await.map_err(|_| ApiError::BadRequest)?;
        domain::parse_create_company(&body).map(NewCompany)
    }
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Per-route guard for POSTs: exactly one `Origin`, equal to the app origin.
async fn same_origin(req: Request, next: Next) -> Result<Response, ApiError> {
    let origins = req.headers().get_all(header::ORIGIN).iter().map(HeaderValue::as_bytes);
    if !domain::same_origin(origins) {
        return Err(ApiError::Forbidden);
    }
    Ok(next.run(req).await)
}

/// Request id, error envelope, `x-request-id`, and one access-log line.
async fn request_context(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = req
        .headers()
        .get(domain::CLOUDFRONT_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .map_or_else(domain::new_request_id, str::to_owned);
    let method = req.method().clone();

    let mut res = next.run(req).await;
    if let Some(err) = api_error(&res) {
        res = render_error(err, &request_id);
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        res.headers_mut().insert("x-request-id", value);
    }
    tracing::info!(
        request_id,
        %method,
        status = res.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request completed"
    );
    res
}

/// The `ApiError` behind an error response: ours, or one inferred from a
/// Tako rejection (bad path/query, 405, 413, missing state).
fn api_error(res: &Response) -> Option<ApiError> {
    if let Some(err) = res.extensions().get::<ApiError>() {
        return Some(*err);
    }
    match res.status() {
        StatusCode::UNAUTHORIZED => Some(ApiError::SessionRequired),
        StatusCode::FORBIDDEN => Some(ApiError::Forbidden),
        // celeratax answers a wrong method with the same 404 as an unknown path.
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => Some(ApiError::NotFound),
        s if s.is_client_error() => Some(ApiError::BadRequest),
        s if s.is_server_error() => Some(ApiError::Internal),
        _ => None,
    }
}

/// Tags the response; `request_context` renders the envelope with the request id.
impl Responder for ApiError {
    fn into_response(self) -> Response {
        let mut res = StatusCode::from_u16(self.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        res.extensions_mut().insert(self);
        res
    }
}

fn render_error(err: ApiError, request_id: &str) -> Response {
    let mut res = err.into_response();
    *res.body_mut() = TakoBody::from(err.body(request_id));
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(scheme) = err.www_authenticate() {
        headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(scheme));
    }
    res
}

/// Adds `domain::PRIVATE_HEADERS` to an authenticated success response.
struct Private<T>(T);

impl<T: Responder> Responder for Private<T> {
    fn into_response(self) -> Response {
        let mut res = self.0.into_response();
        for (name, value) in domain::PRIVATE_HEADERS {
            res.headers_mut().insert(name, HeaderValue::from_static(value));
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_session_in_process() {
        let router = router(Store::seeded());
        let req = http::Request::get("/api/v1/session/current")
            .header("cookie", "ct_session=sess-alice")
            .body(TakoBody::empty())
            .unwrap();
        let res = router.dispatch(req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("x-request-id"));
        // Tako has no test client; reuse the body collector on a throwaway request.
        let body = collect_body(&mut Request::new(res.into_body())).await.unwrap();
        let view: SessionView = serde_json::from_slice(&body).unwrap();
        assert_eq!(view.email, "alice@example.com");
    }
}
