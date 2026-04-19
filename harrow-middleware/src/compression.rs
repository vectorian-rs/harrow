use harrow_core::middleware::Next;
use harrow_core::request::Request;
use harrow_core::response::Response;

/// Minimum body size (in bytes) worth compressing.
const MIN_COMPRESS_SIZE: usize = 256;

/// Middleware that compresses response bodies based on `Accept-Encoding`.
///
/// Supports gzip and deflate via `flate2`. When the `compression-br`
/// feature is enabled, brotli is also available and preferred.
///
/// Skips compression when:
/// - The response already has a `Content-Encoding` header.
/// - The body is smaller than 256 bytes.
/// - No supported encoding is accepted.
pub async fn compression_middleware(req: Request, next: Next) -> Response {
    let accept = req.header("accept-encoding").unwrap_or("").to_string();

    let resp = next.run(req).await;

    // Don't double-compress.
    {
        let inner = resp.inner();
        if inner.headers().contains_key("content-encoding") {
            return resp;
        }
    }

    let encoding = pick_encoding(&accept);
    if encoding == Encoding::Identity {
        return resp;
    }

    // Collect the body to compress. We need the full body in memory.
    let status = resp.status_code();
    let inner = resp.into_inner();
    let headers = inner.headers().clone();

    let body_bytes = match collect_body(inner.into_body()).await {
        Some(b) => b,
        None => return rebuild_response(status, &headers, &[], Encoding::Identity),
    };

    if body_bytes.len() < MIN_COMPRESS_SIZE {
        return rebuild_response(status, &headers, &body_bytes, Encoding::Identity);
    }

    let compressed = match encoding {
        Encoding::Gzip => compress_gzip(&body_bytes),
        Encoding::Deflate => compress_deflate(&body_bytes),
        #[cfg(feature = "compression-br")]
        Encoding::Brotli => compress_brotli(&body_bytes),
        Encoding::Identity => unreachable!(),
    };

    match compressed {
        Some(data) => rebuild_response(status, &headers, &data, encoding),
        None => rebuild_response(status, &headers, &body_bytes, Encoding::Identity),
    }
}

// ---------------------------------------------------------------------------
// Encoding negotiation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    #[cfg(feature = "compression-br")]
    Brotli,
    Gzip,
    Deflate,
    Identity,
}

impl Encoding {
    fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "compression-br")]
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
            Encoding::Identity => "identity",
        }
    }
}

fn pick_encoding(accept: &str) -> Encoding {
    // Preference order: br > gzip > deflate.
    let accept_lower = accept.to_lowercase();
    #[cfg(feature = "compression-br")]
    if accept_lower.contains("br") {
        return Encoding::Brotli;
    }
    if accept_lower.contains("gzip") {
        return Encoding::Gzip;
    }
    if accept_lower.contains("deflate") {
        return Encoding::Deflate;
    }
    Encoding::Identity
}

// ---------------------------------------------------------------------------
// Compression implementations
// ---------------------------------------------------------------------------

fn compress_gzip(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    // Use default compression level to match tower-http behavior
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

fn compress_deflate(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    // Use default compression level to match tower-http behavior
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).ok()?;
    encoder.finish().ok()
}

#[cfg(feature = "compression-br")]
fn compress_brotli(data: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 4, 22);
        use std::io::Write;
        writer.write_all(data).ok()?;
    }
    Some(output)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn collect_body(body: harrow_core::response::ResponseBody) -> Option<Vec<u8>> {
    use http_body_util::BodyExt;
    let collected = body.collect().await.ok()?;
    Some(collected.to_bytes().to_vec())
}

