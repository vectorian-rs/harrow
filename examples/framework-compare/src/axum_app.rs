//! axum 0.8 implementation.
//!
//! Server setup: `axum::serve` on Tokio's default multi-threaded runtime (the
//! setup axum documents for production), `with_graceful_shutdown` for SIGTERM
//! draining, a 10 s per-request `TimeoutLayer` from tower-http, and
//! `DefaultBodyLimit` at 4 KiB. `axum::serve` deliberately exposes no
//! header-read timeout or connection cap; getting those means dropping down to
//! `hyper-util`'s builder, which is not what a typical axum service does.
//!
//! Shape: typed extractors do the work. `AuthenticatedSession` and `Admin`
//! are `FromRequestParts` extractors, `CreateCompanyJson` is a `FromRequest`
//! extractor, and `ApiError` implements `IntoResponse`. Because
//! `IntoResponse` cannot see the request, an error response only carries the
//! `ApiError` in its extensions; the `request_context` middleware renders the
//! envelope with the request id on the way out.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, FromRef, FromRequest, FromRequestParts, Path, RawQuery, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE, WWW_AUTHENTICATE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use tower_http::timeout::TimeoutLayer;

use crate::domain::{self, ApiError, AuthenticatedSession, Company, CreateCompany, SessionView, Store};

pub fn run(addr: SocketAddr, store: Store) {
    tokio::runtime::Runtime::new().expect("tokio runtime").block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(listener, app(store))
            .with_graceful_shutdown(domain::shutdown_signal())
            .await
            .expect("server");
    });
}

pub fn app(store: Store) -> Router {
    let api = Router::new()
        .route("/session/current", get(current_session))
        .route("/session/logout", post(logout))
        .route("/companies", get(list_companies).post(create_company))
        .route("/companies/{company_id}/months/{month}/state", get(month_state))
        .route("/companies/{company_id}/documents/{file_id}/content", get(document_content))
        .route("/usage", get(usage))
        .route("/admin/users", get(admin_users))
        .route_layer(middleware::from_fn(same_origin_for_posts));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api/v1", api)
        .fallback(not_found)
        // celeratax answers a wrong method with the same 404 as an unknown path.
        .method_not_allowed_fallback(not_found)
        .layer(DefaultBodyLimit::max(domain::MAX_JSON_BODY))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(10)))
        .layer(middleware::from_fn(request_context))
        .with_state(store)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

type ApiResult<T> = Result<T, ApiError>;

async fn current_session(session: AuthenticatedSession) -> impl IntoResponse {
    (domain::PRIVATE_HEADERS, Json(SessionView::from(&session)))
}

async fn list_companies(State(store): State<Store>, session: AuthenticatedSession) -> ApiResult<impl IntoResponse> {
    Ok((domain::PRIVATE_HEADERS, Json(store.companies_for(&session).await?)))
}

async fn create_company(
    State(store): State<Store>,
    session: AuthenticatedSession,
    CreateCompanyJson(input): CreateCompanyJson,
) -> ApiResult<impl IntoResponse> {
    let company: Company = store.create_company(&session, input).await?;
    Ok((StatusCode::CREATED, domain::PRIVATE_HEADERS, Json(company)))
}

