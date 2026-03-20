use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use harrow_core::middleware::{Middleware, Next};
use harrow_core::request::Request;
use harrow_core::response::Response;

// ---------------------------------------------------------------------------
// SessionError
// ---------------------------------------------------------------------------

/// Reason a session cookie was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Cookie value has no `.` separator.
    MalformedCookie,
    /// Session ID is not exactly 32 hex characters.
    InvalidIdLength,
    /// MAC is not exactly 64 hex characters.
    InvalidMacLength,
    /// MAC hex could not be decoded.
    InvalidMacEncoding,
    /// MAC does not match the session ID (tampered or wrong secret).
    MacMismatch,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::MalformedCookie => write!(f, "cookie value has no `.` separator"),
            SessionError::InvalidIdLength => {
                write!(f, "session ID is not exactly 32 hex characters")
            }
            SessionError::InvalidMacLength => {
                write!(f, "MAC is not exactly 64 hex characters")
            }
            SessionError::InvalidMacEncoding => write!(f, "MAC hex could not be decoded"),
            SessionError::MacMismatch => write!(f, "MAC does not match the session ID"),
        }
    }
}

impl std::error::Error for SessionError {}

// ---------------------------------------------------------------------------
// Session type
// ---------------------------------------------------------------------------

/// Server-side session data attached to a request via middleware.
#[derive(Clone)]
pub struct Session {
    id: Option<String>,
    data: Arc<RwLock<HashMap<String, String>>>,
    modified: Arc<AtomicBool>,
    destroyed: Arc<AtomicBool>,
}

impl Session {
    fn new(id: Option<String>, data: HashMap<String, String>) -> Self {
        Self {
            id,
            data: Arc::new(RwLock::new(data)),
            modified: Arc::new(AtomicBool::new(false)),
            destroyed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn empty() -> Self {
        Self::new(None, HashMap::new())
    }

    /// Return the session ID, if this session was loaded from a cookie.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Get a session value by key.
    pub fn get(&self, key: &str) -> Option<String> {
        self.data.read().unwrap().get(key).cloned()
    }

    /// Set a session value. Marks the session as modified.
    pub fn set(&self, key: &str, value: &str) {
        self.data
            .write()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        self.modified.store(true, Ordering::Relaxed);
    }

    /// Remove a session value by key. Return the old value if present.
    pub fn remove(&self, key: &str) -> Option<String> {
        let removed = self.data.write().unwrap().remove(key);
        if removed.is_some() {
            self.modified.store(true, Ordering::Relaxed);
        }
        removed
    }

    /// Remove all session data. Mark as modified if non-empty.
    pub fn clear(&self) {
        let mut data = self.data.write().unwrap();
        if !data.is_empty() {
            data.clear();
            self.modified.store(true, Ordering::Relaxed);
        }
    }

    /// Mark the session for destruction. The cookie will be cleared and
    /// server-side data removed.
    pub fn destroy(&self) {
        self.destroyed.store(true, Ordering::Relaxed);
        self.modified.store(true, Ordering::Relaxed);
    }

    fn is_modified(&self) -> bool {
        self.modified.load(Ordering::Relaxed)
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Relaxed)
    }

    fn is_empty(&self) -> bool {
        self.data.read().unwrap().is_empty()
    }

    fn data_snapshot(&self) -> HashMap<String, String> {
        self.data.read().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// SessionStore trait
// ---------------------------------------------------------------------------

/// Backend storage for session data.
///
/// Implement this trait for custom stores (Redis, database, etc.).
pub trait SessionStore: Send + Sync + 'static {
    /// Load session data by ID. Return `None` if not found or expired.
    fn load(&self, id: &str) -> impl Future<Output = Option<HashMap<String, String>>> + Send;
    /// Save session data with the given TTL.
    fn save(
        &self,
        id: &str,
        data: &HashMap<String, String>,
        ttl: Duration,
    ) -> impl Future<Output = ()> + Send;
    /// Remove a session by ID.
    fn remove(&self, id: &str) -> impl Future<Output = ()> + Send;
}

// ---------------------------------------------------------------------------
// InMemorySessionStore
// ---------------------------------------------------------------------------

struct SessionEntry {
    data: HashMap<String, String>,
    expires_at: Instant,
}

/// In-memory session store using `DashMap`. Suitable for single-instance
/// deployments.
pub struct InMemorySessionStore {
    sessions: Arc<DashMap<String, SessionEntry>>,
}

impl InMemorySessionStore {
    /// Create an empty in-memory session store.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    /// Spawn a background task that evicts expired sessions at `interval`.
    pub fn start_sweeper(&self, interval: Duration) {
        let sessions = Arc::clone(&self.sessions);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let now = Instant::now();
                sessions.retain(|_, entry| entry.expires_at > now);
            }
        });
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn load(&self, id: &str) -> impl Future<Output = Option<HashMap<String, String>>> + Send {
        let result = self.sessions.get(id).and_then(|entry| {
            if entry.expires_at > Instant::now() {
                Some(entry.data.clone())
            } else {
                None
            }
        });
        // Remove expired entry eagerly
        if result.is_none() {
            self.sessions.remove(id);
        }
        std::future::ready(result)
    }