fn rebuild_response(
    status: http::StatusCode,
    original_headers: &http::HeaderMap,
    body: &[u8],
    encoding: Encoding,
) -> Response {
    let mut resp = Response::new(status, bytes::Bytes::copy_from_slice(body));

    // Copy original headers (except content-length which is now wrong).
    for (name, value) in original_headers.iter() {
        if name == http::header::CONTENT_LENGTH {
            continue;
        }
        if name == http::header::CONTENT_ENCODING {
            continue;
        }
        if let Ok(v) = value.to_str() {
            resp = resp.header(name.as_str(), v);
        }
    }

    if encoding != Encoding::Identity {
        resp = resp.header("content-encoding", encoding.as_str());
    }

    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use harrow_core::middleware::Middleware;
    use harrow_core::path::PathMatch;
    use harrow_core::state::TypeMap;
    use std::sync::Arc;

    async fn make_request(headers: &[(&str, &str)]) -> Request {
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

    fn large_body_next() -> Next {
        Next::new(|_req| {
            Box::pin(async {
                let body = "x".repeat(1024);
                Response::text(body)
            })
        })
    }

    fn small_body_next() -> Next {
        Next::new(|_req| Box::pin(async { Response::text("tiny") }))
    }

    #[tokio::test]
    async fn gzip_compresses_large_body() {
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, large_body_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "gzip");
        // Compressed should be smaller than 1024 bytes of 'x'.
        let body = http_body_util::BodyExt::collect(inner.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert!(body.len() < 1024);

        // Verify we can decompress.
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, "x".repeat(1024));
    }

    #[tokio::test]
    async fn deflate_compresses_large_body() {
        let req = make_request(&[("accept-encoding", "deflate")]).await;
        let resp = Middleware::call(&compression_middleware, req, large_body_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "deflate");
    }

    #[tokio::test]
    async fn skips_small_body() {
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, small_body_next()).await;
        let inner = resp.into_inner();
        assert!(inner.headers().get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn skips_when_no_accept_encoding() {
        let req = make_request(&[]).await;
        let resp = Middleware::call(&compression_middleware, req, large_body_next()).await;
        let inner = resp.into_inner();
        assert!(inner.headers().get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn skips_already_encoded() {
        let next = Next::new(|_req| {
            Box::pin(async {
                let body = "x".repeat(1024);
                Response::text(body).header("content-encoding", "br")
            })
        });
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, next).await;
        let inner = resp.into_inner();
        // Should keep original encoding, not re-compress.
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "br");
    }

    #[tokio::test]
    async fn deflate_roundtrip_decompression() {
        let req = make_request(&[("accept-encoding", "deflate")]).await;
        let resp = Middleware::call(&compression_middleware, req, large_body_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "deflate");
        let body = http_body_util::BodyExt::collect(inner.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert!(body.len() < 1024);

        use flate2::read::DeflateDecoder;
        use std::io::Read;
        let mut decoder = DeflateDecoder::new(&body[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, "x".repeat(1024));
    }

    #[tokio::test]
    async fn empty_body_not_compressed() {
        let next = Next::new(|_req| Box::pin(async { Response::ok() }));
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, next).await;
        let inner = resp.into_inner();
        assert!(inner.headers().get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn original_headers_preserved() {
        let next = Next::new(|_req| {
            Box::pin(async {
                let body = "x".repeat(1024);
                Response::text(body).header("x-custom", "keep-me")
            })
        });
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, next).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "gzip");
        assert_eq!(inner.headers().get("x-custom").unwrap(), "keep-me");
    }

    #[tokio::test]
    async fn content_length_recomputed_for_compressed_body() {
        let next = Next::new(|_req| {
            Box::pin(async {
                let body = "x".repeat(1024);
                Response::text(body).header("content-length", "1024")
            })
        });
        let req = make_request(&[("accept-encoding", "gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, next).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "gzip");
        let content_length = inner
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .expect("compressed response should have a recomputed content-length");
        assert!(content_length > 0);
        assert!(content_length < 1024);
    }

    #[tokio::test]
    async fn gzip_preferred_over_deflate() {
        let req = make_request(&[("accept-encoding", "deflate, gzip")]).await;
        let resp = Middleware::call(&compression_middleware, req, large_body_next()).await;
        let inner = resp.into_inner();
        assert_eq!(inner.headers().get("content-encoding").unwrap(), "gzip");
    }

    // -----------------------------------------------------------------------
    // proptest: compression round-trip
    // -----------------------------------------------------------------------

    use proptest::prelude::*;

    fn decompress_gzip(data: &[u8]) -> Vec<u8> {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(data);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).expect("gzip decompress");
        buf
    }

    fn decompress_deflate(data: &[u8]) -> Vec<u8> {
        use flate2::read::DeflateDecoder;
        use std::io::Read;
        let mut decoder = DeflateDecoder::new(data);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).expect("deflate decompress");
        buf
    }

    proptest! {
        /// Gzip round-trip: decompress(compress(data)) == data.
        #[test]
        fn proptest_gzip_roundtrip(data in prop::collection::vec(any::<u8>(), 1..4096)) {
            if let Some(compressed) = compress_gzip(&data) {
                let decompressed = decompress_gzip(&compressed);
                prop_assert_eq!(&decompressed, &data);
            }
        }

        /// Deflate round-trip: decompress(compress(data)) == data.
        #[test]
        fn proptest_deflate_roundtrip(data in prop::collection::vec(any::<u8>(), 1..4096)) {
            if let Some(compressed) = compress_deflate(&data) {
                let decompressed = decompress_deflate(&compressed);
                prop_assert_eq!(&decompressed, &data);
            }
        }

        /// Encoding preference: gzip beats deflate, deflate beats identity.
        #[test]
        fn proptest_encoding_preference(
            has_gzip in any::<bool>(),
            has_deflate in any::<bool>(),
        ) {
            let mut parts = Vec::new();
            if has_gzip { parts.push("gzip"); }
            if has_deflate { parts.push("deflate"); }
            let accept = parts.join(", ");
            let encoding = pick_encoding(&accept);
            if has_gzip {
                prop_assert_eq!(encoding, Encoding::Gzip);
            } else if has_deflate {
                prop_assert_eq!(encoding, Encoding::Deflate);
            } else {
                prop_assert_eq!(encoding, Encoding::Identity);
            }
        }

        /// No double-compress: a response with content-encoding already set
        /// is returned unchanged.
        #[test]
        fn proptest_no_double_compress(
            existing_enc in prop_oneof![Just("gzip"), Just("deflate"), Just("br")],
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let enc = existing_enc.to_string();
                let next = Next::new(move |_req| {
                    let enc = enc.clone();
                    Box::pin(async move {
                        let body = "x".repeat(1024);
                        Response::text(body).header("content-encoding", &enc)
                    })
                });
                let req = make_request(&[("accept-encoding", "gzip, deflate, br")]).await;
                let resp = Middleware::call(&compression_middleware, req, next).await;
                let inner = resp.into_inner();
                // Must keep the original encoding, not re-compress
                prop_assert_eq!(
                    inner.headers().get("content-encoding").unwrap().to_str().unwrap(),
                    existing_enc,
                );
                Ok::<_, proptest::test_runner::TestCaseError>(())
            })?;
        }
    }
}
