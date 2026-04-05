//! WebSocket upgrade handshake and shared types.
//!
//! This module provides the runtime-agnostic parts of WebSocket support:
//! - Handshake validation and accept key computation
//! - Shared message types
//!
//! The actual upgrade and frame handling is implemented by the server backends
//! (`harrow-server-tokio`, `harrow-server-monoio`).

use http::StatusCode;
use http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};

use crate::request::Request;
use crate::response::Response;

/// The WebSocket GUID used in the Sec-WebSocket-Accept computation (RFC 6455).
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Errors that can occur during WebSocket handshake or transport.
#[derive(Debug)]
pub enum WsError {
    /// Missing or incorrect `Upgrade: websocket` header.
    MissingUpgrade,
    /// Missing or incorrect `Connection: Upgrade` header.
    MissingConnection,
    /// Missing `Sec-WebSocket-Key` header.
    MissingKey,
    /// Missing or unsupported `Sec-WebSocket-Version` (must be "13").
    UnsupportedVersion,
    /// The hyper `OnUpgrade` handle was not present in request extensions.
    NotUpgradable,
    /// A transport-level error on the WebSocket connection.
    Transport(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::MissingUpgrade => write!(f, "missing Upgrade: websocket header"),
            WsError::MissingConnection => write!(f, "missing Connection: Upgrade header"),
            WsError::MissingKey => write!(f, "missing Sec-WebSocket-Key header"),
            WsError::UnsupportedVersion => {
                write!(f, "unsupported Sec-WebSocket-Version (expected 13)")
            }
            WsError::NotUpgradable => {
                write!(f, "connection does not support upgrades")
            }
            WsError::Transport(msg) => write!(f, "websocket transport error: {msg}"),
        }
    }
}

impl std::error::Error for WsError {}

impl crate::response::IntoResponse for WsError {
    fn into_response(self) -> Response {
        let status = match &self {
            WsError::NotUpgradable => StatusCode::INTERNAL_SERVER_ERROR,
            WsError::Transport(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Response::new(status, self.to_string())
    }
}

impl From<WsError> for Response {
    fn from(err: WsError) -> Self {
        crate::response::IntoResponse::into_response(err)
    }
}

/// Validate that a request is a valid WebSocket upgrade request.
/// Returns the `Sec-WebSocket-Key` value on success.
pub fn validate_upgrade(req: &Request) -> Result<String, WsError> {
    // Check Upgrade: websocket
    let upgrade = req
        .header(UPGRADE.as_str())
        .ok_or(WsError::MissingUpgrade)?;
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(WsError::MissingUpgrade);
    }

    // Check Connection: Upgrade
    let conn = req
        .header(CONNECTION.as_str())
        .ok_or(WsError::MissingConnection)?;
    if !conn.to_ascii_lowercase().contains("upgrade") {
        return Err(WsError::MissingConnection);
    }

    // Check Sec-WebSocket-Version: 13
    let version = req
        .header(SEC_WEBSOCKET_VERSION.as_str())
        .ok_or(WsError::UnsupportedVersion)?;
    if version != "13" {
        return Err(WsError::UnsupportedVersion);
    }

    // Extract Sec-WebSocket-Key
    let key = req
        .header(SEC_WEBSOCKET_KEY.as_str())
        .ok_or(WsError::MissingKey)?;

    Ok(key.to_string())
}

/// Compute the `Sec-WebSocket-Accept` value from the client's key (RFC 6455 Section 4.2.2).
pub fn accept_key(key: &str) -> String {
    use base64::Engine;
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let hash = hasher.finalize();

    base64::engine::general_purpose::STANDARD.encode(hash)
}

/// Build the HTTP 101 Switching Protocols response for a WebSocket upgrade.
pub fn upgrade_response(key: &str, protocol: Option<&str>) -> Response {
    let accept = accept_key(key);
    let resp = Response::new(StatusCode::SWITCHING_PROTOCOLS, "")
        .header(UPGRADE.as_str(), "websocket")
        .header(CONNECTION.as_str(), "Upgrade")
        .header(SEC_WEBSOCKET_ACCEPT.as_str(), &accept);
    match protocol {
        Some(p) => resp.header("sec-websocket-protocol", p),
        None => resp,
    }
}

/// Negotiate a subprotocol from the client's `Sec-WebSocket-Protocol` header.
///
/// Returns the first server-supported protocol that the client also requested,
/// or `None` if there is no match.
pub fn negotiate_protocol<'a>(req: &Request, supported: &'a [&str]) -> Option<&'a str> {
    let header = req.header("sec-websocket-protocol")?;
    let client_protocols: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
    supported
        .iter()
        .find(|&&s| client_protocols.iter().any(|&c| c.eq_ignore_ascii_case(s)))
        .copied()
}