    fn save(
        &self,
        id: &str,
        data: &HashMap<String, String>,
        ttl: Duration,
    ) -> impl Future<Output = ()> + Send {
        self.sessions.insert(
            id.to_string(),
            SessionEntry {
                data: data.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
        std::future::ready(())
    }

    fn remove(&self, id: &str) -> impl Future<Output = ()> + Send {
        self.sessions.remove(id);
        std::future::ready(())
    }
}

// ---------------------------------------------------------------------------
// SameSite
// ---------------------------------------------------------------------------

/// The `SameSite` cookie attribute.
#[derive(Clone, Copy)]
pub enum SameSite {
    /// Cookie is sent only for same-site requests.
    Strict,
    /// Cookie is sent for same-site requests and top-level navigations.
    Lax,
    /// Cookie is always sent (requires `Secure`).
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

// ---------------------------------------------------------------------------
// SessionConfig
// ---------------------------------------------------------------------------

/// Configuration for the session middleware. Uses builder pattern.
pub struct SessionConfig {
    secret: [u8; 32],
    cookie_name: String,
    ttl: Duration,
    path: String,
    domain: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
}

impl SessionConfig {
    /// Create a session config with the given 32-byte secret key.
    ///
    /// Defaults: cookie `sid`, TTL 24h, `Secure`, `HttpOnly`, `SameSite=Lax`.
    pub fn new(secret: [u8; 32]) -> Self {
        Self {
            secret,
            cookie_name: "sid".to_string(),
            ttl: Duration::from_secs(86400),
            path: "/".to_string(),
            domain: None,
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
        }
    }

    /// Set the cookie name.
    pub fn cookie_name(mut self, name: &str) -> Self {
        self.cookie_name = name.to_string();
        self
    }

    /// Set the session TTL.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Set the cookie path.
    pub fn path(mut self, path: &str) -> Self {
        self.path = path.to_string();
        self
    }

    /// Set the cookie domain.
    pub fn domain(mut self, domain: &str) -> Self {
        self.domain = Some(domain.to_string());
        self
    }

    /// Set the `Secure` cookie attribute.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Set the `HttpOnly` cookie attribute.
    pub fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    /// Set the `SameSite` cookie attribute.
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = same_site;
        self
    }
}

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

fn generate_session_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("getrandom failed");
    hex_encode(&buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize]);
        s.push(HEX_CHARS[(b & 0x0f) as usize]);
    }
    s
}

const HEX_CHARS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

fn compute_mac(secret: &[u8; 32], session_id: &str) -> String {
    let hash = blake3::keyed_hash(secret, session_id.as_bytes());
    hash.to_hex().to_string()
}

fn verify_cookie(secret: &[u8; 32], cookie_value: &str) -> Result<String, SessionError> {
    let dot_pos = cookie_value
        .find('.')
        .ok_or(SessionError::MalformedCookie)?;
    let session_id = &cookie_value[..dot_pos];
    let mac_hex = &cookie_value[dot_pos + 1..];

    // Session ID must be exactly 32 hex chars (16 bytes)
    if session_id.len() != 32 {
        return Err(SessionError::InvalidIdLength);
    }
    // MAC must be exactly 64 hex chars (32 bytes)
    if mac_hex.len() != 64 {
        return Err(SessionError::InvalidMacLength);
    }

    let expected = blake3::keyed_hash(secret, session_id.as_bytes());
    let got = blake3::Hash::from_hex(mac_hex).map_err(|_| SessionError::InvalidMacEncoding)?;

    // blake3::Hash PartialEq is constant-time
    if expected == got {
        Ok(session_id.to_string())
    } else {
        Err(SessionError::MacMismatch)
    }
}

