//! Black-box contract from SPEC.md, run over real TCP against each framework.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use framework_compare::domain::{EXPECTED_ORIGIN, Store};
use serde_json::Value;

const ALICE: (&str, &str) = ("cookie", "theme=dark; ct_session=sess-alice");
const BOB: (&str, &str) = ("cookie", "ct_session=sess-bob");
const ORIGIN: (&str, &str) = ("origin", EXPECTED_ORIGIN);
const JSON: (&str, &str) = ("content-type", "application/json");

#[test]
fn harrow() {
    contract(start(framework_compare::harrow_app::run));
}

#[test]
fn axum() {
    contract(start(framework_compare::axum_app::run));
}

#[test]
fn ntex() {
    contract(start(framework_compare::ntex_app::run));
}

#[test]
fn tako() {
    contract(start(framework_compare::tako_app::run));
}

fn contract(addr: SocketAddr) {
    let doc = format!("doc-{}", "ab".repeat(32));

    // Health and request id.
    let r = send(addr, "GET", "/healthz", &[], b"");
    r.status(200).body_eq("ok");
    assert!(r.header("x-request-id").is_some_and(|id| !id.is_empty()), "x-request-id missing");
    let r = send(addr, "GET", "/healthz", &[("cloudfront-viewer-http-request-id", "cf-123")], b"");
    assert_eq!(r.header("x-request-id"), Some("cf-123"), "CloudFront id not echoed");

    // Session required: error envelope carries the same request id as the header.
    let r = send(addr, "GET", "/api/v1/session/current", &[], b"");
    r.status(401).error("session_required").no_store();
    assert_eq!(r.header("www-authenticate"), Some("DD-Session"));
    assert_eq!(r.json()["error"]["request_id"].as_str(), r.header("x-request-id"), "request_id mismatch");
    send(addr, "GET", "/api/v1/session/current", &[("cookie", "ct_session=nope")], b"").status(401);

    let r = send(addr, "GET", "/api/v1/session/current", &[ALICE], b"");
    r.status(200).private();
    assert_eq!(r.json()["email"], "alice@example.com");

    // Companies.
    let r = send(addr, "GET", "/api/v1/companies", &[ALICE], b"");
    r.status(200).private();
    assert_eq!(r.json()[0]["id"], "cmp-acme");

    let body = br#"{"name":"Beta Zrt."}"#;
    let r = send(addr, "POST", "/api/v1/companies", &[ALICE, ORIGIN, JSON], body);
    r.status(201);
    assert_eq!(r.json()["name"], "Beta Zrt.");
    send(addr, "POST", "/api/v1/companies", &[ALICE, JSON], body).status(403).error("forbidden");
    send(addr, "POST", "/api/v1/companies", &[ALICE, ORIGIN, ORIGIN, JSON], body).status(403);
    send(addr, "POST", "/api/v1/companies", &[ALICE, ORIGIN, ("content-type", "text/plain")], body)
        .status(400)
        .error("bad_request");
    send(addr, "POST", "/api/v1/companies", &[ALICE, ORIGIN, JSON], br#"{"name":"x","admin":true}"#)
        .status(400);
    let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(5000));
    let r = send(addr, "POST", "/api/v1/companies", &[ALICE, ORIGIN, JSON], big.as_bytes());
    assert!(matches!(r.code, 400 | 413), "oversized body: got {}", r.code);

    // Path params, validation, access control.
    let r = send(addr, "GET", "/api/v1/companies/cmp-acme/months/2026-10/state", &[ALICE], b"");
    r.status(200).private();
    assert_eq!(r.json()["state"], "open");
    assert_eq!(r.json()["month"], "2026-10");
    send(addr, "GET", "/api/v1/companies/cmp-acme/months/2026-13/state", &[ALICE], b"").status(400);
    send(addr, "GET", "/api/v1/companies/cmp-acme/months/2026-10/state", &[BOB], b"")
        .status(404)
        .error("not_found");

    // Binary download.
    let path = format!("/api/v1/companies/cmp-acme/documents/{doc}/content");
    let r = send(addr, "GET", &path, &[ALICE], b"");
    r.status(200);
    assert_eq!(r.header("content-type"), Some("application/pdf"));
    assert_eq!(r.header("content-disposition"), Some(format!("inline; filename=\"{doc}.pdf\"").as_str()));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.body.starts_with(b"%PDF"));
    send(addr, "GET", "/api/v1/companies/cmp-acme/documents/doc-bad/content", &[ALICE], b"").status(400);

    // Query string.
    let r = send(addr, "GET", "/api/v1/usage?period=2026-10", &[ALICE], b"");
    r.status(200);
    assert_eq!(r.json()["period"], "2026-10");
    send(addr, "GET", "/api/v1/usage", &[ALICE], b"").status(400);
    send(addr, "GET", "/api/v1/usage?period=oct", &[ALICE], b"").status(400);

    // Admin hidden behind 404.
    let r = send(addr, "GET", "/api/v1/admin/users", &[ALICE], b"");
    r.status(200);
    assert!(r.json().as_array().is_some_and(|a| a.len() == 2));
    send(addr, "GET", "/api/v1/admin/users", &[BOB], b"").status(404).error("not_found");

    // Fallback.
    let r = send(addr, "GET", "/nope", &[], b"");
    r.status(404).error("not_found").no_store();
    assert!(r.header("x-request-id").is_some());
    send(addr, "DELETE", "/api/v1/companies", &[ALICE], b"").status(404).error("not_found");

    // Logout: 303, two Set-Cookie headers, session revoked.
    send(addr, "POST", "/api/v1/session/logout", &[BOB], b"").status(403);
    let r = send(addr, "POST", "/api/v1/session/logout", &[BOB, ORIGIN], b"");
    r.status(303);
    assert_eq!(r.header("location"), Some("/"));
    let cookies = r.headers_named("set-cookie");
    assert_eq!(cookies.len(), 2, "want two set-cookie headers, got {cookies:?}");
    assert!(cookies.iter().any(|c| c.starts_with("ct_session=;")));
    assert!(cookies.iter().any(|c| c.starts_with("ct_pow=;")));
    send(addr, "GET", "/api/v1/session/current", &[BOB], b"").status(401);
}

