//! ntex 4 implementation.
//!
//! Server setup: `web::server_with_config` with ntex's thread-per-core model
//! (one worker per CPU, each with its own single-threaded runtime and its own
//! copy of the `Api` state; the `Store` inside is `Arc`-backed, so all
//! workers share one store). Per worker: at most 1024 connections, request
//! heads must arrive within 5 s, 5 s keep-alive, 10 s write timeout, and a
//! 4 KiB body limit for the `Bytes` extractor. ntex's own signal handling is
//! disabled because it stops immediately on SIGINT; instead SIGTERM and
//! Ctrl-C both trigger `Server::stop(true)`, which drains workers for up to
//! 30 s.
//!
//! Shape: the application state `Api` declares its own error domain
//! (`Envelope`), so the compiler forces every ntex extractor error that can
//! reach a client to be mapped into `ApiError`. Filters change the
//! request-state type: `/api/v1` handlers receive an `AuthenticatedSession`
//! and the admin handler receives an `Admin`, so an unauthenticated route
//! cannot be wired to a handler that needs a session.

use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::{HttpServiceConfig, Method, Request, Response, StatusCode};
use ntex::service::{Ctx, Middleware, Service, ServiceFactory};
use ntex::util::Bytes;
use ntex::web::error::{PathError, PayloadError};
use ntex::web::types::{Path, PayloadConfig};
use ntex::web::{
    self, HttpRequest, HttpResponse, HttpResponseBuilder, WebAppConfig, WebError, WebRequest,
    WebResponseError,
};
use ntex::{SharedCfg, error::Failure, time::Seconds};

use crate::domain::{self, ApiError, AuthenticatedSession, SessionView, Store};

pub fn run(addr: SocketAddr, store: Store) {
    serve(addr, store).expect("ntex server");
}

#[ntex::main(name = "celeratax-ntex")]
async fn serve(addr: SocketAddr, store: Store) -> io::Result<()> {
    let http = HttpServiceConfig::new()
        .set_headers_read_rate(Seconds(1), Seconds(5), 256)
        .set_keepalive_timeout(Seconds(5))
        .set_write_timeout(Seconds(10));

    let server = web::server_with_config(
        async move || Ok::<_, io::Error>(Api { store: store.clone() }),
        async |_: &Api| app(),
    )
    .max_connections(1024)
    .graceful_shutdown_timeout(Seconds(30))
    .disable_signals()
    .bind(addr, SharedCfg::new("celeratax").add(http))?
    .run();

    let handle = server.clone();
    ntex::rt::spawn(async move {
        domain::shutdown_signal().await;
        handle.stop(true).await;
    });
    server.await
}

/// The application, without server or state; shared by `serve` and tests.
pub fn app() -> impl ServiceFactory<Api, Request, Res = Response, Error = WebError<Api, Envelope>, InitError = Failure>
{
    let app = web::App::<Api>::new()
        .with_config(WebAppConfig::new().set_state(PayloadConfig::new(domain::MAX_JSON_BODY)))
        .route("/healthz", web::get().to(async || "ok"))
        .service(
            web::scope("/api/v1")
                .filter(same_origin)
                .filter(authenticate)
                .route("/session/current", web::get().to_with_state(current_session))
                .route("/session/logout", web::post().to_with_state(logout))
                .route("/companies", web::get().to_with_state(list_companies))
                .route("/companies", web::post().to_with_state(create_company))
                .route("/companies/{company_id}/months/{month}/state", web::get().to_with_state(month_state))
                .route("/companies/{company_id}/documents/{file_id}/content", web::get().to_with_state(document))
                .route("/usage", web::get().to_with_state(usage))
                .service(
                    web::resource("/admin/users")
                        .guard(web::guard::Get())
                        .filter(require_admin)
                        .to_with_state(admin_users),
                )
                // Scopes do not fall back to the app's default service.
                .default_service(web::to(not_found)),
        )
        // Routes registered with `.route()` carry their method as a resource
        // guard, so a wrong method lands here too: 404, like celeratax.
        .default_service(web::to(not_found))
        .build();
    RequestContext.apply_to(app)
}

// ---------------------------------------------------------------------------
// State and errors
// ---------------------------------------------------------------------------

/// Per-worker application state.
#[derive(Clone)]
pub struct Api {
    store: Store,
}

impl web::State for Api {
    type Error = Envelope;
}

/// Error domain of this app: every error renders as the `ApiError` envelope.
/// Uninhabited; it only selects `WebResponseError` impls.
#[derive(Debug)]
pub enum Envelope {}