fn build_set_cookie(config: &SessionConfig, session_id: &str) -> String {
    let mac = compute_mac(&config.secret, session_id);
    let mut cookie = format!(
        "{}={}.{}; Path={}; Max-Age={}; SameSite={}",
        config.cookie_name,
        session_id,
        mac,
        config.path,
        config.ttl.as_secs(),
        config.same_site.as_str(),
    );
    if let Some(ref domain) = config.domain {
        cookie.push_str(&format!("; Domain={domain}"));
    }
    if config.http_only {
        cookie.push_str("; HttpOnly");
    }
    if config.secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn build_clear_cookie(config: &SessionConfig) -> String {
    let mut cookie = format!(
        "{}=; Path={}; Max-Age=0; SameSite={}",
        config.cookie_name,
        config.path,
        config.same_site.as_str(),
    );
    if let Some(ref domain) = config.domain {
        cookie.push_str(&format!("; Domain={domain}"));
    }
    if config.http_only {
        cookie.push_str("; HttpOnly");
    }
    if config.secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn extract_cookie<'a>(headers: &'a http::HeaderMap, cookie_name: &str) -> Option<&'a str> {
    for value in headers.get_all(http::header::COOKIE) {
        let value_str = value.to_str().ok()?;
        for pair in value_str.split(';') {
            let pair = pair.trim();
            if let Some(val) = pair.strip_prefix(cookie_name) {
                let val = val.trim_start();
                if let Some(val) = val.strip_prefix('=') {
                    return Some(val.trim());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SessionMiddleware
// ---------------------------------------------------------------------------

/// Middleware that manages cookie-based sessions with server-side storage.
pub struct SessionMiddleware<S: SessionStore> {
    store: Arc<S>,
    config: Arc<SessionConfig>,
}

/// Create a session middleware with the given store and config.
pub fn session_middleware<S: SessionStore>(
    store: S,
    config: SessionConfig,
) -> SessionMiddleware<S> {
    SessionMiddleware {
        store: Arc::new(store),
        config: Arc::new(config),
    }
}

impl<S: SessionStore> Middleware for SessionMiddleware<S> {
    fn call(&self, mut req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let store = Arc::clone(&self.store);
        let config = Arc::clone(&self.config);

        // Extract and verify cookie
        let cookie_value = extract_cookie(req.headers(), &config.cookie_name)
            .and_then(|v| verify_cookie(&config.secret, v).ok());

        Box::pin(async move {
            // Load or create session
            let (session_id, data) = match cookie_value {
                Some(ref id) => {
                    let data = store.load(id).await.unwrap_or_default();
                    (Some(id.clone()), data)
                }
                None => (None, HashMap::new()),
            };

            let had_cookie = session_id.is_some();
            let session = Session::new(session_id, data);
            req.set_ext(session.clone());

            let resp = next.run(req).await;

            // Post-handler: persist/clear based on session state
            if session.is_destroyed() {
                // Remove from store if it existed
                if let Some(ref id) = session.id {
                    store.remove(id).await;
                }
                // Clear the cookie
                return resp.append_header("set-cookie", &build_clear_cookie(&config));
            }

            if !session.is_modified() {
                // Unmodified session: no cookie, no save
                return resp;
            }

            if session.is_empty() {
                // Modified but empty: remove from store and clear cookie if it existed
                if let Some(ref id) = session.id {
                    store.remove(id).await;
                }
                if had_cookie {
                    return resp.append_header("set-cookie", &build_clear_cookie(&config));
                }
                return resp;
            }

            // Modified with data: save and set cookie
            let id = session.id.clone().unwrap_or_else(generate_session_id);
            let data = session.data_snapshot();
            store.save(&id, &data, config.ttl).await;
            resp.append_header("set-cookie", &build_set_cookie(&config, &id))
        })
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    fn test_secret() -> [u8; 32] {
        *b"test-secret-key-for-harrow-sess!"
    }

    fn make_request(headers: &[(&str, &str)]) -> Request {
        let mut builder = http::Request::builder().method("GET").uri("/");
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
        Next::new(|_req| Box::pin(async { Response::ok() }))
    }

    fn make_valid_cookie(secret: &[u8; 32], session_id: &str) -> String {
        let mac = compute_mac(secret, session_id);
        format!("{session_id}.{mac}")
    }

    // -- Cookie helper tests -------------------------------------------------

    #[test]
    fn generate_session_id_length_and_hex() {
        let id = generate_session_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_session_id_uniqueness() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn compute_mac_deterministic() {
        let secret = test_secret();
        let mac1 = compute_mac(&secret, "abcdef0123456789abcdef0123456789");
        let mac2 = compute_mac(&secret, "abcdef0123456789abcdef0123456789");
        assert_eq!(mac1, mac2);
        assert_eq!(mac1.len(), 64);
    }

    #[test]
    fn compute_mac_varies_with_input() {
        let secret = test_secret();
        let mac1 = compute_mac(&secret, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mac2 = compute_mac(&secret, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_ne!(mac1, mac2);
    }

    #[test]
    fn verify_cookie_valid() {
        let secret = test_secret();
        let id = "abcdef0123456789abcdef0123456789";
        let cookie_val = make_valid_cookie(&secret, id);
        assert_eq!(verify_cookie(&secret, &cookie_val), Ok(id.to_string()));
    }

    #[test]
    fn verify_cookie_tampered_mac() {
        let secret = test_secret();
        let id = "abcdef0123456789abcdef0123456789";
        let mut cookie_val = make_valid_cookie(&secret, id);
        // Flip a character in the MAC portion
        let len = cookie_val.len();
        unsafe {
            cookie_val.as_bytes_mut()[len - 1] = b'0';
        }
        // May or may not match depending on original last char;
        // use a definitely-wrong MAC
        let tampered = format!("{id}.{}", "0".repeat(64));
        assert_eq!(
            verify_cookie(&secret, &tampered),
            Err(SessionError::MacMismatch)
        );
    }

    #[test]
    fn verify_cookie_tampered_id() {
        let secret = test_secret();
        let id = "abcdef0123456789abcdef0123456789";
        let mac = compute_mac(&secret, id);
        let tampered_id = "00000000000000000000000000000000";
        let cookie_val = format!("{tampered_id}.{mac}");
        assert_eq!(
            verify_cookie(&secret, &cookie_val),
            Err(SessionError::MacMismatch)
        );
    }

    #[test]
    fn verify_cookie_wrong_secret() {
        let secret = test_secret();
        let id = "abcdef0123456789abcdef0123456789";
        let cookie_val = make_valid_cookie(&secret, id);
        let wrong_secret = [0u8; 32];
        assert_eq!(
            verify_cookie(&wrong_secret, &cookie_val),
            Err(SessionError::MacMismatch)
        );
    }

    #[test]
    fn verify_cookie_no_dot() {
        let secret = test_secret();
        assert_eq!(
            verify_cookie(&secret, "nodothere"),
            Err(SessionError::MalformedCookie)
        );
    }

    #[test]
    fn verify_cookie_empty() {
        let secret = test_secret();
        assert_eq!(
            verify_cookie(&secret, ""),
            Err(SessionError::MalformedCookie)
        );
    }

    #[test]
    fn verify_cookie_wrong_id_length() {
        let secret = test_secret();
        // Too short
        let cookie_val = format!("short.{}", "a".repeat(64));
        assert_eq!(
            verify_cookie(&secret, &cookie_val),
            Err(SessionError::InvalidIdLength)
        );
    }

    #[test]
    fn verify_cookie_wrong_mac_length() {
        let secret = test_secret();
        let id = "abcdef0123456789abcdef0123456789";
        let cookie_val = format!("{id}.tooshort");
        assert_eq!(
            verify_cookie(&secret, &cookie_val),
            Err(SessionError::InvalidMacLength)
        );
    }

    #[test]
    fn build_set_cookie_format() {
        let config = SessionConfig::new(test_secret());
        let cookie = build_set_cookie(&config, "abcdef0123456789abcdef0123456789");
        assert!(cookie.starts_with("sid=abcdef0123456789abcdef0123456789."));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=86400"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn build_set_cookie_with_domain() {
        let config = SessionConfig::new(test_secret()).domain("example.com");
        let cookie = build_set_cookie(&config, "abcdef0123456789abcdef0123456789");
        assert!(cookie.contains("Domain=example.com"));
    }

    #[test]
    fn build_set_cookie_no_domain() {
        let config = SessionConfig::new(test_secret());
        let cookie = build_set_cookie(&config, "abcdef0123456789abcdef0123456789");
        assert!(!cookie.contains("Domain="));
    }

    #[test]
    fn build_clear_cookie_format() {
        let config = SessionConfig::new(test_secret());
        let cookie = build_clear_cookie(&config);
        assert!(cookie.starts_with("sid=;"));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn extract_cookie_present() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            http::header::HeaderValue::from_static("sid=abc123; other=x"),
        );
        assert_eq!(extract_cookie(&headers, "sid"), Some("abc123"));
    }

    #[test]
    fn extract_cookie_missing() {
        let headers = http::HeaderMap::new();
        assert_eq!(extract_cookie(&headers, "sid"), None);
    }

    #[test]
    fn extract_cookie_multiple_headers() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::COOKIE,
            http::header::HeaderValue::from_static("other=x"),
        );
        headers.append(
            http::header::COOKIE,
            http::header::HeaderValue::from_static("sid=found"),
        );
        assert_eq!(extract_cookie(&headers, "sid"), Some("found"));
    }

    #[test]
    fn extract_cookie_whitespace() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            http::header::HeaderValue::from_static("  sid = value  ; other=x"),
        );
        assert_eq!(extract_cookie(&headers, "sid"), Some("value"));
    }

    // -- Session type tests --------------------------------------------------

    #[test]
    fn session_get_set() {
        let session = Session::empty();
        session.set("key", "value");
        assert_eq!(session.get("key"), Some("value".to_string()));
    }

    #[test]
    fn session_get_missing() {
        let session = Session::empty();
        assert_eq!(session.get("missing"), None);
    }

    #[test]
    fn session_remove() {
        let session = Session::empty();
        session.set("key", "value");
        let removed = session.remove("key");
        assert_eq!(removed, Some("value".to_string()));
        assert_eq!(session.get("key"), None);
    }

    #[test]
    fn session_remove_missing_no_flag() {
        let session = Session::empty();
        let removed = session.remove("missing");
        assert_eq!(removed, None);
        assert!(!session.is_modified());
    }

    #[test]
    fn session_clear() {
        let session = Session::empty();
        session.set("a", "1");
        session.set("b", "2");
        // Reset modified flag to test clear sets it
        session.modified.store(false, Ordering::Relaxed);
        session.clear();
        assert!(session.is_empty());
        assert!(session.is_modified());
    }

    #[test]
    fn session_clear_empty_no_flag() {
        let session = Session::empty();
        session.clear();
        assert!(!session.is_modified());
    }

    #[test]
    fn session_destroy() {
        let session = Session::empty();
        session.destroy();
        assert!(session.is_destroyed());
        assert!(session.is_modified());
    }

    #[test]
    fn session_modified_flag() {
        let session = Session::empty();
        assert!(!session.is_modified());
        session.set("key", "value");
        assert!(session.is_modified());
    }

    #[test]
    fn session_clone_shares_state() {
        let session = Session::empty();
        let clone = session.clone();
        session.set("key", "value");
        assert_eq!(clone.get("key"), Some("value".to_string()));
    }

    #[test]
    fn session_id_values() {
        let session = Session::empty();
        assert_eq!(session.id(), None);

        let session = Session::new(Some("abc".to_string()), HashMap::new());
        assert_eq!(session.id(), Some("abc"));
    }

    // -- Session concurrency tests -------------------------------------------

    #[tokio::test]
    async fn session_concurrent_writes() {
        let session = Session::empty();
        let barrier = Arc::new(tokio::sync::Barrier::new(50));

        let mut handles = Vec::new();
        for i in 0..50 {
            let s = session.clone();
            let b = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                b.wait().await;
                s.set(&format!("key-{i}"), &format!("val-{i}"));
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        for i in 0..50 {
            assert_eq!(
                session.get(&format!("key-{i}")),
                Some(format!("val-{i}")),
                "missing key-{i}"
            );
        }
    }

    #[tokio::test]
    async fn session_concurrent_read_write() {
        let session = Session::empty();
        session.set("counter", "0");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let writer = {
            let s = session.clone();
            let b = Arc::clone(&barrier);
            tokio::spawn(async move {
                b.wait().await;
                for i in 1..=100 {
                    s.set("counter", &i.to_string());
                }
            })
        };

        let reader = {
            let s = session.clone();
            let b = Arc::clone(&barrier);
            tokio::spawn(async move {
                b.wait().await;
                for _ in 0..100 {
                    if let Some(val) = s.get("counter") {
                        // Must be a valid integer string — no partial/corrupt data
                        let n: u32 = val.parse().expect("corrupt read");
                        assert!(n <= 100, "unexpected value {n}");
                    }
                }
            })
        };

        writer.await.unwrap();
        reader.await.unwrap();

        // Final value must be the last write
        assert_eq!(session.get("counter"), Some("100".to_string()));
    }

    #[tokio::test]
    async fn session_modified_visible_after_await() {
        let session = Session::empty();
        assert!(!session.is_modified());

        let handle = {
            let s = session.clone();
            tokio::spawn(async move {
                s.set("key", "value");
            })
        };

        // The `.await` on JoinHandle provides a happens-before guarantee,
        // so the Relaxed store in `set` is visible here.
        handle.await.unwrap();

        assert!(session.is_modified());
        assert_eq!(session.get("key"), Some("value".to_string()));
    }

    // -- InMemorySessionStore tests ------------------------------------------

    #[tokio::test]
    async fn store_save_and_load() {
        let store = InMemorySessionStore::new();
        let mut data = HashMap::new();
        data.insert("key".to_string(), "value".to_string());
        store.save("id1", &data, Duration::from_secs(60)).await;
        let loaded = store.load("id1").await;
        assert_eq!(loaded, Some(data));
    }

    #[tokio::test]
    async fn store_load_missing() {
        let store = InMemorySessionStore::new();
        assert_eq!(store.load("nonexistent").await, None);
    }

    #[tokio::test]
    async fn store_remove() {
        let store = InMemorySessionStore::new();
        let data = HashMap::new();
        store.save("id1", &data, Duration::from_secs(60)).await;
        store.remove("id1").await;
        assert_eq!(store.load("id1").await, None);
    }

    #[tokio::test]
    async fn store_overwrite() {
        let store = InMemorySessionStore::new();
        let mut data1 = HashMap::new();
        data1.insert("key".to_string(), "v1".to_string());
        store.save("id1", &data1, Duration::from_secs(60)).await;

        let mut data2 = HashMap::new();
        data2.insert("key".to_string(), "v2".to_string());
        store.save("id1", &data2, Duration::from_secs(60)).await;

        let loaded = store.load("id1").await.unwrap();
        assert_eq!(loaded.get("key").unwrap(), "v2");
    }

    #[tokio::test]
    async fn store_expired_returns_none() {
        let store = InMemorySessionStore::new();
        let data = HashMap::new();
        // TTL of 0 = expired immediately
        store.save("id1", &data, Duration::from_millis(0)).await;
        // Small sleep to ensure expiry
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(store.load("id1").await, None);
    }

    #[tokio::test]
    async fn store_different_ids_independent() {
        let store = InMemorySessionStore::new();
        let mut data1 = HashMap::new();
        data1.insert("key".to_string(), "v1".to_string());
        let mut data2 = HashMap::new();
        data2.insert("key".to_string(), "v2".to_string());
        store.save("id1", &data1, Duration::from_secs(60)).await;
        store.save("id2", &data2, Duration::from_secs(60)).await;
        assert_eq!(store.load("id1").await.unwrap().get("key").unwrap(), "v1");
        assert_eq!(store.load("id2").await.unwrap().get("key").unwrap(), "v2");
    }

    // -- Middleware tests -----------------------------------------------------

    fn get_set_cookie(resp: &Response) -> Option<String> {
        resp.inner()
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    #[tokio::test]
    async fn mw_no_cookie_no_modification_no_set_cookie() {
        let store = InMemorySessionStore::new();
        let config = SessionConfig::new(test_secret()).secure(false);
        let mw = session_middleware(store, config);

        let req = make_request(&[]);
        let resp = mw.call(req, ok_next()).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
        assert!(get_set_cookie(&resp).is_none());
    }

    #[tokio::test]
    async fn mw_new_session_sets_cookie() {
        let store = Arc::new(InMemorySessionStore::new());
        let config = SessionConfig::new(test_secret()).secure(false);
        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                session.set("user", "alice");
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        let cookie = get_set_cookie(&resp).expect("expected set-cookie");
        assert!(cookie.starts_with("sid="));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=86400"));
    }

    #[tokio::test]
    async fn mw_existing_session_loads_data() {
        let store = Arc::new(InMemorySessionStore::new());
        let secret = test_secret();
        let session_id = "abcdef0123456789abcdef0123456789";

        // Pre-populate store
        let mut data = HashMap::new();
        data.insert("user".to_string(), "bob".to_string());
        store.save(session_id, &data, Duration::from_secs(60)).await;

        let config = SessionConfig::new(secret).secure(false);
        let cookie_val = make_valid_cookie(&secret, session_id);
        let cookie_header = format!("sid={cookie_val}");

        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[("cookie", &cookie_header)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                let user = session.get("user").unwrap_or_default();
                Response::text(user)
            })
        });
        let resp = mw.call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn mw_tampered_cookie_gives_empty_session() {
        let store = InMemorySessionStore::new();
        let config = SessionConfig::new(test_secret()).secure(false);
        let mw = session_middleware(store, config);

        let tampered = "sid=abcdef0123456789abcdef0123456789.0000000000000000000000000000000000000000000000000000000000000000";
        let req = make_request(&[("cookie", tampered)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                assert_eq!(session.id(), None);
                assert_eq!(session.get("anything"), None);
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
        // No set-cookie because session was never modified
        assert!(get_set_cookie(&resp).is_none());
    }

    #[tokio::test]
    async fn mw_expired_session_gives_empty() {
        let store = Arc::new(InMemorySessionStore::new());
        let secret = test_secret();
        let session_id = "abcdef0123456789abcdef0123456789";

        // Save with 0 TTL (expired)
        let data = HashMap::new();
        store
            .save(session_id, &data, Duration::from_millis(0))
            .await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        let config = SessionConfig::new(secret).secure(false);
        let cookie_val = make_valid_cookie(&secret, session_id);
        let cookie_header = format!("sid={cookie_val}");

        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[("cookie", &cookie_header)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                // Session data should be empty since it expired
                assert_eq!(session.get("user"), None);
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        assert_eq!(resp.status_code(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn mw_destroy_clears_and_removes() {
        let store = Arc::new(InMemorySessionStore::new());
        let secret = test_secret();
        let session_id = "abcdef0123456789abcdef0123456789";

        let mut data = HashMap::new();
        data.insert("user".to_string(), "alice".to_string());
        store.save(session_id, &data, Duration::from_secs(60)).await;

        let config = SessionConfig::new(secret).secure(false);
        let cookie_val = make_valid_cookie(&secret, session_id);
        let cookie_header = format!("sid={cookie_val}");

        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[("cookie", &cookie_header)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                session.destroy();
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        let cookie = get_set_cookie(&resp).expect("expected clear cookie");
        assert!(cookie.contains("Max-Age=0"));
        // Verify removed from store
        assert_eq!(store.load(session_id).await, None);
    }

    #[tokio::test]
    async fn mw_unmodified_session_no_op() {
        let store = Arc::new(InMemorySessionStore::new());
        let secret = test_secret();
        let session_id = "abcdef0123456789abcdef0123456789";

        let mut data = HashMap::new();
        data.insert("user".to_string(), "alice".to_string());
        store.save(session_id, &data, Duration::from_secs(60)).await;

        let config = SessionConfig::new(secret).secure(false);
        let cookie_val = make_valid_cookie(&secret, session_id);
        let cookie_header = format!("sid={cookie_val}");

        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[("cookie", &cookie_header)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                // Read but don't modify
                let session = req.ext::<Session>().unwrap();
                let _ = session.get("user");
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        // No set-cookie since nothing changed
        assert!(get_set_cookie(&resp).is_none());
    }

    #[tokio::test]
    async fn mw_clear_removes_session() {
        let store = Arc::new(InMemorySessionStore::new());
        let secret = test_secret();
        let session_id = "abcdef0123456789abcdef0123456789";

        let mut data = HashMap::new();
        data.insert("user".to_string(), "alice".to_string());
        store.save(session_id, &data, Duration::from_secs(60)).await;

        let config = SessionConfig::new(secret).secure(false);
        let cookie_val = make_valid_cookie(&secret, session_id);
        let cookie_header = format!("sid={cookie_val}");

        let mw = session_middleware(InMemorySessionStoreRef(Arc::clone(&store)), config);

        let req = make_request(&[("cookie", &cookie_header)]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                session.clear();
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        let cookie = get_set_cookie(&resp).expect("expected clear cookie");
        assert!(cookie.contains("Max-Age=0"));
        assert_eq!(store.load(session_id).await, None);
    }

    #[tokio::test]
    async fn mw_cookie_has_secure_httponly_samesite() {
        let store = InMemorySessionStore::new();
        let config = SessionConfig::new(test_secret());
        let mw = session_middleware(store, config);

        let req = make_request(&[]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                session.set("key", "val");
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        let cookie = get_set_cookie(&resp).expect("expected set-cookie");
        assert!(cookie.contains("Secure"), "missing Secure");
        assert!(cookie.contains("HttpOnly"), "missing HttpOnly");
        assert!(cookie.contains("SameSite=Lax"), "missing SameSite=Lax");
    }

    #[tokio::test]
    async fn mw_custom_cookie_name() {
        let store = InMemorySessionStore::new();
        let config = SessionConfig::new(test_secret())
            .cookie_name("my_session")
            .secure(false);
        let mw = session_middleware(store, config);

        let req = make_request(&[]);
        let next = Next::new(|req: Request| {
            Box::pin(async move {
                let session = req.ext::<Session>().unwrap();
                session.set("key", "val");
                Response::ok()
            })
        });
        let resp = mw.call(req, next).await;
        let cookie = get_set_cookie(&resp).expect("expected set-cookie");
        assert!(cookie.starts_with("my_session="));
    }

    // -- SessionConfig tests -------------------------------------------------

    #[test]
    fn session_config_defaults() {
        let config = SessionConfig::new(test_secret());
        assert_eq!(config.cookie_name, "sid");
        assert_eq!(config.ttl, Duration::from_secs(86400));
        assert_eq!(config.path, "/");
        assert!(config.domain.is_none());
        assert!(config.secure);
        assert!(config.http_only);
    }

    #[test]
    fn session_config_builder_chain() {
        let config = SessionConfig::new(test_secret())
            .cookie_name("sess")
            .ttl(Duration::from_secs(3600))
            .path("/app")
            .domain("example.com")
            .secure(false)
            .http_only(false)
            .same_site(SameSite::Strict);

        assert_eq!(config.cookie_name, "sess");
        assert_eq!(config.ttl, Duration::from_secs(3600));
        assert_eq!(config.path, "/app");
        assert_eq!(config.domain.as_deref(), Some("example.com"));
        assert!(!config.secure);
        assert!(!config.http_only);
        assert!(matches!(config.same_site, SameSite::Strict));
    }

    // -- Helper: wrapper to share Arc<InMemorySessionStore> in tests ----------

    struct InMemorySessionStoreRef(Arc<InMemorySessionStore>);

    impl SessionStore for InMemorySessionStoreRef {
        fn load(&self, id: &str) -> impl Future<Output = Option<HashMap<String, String>>> + Send {
            self.0.load(id)
        }
        fn save(
            &self,
            id: &str,
            data: &HashMap<String, String>,
            ttl: Duration,
        ) -> impl Future<Output = ()> + Send {
            self.0.save(id, data, ttl)
        }
        fn remove(&self, id: &str) -> impl Future<Output = ()> + Send {
            self.0.remove(id)
        }
    }
}
