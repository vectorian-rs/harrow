//! Framework-neutral business code shared by all four implementations.
//!
//! Nothing here knows about any web framework. Header access goes through
//! plain `&str` / `&[u8]` so ntex (own header types) and the `http`-crate
//! frameworks can all call it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub const EXPECTED_ORIGIN: &str = "https://app.celeratax.example";
pub const SESSION_COOKIE: &str = "ct_session";
pub const MAX_JSON_BODY: usize = 4 * 1024;
pub const CLOUDFRONT_REQUEST_ID: &str = "cloudfront-viewer-http-request-id";

pub const CLEAR_SESSION_COOKIE: &str =
    "ct_session=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0";
pub const CLEAR_POW_COOKIE: &str = "ct_pow=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0";

/// Headers for authenticated success responses.
pub const PRIVATE_HEADERS: [(&str, &str); 2] = [("cache-control", "no-store"), ("vary", "cookie")];

const ADMINS: [&str; 1] = ["alice@example.com"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiError {
    BadRequest,
    SessionRequired,
    Forbidden,
    NotFound,
    Internal,
}

impl ApiError {
    pub fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::SessionRequired => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Internal => 500,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::SessionRequired => "session_required",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Internal => "internal",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::BadRequest => "Invalid request.",
            Self::SessionRequired => "Sign in again.",
            Self::Forbidden => "Not allowed.",
            Self::NotFound => "Not found.",
            Self::Internal => "Something went wrong.",
        }
    }

    pub fn www_authenticate(self) -> Option<&'static str> {
        match self {
            Self::SessionRequired => Some("DD-Session"),
            _ => None,
        }
    }

    /// `{"error":{"code":..,"message":..,"request_id":..}}`
    pub fn body(self, request_id: &str) -> String {
        serde_json::json!({
            "error": { "code": self.code(), "message": self.message(), "request_id": request_id }
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

pub fn new_request_id() -> String {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("os rng");
    let mut id = String::with_capacity(20);
    id.push_str("req_");
    for b in buf {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

/// Value of the `ct_session` cookie from a `Cookie` header, if present.
pub fn session_token(cookie_header: Option<&str>) -> Option<&str> {
    cookie_header?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
        .filter(|v| !v.is_empty())
}

/// Exactly one `Origin` header, equal to [`EXPECTED_ORIGIN`].
pub fn same_origin<'a>(mut origins: impl Iterator<Item = &'a [u8]>) -> bool {
    match (origins.next(), origins.next()) {
        (Some(origin), None) => origin == EXPECTED_ORIGIN.as_bytes(),
        _ => false,
    }
}

/// Raw query-string lookup, matching celeratax (no percent-decoding).
pub fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

pub fn is_json_content_type(value: Option<&str>) -> bool {
    value
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

pub fn validate_company_id(id: &str) -> Result<&str, ApiError> {
    let ok = id.strip_prefix("cmp-").is_some_and(|rest| {
        (1..=32).contains(&rest.len())
            && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    ok.then_some(id).ok_or(ApiError::BadRequest)
}

pub fn validate_month(month: &str) -> Result<&str, ApiError> {
    let b = month.as_bytes();
    let ok = b.len() == 7
        && b[4] == b'-'
        && b[..4].iter().chain(&b[5..]).all(u8::is_ascii_digit)
        && matches!(&month[5..], "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12");
    ok.then_some(month).ok_or(ApiError::BadRequest)
}

pub fn validate_file_id(id: &str) -> Result<&str, ApiError> {
    let ok = id
        .strip_prefix("doc-")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    ok.then_some(id).ok_or(ApiError::BadRequest)
}

pub fn is_admin(email: &str) -> bool {
    ADMINS.contains(&email)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub user_id: String,
    pub email: String,
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionView {
    pub user_id: String,
    pub email: String,
}

impl From<&AuthenticatedSession> for SessionView {
    fn from(s: &AuthenticatedSession) -> Self {
        Self { user_id: s.user_id.clone(), email: s.email.clone() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Company {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCompany {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonthState {
    pub company_id: String,
    pub month: String,
    pub state: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub period: String,
    pub documents: u32,
}

/// Parse and validate a JSON create-company body.
pub fn parse_create_company(body: &[u8]) -> Result<CreateCompany, ApiError> {
    if body.len() > MAX_JSON_BODY {
        return Err(ApiError::BadRequest);
    }
    let input: CreateCompany = serde_json::from_slice(body).map_err(|_| ApiError::BadRequest)?;
    let len = input.name.trim().chars().count();
    if !(1..=80).contains(&len) {
        return Err(ApiError::BadRequest);
    }
    Ok(input)
}

// ---------------------------------------------------------------------------
// Fake store (stands in for DynamoDB / S3)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, (String, String)>,
    revoked: HashSet<String>,
    companies: Vec<(Company, String)>,
    next_company: u32,
}

impl Store {
    /// Seeded store: `sess-alice` (admin, owns `cmp-acme`) and `sess-bob`.
    pub fn seeded() -> Self {
        let store = Self::default();
        {
            let mut inner = store.inner.lock().unwrap();
            inner.sessions.insert("sess-alice".into(), ("usr-alice".into(), "alice@example.com".into()));
            inner.sessions.insert("sess-bob".into(), ("usr-bob".into(), "bob@example.com".into()));
            inner.companies.push((Company { id: "cmp-acme".into(), name: "Acme Kft.".into() }, "usr-alice".into()));
        }
        store
    }

    pub async fn authenticate(&self, token: Option<&str>) -> Result<AuthenticatedSession, ApiError> {
        let token = token.ok_or(ApiError::SessionRequired)?;
        let inner = self.inner.lock().map_err(|_| ApiError::Internal)?;
        if inner.revoked.contains(token) {
            return Err(ApiError::SessionRequired);
        }
        let (user_id, email) = inner.sessions.get(token).ok_or(ApiError::SessionRequired)?;
        Ok(AuthenticatedSession { user_id: user_id.clone(), email: email.clone(), token: token.to_owned() })
    }

    pub async fn revoke(&self, session: &AuthenticatedSession) -> Result<(), ApiError> {
        self.inner.lock().map_err(|_| ApiError::Internal)?.revoked.insert(session.token.clone());
        Ok(())
    }

    pub async fn companies_for(&self, session: &AuthenticatedSession) -> Result<Vec<Company>, ApiError> {
        let inner = self.inner.lock().map_err(|_| ApiError::Internal)?;
        Ok(inner.companies.iter().filter(|(_, owner)| *owner == session.user_id).map(|(c, _)| c.clone()).collect())
    }

    pub async fn create_company(&self, session: &AuthenticatedSession, input: CreateCompany) -> Result<Company, ApiError> {
        let mut inner = self.inner.lock().map_err(|_| ApiError::Internal)?;
        inner.next_company += 1;
        let company = Company { id: format!("cmp-new-{}", inner.next_company), name: input.name.trim().to_owned() };
        inner.companies.push((company.clone(), session.user_id.clone()));
        Ok(company)
    }

    pub async fn company_for_user(&self, session: &AuthenticatedSession, company_id: &str) -> Result<Company, ApiError> {
        let inner = self.inner.lock().map_err(|_| ApiError::Internal)?;
        inner
            .companies
            .iter()
            .find(|(c, owner)| c.id == company_id && *owner == session.user_id)
            .map(|(c, _)| c.clone())
            .ok_or(ApiError::NotFound)
    }

    pub async fn month_state(&self, company: &Company, month: &str) -> Result<MonthState, ApiError> {
        Ok(MonthState { company_id: company.id.clone(), month: month.to_owned(), state: "open".into() })
    }

    pub async fn document(&self, _company: &Company, file_id: &str) -> Result<Vec<u8>, ApiError> {
        Ok(format!("%PDF-1.4\n% {file_id}\n%%EOF\n").into_bytes())
    }

    pub async fn usage(&self, _session: &AuthenticatedSession, period: &str) -> Result<Usage, ApiError> {
        Ok(Usage { period: period.to_owned(), documents: 3 })
    }

    pub async fn admin_list_users(&self) -> Result<Vec<String>, ApiError> {
        let inner = self.inner.lock().map_err(|_| ApiError::Internal)?;
        let mut emails: Vec<String> = inner.sessions.values().map(|(_, e)| e.clone()).collect();
        emails.sort();
        Ok(emails)
    }
}

/// Resolve on SIGTERM or Ctrl-C.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validators() {
        assert!(validate_month("2026-10").is_ok());
        assert!(validate_month("2026-13").is_err());
        assert!(validate_company_id("cmp-acme").is_ok());
        assert!(validate_company_id("acme").is_err());
        assert!(validate_file_id(&format!("doc-{}", "a".repeat(64))).is_ok());
        assert!(validate_file_id("doc-xyz").is_err());
    }

    #[test]
    fn cookie_and_origin() {
        assert_eq!(session_token(Some("a=1; ct_session=sess-alice")), Some("sess-alice"));
        assert_eq!(session_token(Some("ct_session=")), None);
        assert!(same_origin([EXPECTED_ORIGIN.as_bytes()].into_iter()));
        assert!(!same_origin([EXPECTED_ORIGIN.as_bytes(), EXPECTED_ORIGIN.as_bytes()].into_iter()));
        assert!(!same_origin(std::iter::empty()));
    }
}