/// `ApiError` as an ntex error. `ApiError` has no `std::error::Error` impl,
/// so it is wrapped here instead of changing `domain.rs`.
#[derive(Debug, Clone, Copy)]
pub struct Error(ApiError);

impl From<ApiError> for Error {
    fn from(err: ApiError) -> Self {
        Self(err)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.code())
    }
}

impl std::error::Error for Error {}

/// ntex renders errors from the app state alone, without the request, so the
/// request id is not known here. The response carries the `ApiError` and
/// `RequestContext` renders the envelope once it can add the id.
impl WebResponseError<Api, Envelope> for Error {
    fn error_response(&self, _: &Api) -> HttpResponse {
        let res = HttpResponse::new(status(self.0));
        res.extensions_mut().insert(self.0);
        res
    }
}

/// Body read failures, including bodies over `MAX_JSON_BODY`.
impl WebResponseError<Api, Envelope> for PayloadError {
    fn error_response(&self, st: &Api) -> HttpResponse {
        Error(ApiError::BadRequest).error_response(st)
    }
}

impl WebResponseError<Api, Envelope> for PathError {
    fn error_response(&self, st: &Api) -> HttpResponse {
        Error(ApiError::BadRequest).error_response(st)
    }
}

// ---------------------------------------------------------------------------
// Handlers: `(&Api, request state, extractors...)`
// ---------------------------------------------------------------------------

type Reply = Result<HttpResponse, Error>;

async fn current_session(_: &Api, session: AuthenticatedSession) -> HttpResponse {
    private(StatusCode::OK).json(&SessionView::from(&session))
}

async fn list_companies(api: &Api, session: AuthenticatedSession) -> Reply {
    Ok(private(StatusCode::OK).json(&api.store.companies_for(&session).await?))
}

async fn create_company(api: &Api, session: AuthenticatedSession, req: HttpRequest, body: Bytes) -> Reply {
    if !domain::is_json_content_type(header_str(req.headers().get(header::CONTENT_TYPE))) {
        return Err(ApiError::BadRequest.into());
    }
    let input = domain::parse_create_company(&body)?;
    let company = api.store.create_company(&session, input).await?;
    Ok(private(StatusCode::CREATED).json(&company))
}

async fn month_state(api: &Api, session: AuthenticatedSession, path: Path<(String, String)>) -> Reply {
    let (company_id, month) = path.into_inner();
    let company_id = domain::validate_company_id(&company_id)?;
    let month = domain::validate_month(&month)?;
    let company = api.store.company_for_user(&session, company_id).await?;
    Ok(private(StatusCode::OK).json(&api.store.month_state(&company, month).await?))
}

async fn document(api: &Api, session: AuthenticatedSession, path: Path<(String, String)>) -> Reply {
    let (company_id, file_id) = path.into_inner();
    let company_id = domain::validate_company_id(&company_id)?;
    let file_id = domain::validate_file_id(&file_id)?;
    let company = api.store.company_for_user(&session, company_id).await?;
    let pdf = api.store.document(&company, file_id).await?;
    Ok(HttpResponse::Ok()
        .content_type("application/pdf")
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{file_id}.pdf\""))
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(pdf))
}

async fn usage(api: &Api, session: AuthenticatedSession, req: HttpRequest) -> Reply {
    let query = req.uri().query().map(|q| q.as_str());
    let period = domain::query_param(query, "period").ok_or(ApiError::BadRequest)?;
    let usage = api.store.usage(&session, domain::validate_month(period)?).await?;
    Ok(private(StatusCode::OK).json(&usage))
}

async fn logout(api: &Api, session: AuthenticatedSession) -> Reply {
    api.store.revoke(&session).await?;
    Ok(HttpResponse::SeeOther()
        .header(header::LOCATION, "/")
        .header(header::SET_COOKIE, domain::CLEAR_SESSION_COOKIE)
        .header(header::SET_COOKIE, domain::CLEAR_POW_COOKIE)
        .build())
}

async fn admin_users(api: &Api, _: Admin) -> Reply {
    Ok(private(StatusCode::OK).json(&api.store.admin_list_users().await?))
}

async fn not_found() -> Reply {
    Err(ApiError::NotFound.into())
}

// ---------------------------------------------------------------------------
// Filters: run after routing picks the scope, may change the request state
// ---------------------------------------------------------------------------