/// WebSocket close codes (RFC 6455 Section 7.4.1).
pub mod close_code {
    /// Normal closure (1000).
    pub const NORMAL: u16 = 1000;
    /// Endpoint going away (1001), e.g. server shutdown or browser navigating away.
    pub const AWAY: u16 = 1001;
    /// Protocol error (1002).
    pub const PROTOCOL: u16 = 1002;
    /// Unsupported data type (1003), e.g. text-only endpoint received binary.
    pub const UNSUPPORTED: u16 = 1003;
    /// No status code present (1005). Must not be sent in a close frame.
    pub const NO_STATUS: u16 = 1005;
    /// Abnormal closure (1006). Must not be sent in a close frame.
    pub const ABNORMAL: u16 = 1006;
    /// Invalid payload data (1007), e.g. non-UTF-8 in a text message.
    pub const INVALID: u16 = 1007;
    /// Policy violation (1008).
    pub const POLICY: u16 = 1008;
    /// Message too big (1009).
    pub const TOO_BIG: u16 = 1009;
    /// Missing expected extension (1010).
    pub const EXTENSION: u16 = 1010;
    /// Unexpected server error (1011).
    pub const ERROR: u16 = 1011;
}

/// UTF-8 validated bytes, backed by ref-counted `Bytes` for zero-copy.
#[derive(Clone, Eq)]
pub struct Utf8Bytes(bytes::Bytes);

impl Utf8Bytes {
    /// Wrap `Bytes` that are known to be valid UTF-8.
    ///
    /// # Safety
    /// The caller must guarantee that `bytes` contains valid UTF-8.
    pub unsafe fn from_bytes_unchecked(bytes: bytes::Bytes) -> Self {
        Self(bytes)
    }

    /// View the underlying bytes as a string slice.
    pub fn as_str(&self) -> &str {
        // SAFETY: constructor guarantees valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    /// Consume into the underlying `Bytes`.
    pub fn into_bytes(self) -> bytes::Bytes {
        self.0
    }
}

impl std::ops::Deref for Utf8Bytes {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for Utf8Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for Utf8Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for Utf8Bytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<str> for Utf8Bytes {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Utf8Bytes {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Utf8Bytes {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl std::hash::Hash for Utf8Bytes {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl From<String> for Utf8Bytes {
    fn from(s: String) -> Self {
        Self(bytes::Bytes::from(s.into_bytes()))
    }
}

impl From<&str> for Utf8Bytes {
    fn from(s: &str) -> Self {
        Self(bytes::Bytes::copy_from_slice(s.as_bytes()))
    }
}

impl TryFrom<bytes::Bytes> for Utf8Bytes {
    type Error = std::str::Utf8Error;
    fn try_from(bytes: bytes::Bytes) -> Result<Self, Self::Error> {
        std::str::from_utf8(&bytes)?;
        Ok(Self(bytes))
    }
}

/// WebSocket message types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// UTF-8 text message.
    Text(Utf8Bytes),
    /// Binary message.
    Binary(bytes::Bytes),
    /// Ping message.
    Ping(bytes::Bytes),
    /// Pong message.
    Pong(bytes::Bytes),
    /// Close message with optional code and reason.
    Close(Option<(u16, String)>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_is_deterministic() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept1 = accept_key(key);
        let accept2 = accept_key(key);
        assert_eq!(accept1, accept2);
        // Verify it's valid base64 and 28 chars (SHA-1 = 20 bytes → 28 base64 chars)
        assert_eq!(accept1.len(), 28);
    }

    #[test]
    fn accept_key_differs_for_different_inputs() {
        let a = accept_key("key1");
        let b = accept_key("key2");
        assert_ne!(a, b);
    }

    #[test]
    fn accept_key_matches_rfc6455_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(accept_key(key), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn upgrade_response_has_correct_headers() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let resp = upgrade_response(key, None);
        assert_eq!(resp.status_code(), StatusCode::SWITCHING_PROTOCOLS);
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get(UPGRADE).unwrap(), "websocket");
        assert_eq!(inner.headers().get(CONNECTION).unwrap(), "Upgrade");
        assert_eq!(
            inner.headers().get(SEC_WEBSOCKET_ACCEPT).unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        );
        assert!(inner.headers().get("sec-websocket-protocol").is_none());
    }

    #[test]
    fn upgrade_response_includes_selected_protocol() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let resp = upgrade_response(key, Some("graphql-ws"));
        let inner = resp.into_inner();
        assert_eq!(
            inner.headers().get("sec-websocket-protocol").unwrap(),
            "graphql-ws",
        );
    }
}
