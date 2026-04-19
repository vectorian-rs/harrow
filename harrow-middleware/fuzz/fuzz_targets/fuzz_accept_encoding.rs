#![no_main]

use std::cell::RefCell;
use std::io::Read;
use std::sync::Arc;

use bytes::Bytes;
use flate2::read::{DeflateDecoder, GzDecoder};
use harrow_core::middleware::{Middleware, Next};
use harrow_core::path::PathMatch;
use harrow_core::request::{Request, full_body};
use harrow_core::response::Response;
use harrow_core::state::TypeMap;
use harrow_middleware::compression::compression_middleware;
use http::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use http_body_util::BodyExt;
use libfuzzer_sys::fuzz_target;

thread_local! {
    static RUNTIME: RefCell<tokio::runtime::Runtime> = RefCell::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime"),
    );
}

const ORIGINAL_BODY_LEN: usize = 1024;
static ORIGINAL_BODY: [u8; ORIGINAL_BODY_LEN] = [b'x'; ORIGINAL_BODY_LEN];

fuzz_target!(|data: &[u8]| {
    let accept = match http::HeaderValue::from_bytes(data) {
        Ok(value) => value,
        Err(_) => return,
    };

    RUNTIME.with(|runtime| {
        runtime.borrow_mut().block_on(async {
            let req = make_request(accept);
            let next = large_body_next();
            let resp = Middleware::call(&compression_middleware, req, next).await;
            let inner = resp.into_inner();
            let encoding = inner
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = inner
                .into_body()
                .collect()
                .await
                .expect("collect compressed body")
                .to_bytes();

            match encoding.as_deref() {
                None => assert_eq!(body.as_ref(), &ORIGINAL_BODY),
                Some("gzip") => assert_eq!(decompress_gzip(&body).as_slice(), &ORIGINAL_BODY),
                Some("deflate") => {
                    assert_eq!(decompress_deflate(&body).as_slice(), &ORIGINAL_BODY)
                }
                Some(other) => panic!("unsupported content-encoding emitted: {other}"),
            }
        });
    });
});

fn make_request(accept: http::HeaderValue) -> Request {
    let inner = http::Request::builder()
        .method("GET")
        .uri("/")
        .header(ACCEPT_ENCODING, accept)
        .body(full_body(http_body_util::Full::new(Bytes::new())))
        .expect("build fuzz request");

    Request::new(inner, PathMatch::default(), Arc::new(TypeMap::new()), None)
}

fn large_body_next() -> Next {
    Next::new(|_req| {
        Box::pin(async move { Response::new(http::StatusCode::OK, Bytes::from_static(&ORIGINAL_BODY)) })
    })
}

fn decompress_gzip(data: &[u8]) -> Vec<u8> {
    let mut decoder = GzDecoder::new(data);
    let mut buf = Vec::new();
    decoder
        .read_to_end(&mut buf)
        .expect("gzip body round-trips");
    buf
}

fn decompress_deflate(data: &[u8]) -> Vec<u8> {
    let mut decoder = DeflateDecoder::new(data);
    let mut buf = Vec::new();
    decoder
        .read_to_end(&mut buf)
        .expect("deflate body round-trips");
    buf
}