async fn month_state(
    State(store): State<Store>,
    session: AuthenticatedSession,
    Path((company_id, month)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let company_id = domain::validate_company_id(&company_id)?;
    let month = domain::validate_month(&month)?;
    let company = store.company_for_user(&session, company_id).await?;
    Ok((domain::PRIVATE_HEADERS, Json(store.month_state(&company, month).await?)))
}

async fn document_content(
    State(store): State<Store>,
    session: AuthenticatedSession,
    Path((company_id, file_id)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let company_id = domain::validate_company_id(&company_id)?;
    let file_id = domain::validate_file_id(&file_id)?;
    let company = store.company_for_user(&session, company_id).await?;
    let pdf = store.document(&company, file_id).await?;
    Ok((
        [(CONTENT_TYPE, "application/pdf"), (CACHE_CONTROL, "private, no-store"), (X_CONTENT_TYPE_OPTIONS, "nosniff")],
        [(CONTENT_DISPOSITION, format!("inline; filename=\"{file_id}.pdf\""))],
        pdf,
    ))
}

/// `RawQuery` rather than `Query<T>`: the domain helper does celeratax's
/// non-decoding lookup, and `Query`'s rejection would not be our envelope.
async fn usage(
    State(store): State<Store>,
    session: AuthenticatedSession,
    RawQuery(query): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let period = domain::query_param(query.as_deref(), "period").ok_or(ApiError::BadRequest)?;
    let usage = store.usage(&session, domain::validate_month(period)?).await?;
    Ok((domain::PRIVATE_HEADERS, Json(usage)))
}

async fn logout(State(store): State<Store>, session: AuthenticatedSession) -> ApiResult<impl IntoResponse> {
    store.revoke(&session).await?;
    let cookies = AppendHeaders([(SET_COOKIE, domain::CLEAR_SESSION_COOKIE), (SET_COOKIE, domain::CLEAR_POW_COOKIE)]);
    Ok((cookies, Redirect::to("/"))) // `Redirect::to` is 303 See Other
}

async fn admin_users(State(store): State<Store>, _admin: Admin) -> ApiResult<impl IntoResponse> {
    Ok((domain::PRIVATE_HEADERS, Json(store.admin_list_users().await?)))
}

async fn not_found() -> ApiError {
    ApiError::NotFound
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

impl<S> FromRequestParts<S> for AuthenticatedSession
where
    Store: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let cookie = parts.headers.get(COOKIE).and_then(|v| v.to_str().ok());
        Store::from_ref(state).authenticate(domain::session_token(cookie)).await
    }
}

/// A session whose user is an admin. Non-admins get the same 404 as an
/// unknown route.
struct Admin;

impl<S> FromRequestParts<S> for Admin
where
    Store: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session = AuthenticatedSession::from_request_parts(parts, state).await?;
        domain::is_admin(&session.email).then_some(Admin).ok_or(ApiError::NotFound)
    }
}

/// Like `Json<CreateCompany>`, but with the domain's content-type check,
/// validation, and 400s instead of axum's 415/422 plain-text rejections.
struct CreateCompanyJson(CreateCompany);

impl<S: Send + Sync> FromRequest<S> for CreateCompanyJson {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
        if !domain::is_json_content_type(content_type) {
            return Err(ApiError::BadRequest);
        }
        // Over `DefaultBodyLimit` this is a `LengthLimitError`; the contract
        // allows 400 for that, so every body failure is a plain bad request.
        let body = Bytes::from_request(req, state).await.map_err(|_| ApiError::BadRequest)?;
        domain::parse_create_company(&body).map(Self)
    }
}

// ---------------------------------------------------------------------------
// Errors and middleware
// ---------------------------------------------------------------------------

impl IntoResponse for ApiError {
    /// Status only; `request_context` adds the envelope, which needs the
    /// request id that `IntoResponse` has no access to.
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut res = status.into_response();
        res.extensions_mut().insert(self);
        res
    }
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
    if let Some(err) = res.extensions_mut().remove::<ApiError>() {
        res = render_error(err, &request_id, res.status());
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

fn render_error(err: ApiError, request_id: &str, status: StatusCode) -> Response {
    let mut res = (
        status,
        [(CONTENT_TYPE, "application/json"), (CACHE_CONTROL, "no-store")],
        err.body(request_id),
    )
        .into_response();
    if let Some(scheme) = err.www_authenticate() {
        res.headers_mut().insert(WWW_AUTHENTICATE, HeaderValue::from_static(scheme));
    }
    res
}

async fn same_origin_for_posts(req: Request, next: Next) -> Response {
    let origins = req.headers().get_all(ORIGIN).iter().map(HeaderValue::as_bytes);
    if req.method() == Method::POST && !domain::same_origin(origins) {
        return ApiError::Forbidden.into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn current_session_in_process() {
        let req = Request::get("/api/v1/session/current")
            .header(COOKIE, "ct_session=sess-alice")
            .body(Body::empty())
            .unwrap();
        let res = app(Store::seeded()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("x-request-id"));
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let view: SessionView = serde_json::from_slice(&body).unwrap();
        assert_eq!(view.email, "alice@example.com");
    }
}
