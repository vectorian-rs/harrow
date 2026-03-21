use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::TcpStream;

use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;

use crate::codec;

/// Maximum size of the header read buffer (64 KiB).
const MAX_HEADER_BUF: usize = 64 * 1024;

/// Handle a single TCP connection with keep-alive support.
pub(crate) async fn handle_connection(
    stream: TcpStream,
    shared: Arc<SharedState>,
    header_read_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    active_count: Rc<Cell<usize>>,
) {
    active_count.set(active_count.get() + 1);

    let result = if let Some(ct) = connection_timeout {
        monoio::select! {
            r = handle_connection_inner(stream, shared, header_read_timeout) => r,
            _ = monoio::time::sleep(ct) => {
                tracing::warn!("connection timed out");
                Ok(())
            }
        }
    } else {
        handle_connection_inner(stream, shared, header_read_timeout).await
    };

    if let Err(e) = result {
        tracing::debug!("connection error: {e}");
    }

    active_count.set(active_count.get() - 1);
}

async fn handle_connection_inner(
    mut stream: TcpStream,
    shared: Arc<SharedState>,
    header_read_timeout: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = BytesMut::with_capacity(8192);

    loop {
        // --- Read headers ---
        let parsed = read_headers(&mut stream, &mut buf, header_read_timeout).await?;
        let keep_alive = parsed.keep_alive;

        // --- Read body (if any) into a Bytes ---
        let body_bytes =
            read_body(&mut stream, &mut buf, parsed.content_length, parsed.chunked).await?;

        // --- Build http::Request<Body> ---
        let mut builder = http::Request::builder()
            .method(parsed.method)
            .uri(parsed.uri)
            .version(parsed.version);
        for (name, value) in parsed.headers.iter() {
            builder = builder.header(name, value);
        }
        let body: Body = {
            use http_body_util::Full;
            Full::new(body_bytes)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
                .boxed()
        };
        let req = builder.body(body)?;

        // --- Dispatch ---
        let response = dispatch(Arc::clone(&shared), req).await;

        // --- Write response ---
        write_response(&mut stream, response).await?;

        if !keep_alive {
            break;
        }
    }

    Ok(())
}

/// Read HTTP headers from the stream into `buf`.
///
/// Returns the parsed request once a complete header block is found.
async fn read_headers(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    timeout: Option<Duration>,
) -> Result<codec::ParsedRequest, Box<dyn std::error::Error>> {
    loop {
        // Try parsing what we have.
        match codec::try_parse_request(buf) {
            Ok(parsed) => {
                // Remove consumed header bytes from buf, leaving any trailing body data.
                let _ = buf.split_to(parsed.header_len);
                return Ok(parsed);
            }
            Err(codec::CodecError::Incomplete) => {
                // Need more data.
            }
            Err(codec::CodecError::Invalid(msg)) => {
                return Err(msg.into());
            }
        }

        if buf.len() >= MAX_HEADER_BUF {
            return Err("request headers too large".into());
        }

        // Read more data from socket.
        let read_buf = vec![0u8; 4096];
        let (result, read_buf) = if let Some(dur) = timeout {
            monoio::select! {
                r = stream.read(read_buf) => r,
                _ = monoio::time::sleep(dur) => {
                    return Err("header read timeout".into());
                }
            }
        } else {
            stream.read(read_buf).await
        };

        let n = result?;
        if n == 0 {
            if buf.is_empty() {
                // Clean close — client disconnected between requests.
                return Err("connection closed".into());
            }
            return Err("unexpected eof during header read".into());
        }
        buf.extend_from_slice(&read_buf[..n]);
    }
}

/// Read the request body based on Content-Length or chunked encoding.
///
/// `buf` may already contain body data left over from header parsing.
async fn read_body(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    content_length: Option<u64>,
    chunked: bool,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    if chunked {
        return read_chunked_body(stream, buf).await;
    }

    let length = match content_length {
        Some(0) | None => return Ok(Bytes::new()),
        Some(len) => len as usize,
    };

    // Read until we have `length` bytes of body.
    while buf.len() < length {
        let needed = length - buf.len();
        let read_buf = vec![0u8; needed.min(8192)];
        let (result, read_buf) = stream.read(read_buf).await;
        let n = result?;
        if n == 0 {
            return Err("unexpected eof during body read".into());
        }
        buf.extend_from_slice(&read_buf[..n]);
    }

    let body = buf.split_to(length).freeze();
    Ok(body)
}

/// Read a chunked transfer-encoded body.
async fn read_chunked_body(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
) -> Result<Bytes, Box<dyn std::error::Error>> {
    loop {
        match codec::decode_chunked(buf)? {
            Some((body, consumed)) => {
                let _ = buf.split_to(consumed);
                return Ok(body);
            }
            None => {
                // Need more data
                let read_buf = vec![0u8; 4096];
                let (result, read_buf) = stream.read(read_buf).await;
                let n = result?;
                if n == 0 {
                    return Err("unexpected eof during chunked body read".into());
                }
                buf.extend_from_slice(&read_buf[..n]);
            }
        }
    }
}

/// Write the full HTTP response (head + body) to the stream.
async fn write_response(
    stream: &mut TcpStream,
    response: http::Response<harrow_core::response::ResponseBody>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (parts, body) = response.into_parts();

    let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);

    // Write response head.
    let head = codec::write_response_head(parts.status, &parts.headers, !has_content_length);
    let (result, _) = stream.write_all(head).await;
    result?;

    // Drain body frame-by-frame.
    if has_content_length {
        // Known length — write body frames directly.
        write_body_direct(stream, body).await?;
    } else {
        // Unknown length — use chunked transfer-encoding.
        write_body_chunked(stream, body).await?;
    }

    Ok(())
}

/// Write body frames directly (Content-Length path).
async fn write_body_direct(
    stream: &mut TcpStream,
    mut body: harrow_core::response::ResponseBody,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            let (result, _) = stream.write_all(data.to_vec()).await;
            result?;
        }
    }
    Ok(())
}

/// Write body frames with chunked transfer-encoding.
async fn write_body_chunked(
    stream: &mut TcpStream,
    mut body: harrow_core::response::ResponseBody,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            let chunk = codec::encode_chunk(&data);
            let (result, _) = stream.write_all(chunk).await;
            result?;
        }
    }
    // Write terminator
    let (result, _) = stream.write_all(codec::CHUNK_TERMINATOR.to_vec()).await;
    result?;
    Ok(())
}