// ---------------------------------------------------------------------------
// Tiny HTTP/1.1 client (Connection: close, handles chunked bodies)
// ---------------------------------------------------------------------------

fn start(run: fn(SocketAddr, Store)) -> SocketAddr {
    let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    std::thread::spawn(move || run(addr, Store::seeded()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "server did not start on {addr}");
        std::thread::sleep(Duration::from_millis(20));
    }
    addr
}

struct Resp {
    code: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    what: String,
}

fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let _ = stream.write_all(body);

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("no header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let code = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body);
    }
    Resp { code, headers, body, what: format!("{method} {path}") }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&data[..eol]).unwrap().split(';').next().unwrap(), 16)
            .unwrap();
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[eol + 2..eol + 2 + size]);
        data = &data[eol + 4 + size..];
    }
}

impl Resp {
    fn status(&self, want: u16) -> &Self {
        assert_eq!(self.code, want, "{}: body {}", self.what, String::from_utf8_lossy(&self.body));
        self
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{}: not JSON ({e}): {}", self.what, String::from_utf8_lossy(&self.body)))
    }

    fn body_eq(&self, want: &str) -> &Self {
        assert_eq!(String::from_utf8_lossy(&self.body), want, "{}", self.what);
        self
    }

    fn error(&self, code: &str) -> &Self {
        assert_eq!(self.json()["error"]["code"], code, "{}", self.what);
        assert_eq!(self.header("content-type"), Some("application/json"), "{}", self.what);
        self
    }

    fn no_store(&self) -> &Self {
        assert_eq!(self.header("cache-control"), Some("no-store"), "{}", self.what);
        self
    }

    fn private(&self) -> &Self {
        self.no_store();
        assert!(self.header("vary").is_some_and(|v| v.to_ascii_lowercase().contains("cookie")), "{}: vary", self.what);
        self
    }
}