/// Every POST must carry exactly one `Origin` equal to the app's origin.
async fn same_origin(req: WebRequest<()>) -> Result<WebRequest<()>, Error> {
    let origins = req.headers().get_all(header::ORIGIN).map(HeaderValue::as_bytes);
    if req.method() == Method::POST && !domain::same_origin(origins) {
        return Err(ApiError::Forbidden.into());
    }
    Ok(req)
}

/// `WebRequest<()>` -> `WebRequest<AuthenticatedSession>`.
async fn authenticate(api: &Api, req: WebRequest<()>) -> Result<WebRequest<AuthenticatedSession>, Error> {
    let token = domain::session_token(header_str(req.headers().get(header::COOKIE)));
    let session = api.store.authenticate(token).await?;
    Ok(req.map_state(|()| session))
}

/// Proof that the session belongs to an admin.
struct Admin(#[allow(dead_code)] AuthenticatedSession);

/// Non-admins get the same 404 as an unknown route.
async fn require_admin(req: WebRequest<AuthenticatedSession>) -> Result<WebRequest<Admin>, Error> {
    if !domain::is_admin(&req.st().email) {
        return Err(ApiError::NotFound.into());
    }
    Ok(req.map_state(Admin))
}

// ---------------------------------------------------------------------------
// Middleware: request id, error envelope, access log
// ---------------------------------------------------------------------------

/// Wraps the built app at the `Request -> Response` level rather than via
/// `App::middleware`. Web middleware sees a failed filter as an `Err` without
/// the request, so it can neither build a response for it nor log it (ntex's
/// own `Logger` skips such requests). Out here every outcome, including
/// filter errors, is already a `Response`.
pub struct RequestContext;

impl<S> Middleware<S, Api> for RequestContext {
    type Service = RequestContextService<S>;

    fn create(&self, _: &Api, service: S) -> Self::Service {
        RequestContextService { service }
    }
}

pub struct RequestContextService<S> {
    service: S,
}

impl<S> Service<Api, Request> for RequestContextService<S>
where
    S: Service<Api, Request, Res = Response>,
{
    type Res = Response;
    type Error = S::Error;

    ntex::forward_ready!(Api, service);
    ntex::forward_shutdown!(Api, service);

    async fn call(&self, req: Request, ctx: Ctx<'_, Self, Api>) -> Result<Response, S::Error> {
        let started = Instant::now();
        let request_id = header_str(req.headers().get(domain::CLOUDFRONT_REQUEST_ID))
            .map_or_else(domain::new_request_id, str::to_owned);
        let method = req.method().clone();

        let mut res = ctx.call(&self.service, req).await?;
        let error = res.extensions().get::<ApiError>().copied();
        if let Some(error) = error {
            res = render_error(error, &request_id);
        }
        if let Ok(value) = HeaderValue::try_from(request_id.as_str()) {
            res.headers_mut().insert(HeaderName::from_static("x-request-id"), value);
        }

        tracing::info!(
            request_id,
            %method,
            status = res.status().as_u16(),
            latency_ms = started.elapsed().as_millis() as u64,
            "request completed"
        );
        Ok(res)
    }
}

// ---------------------------------------------------------------------------
// Glue
// ---------------------------------------------------------------------------

fn render_error(err: ApiError, request_id: &str) -> HttpResponse {
    let mut res = HttpResponse::builder(status(err));
    res.content_type("application/json").header(header::CACHE_CONTROL, "no-store");
    if let Some(scheme) = err.www_authenticate() {
        res.header(header::WWW_AUTHENTICATE, scheme);
    }
    res.body(err.body(request_id))
}

fn status(err: ApiError) -> StatusCode {
    StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn private(status: StatusCode) -> HttpResponseBuilder {
    let mut res = HttpResponse::builder(status);
    for (name, value) in domain::PRIVATE_HEADERS {
        res.header(name, value);
    }
    res
}

fn header_str(value: Option<&HeaderValue>) -> Option<&str> {
    value.and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use ntex::web::test::{self, TestRequest};

    use super::*;

    #[ntex::test]
    async fn current_session_in_process() {
        let app = test::init_service_st(Api { store: Store::seeded() }, app()).await;
        let req = TestRequest::with_uri("/api/v1/session/current")
            .header(header::COOKIE, "ct_session=sess-alice")
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("x-request-id"));
        let body: SessionView = serde_json::from_slice(&test::read_body(res).await).unwrap();
        assert_eq!(body.email, "alice@example.com");
    }
}
