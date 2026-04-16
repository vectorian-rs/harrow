use bytes::BytesMut;
use tokio::io::AsyncReadExt;

use harrow_codec_h1::{CodecError, MAX_HEADER_BUF, ParsedRequest, try_parse_request};
use harrow_server::h1::ErrorResponse;

use crate::ServerConfig;
use crate::h1::error::write_error;

pub(crate) async fn read_request_head<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    config: &ServerConfig,
) -> Option<ParsedRequest>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request_started = std::time::Instant::now();

    loop {
        match try_parse_request(buf) {
            Ok(parsed) => return Some(parsed),
            Err(CodecError::Incomplete) => {
                if buf.len() >= MAX_HEADER_BUF {
                    let error = ErrorResponse::RequestHeadersTooLarge;
                    write_error(stream, error.status_u16(), error.body()).await;
                    return None;
                }
            }
            Err(err @ CodecError::Invalid(_)) => {
                let error = ErrorResponse::from_codec_error(&err);
                write_error(stream, error.status_u16(), error.body()).await;
                return None;
            }
            Err(err @ CodecError::BodyTooLarge) => {
                let error = ErrorResponse::from_codec_error(&err);
                write_error(stream, error.status_u16(), error.body()).await;
                return None;
            }
        }

        if let Some(timeout) = config.header_read_timeout {
            let remaining = timeout.saturating_sub(request_started.elapsed());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                Ok(Ok(0)) => return None,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return None,
                Err(_) => return None,
            }
        } else {
            match stream.read_buf(buf).await {
                Ok(0) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}
