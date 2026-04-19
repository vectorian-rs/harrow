use bytes::BytesMut;
use tokio::io::AsyncReadExt;

use harrow_codec_h1::{CodecError, MAX_HEADER_BUF, ParsedRequest, try_parse_request};
use harrow_server::h1::ErrorResponse;

use crate::ServerConfig;

pub(crate) enum RequestHeadRead {
    Parsed(Box<ParsedRequest>),
    WriteError(ErrorResponse),
    Close,
}

pub(crate) async fn read_request_head<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    config: &ServerConfig,
) -> RequestHeadRead
where
    S: tokio::io::AsyncRead + Unpin,
{
    let request_started = std::time::Instant::now();

    loop {
        match try_parse_request(buf) {
            Ok(parsed) => return RequestHeadRead::Parsed(Box::new(parsed)),
            Err(CodecError::Incomplete) => {
                if buf.len() >= MAX_HEADER_BUF {
                    return RequestHeadRead::WriteError(ErrorResponse::RequestHeadersTooLarge);
                }
            }
            Err(err @ CodecError::Invalid(_)) => {
                return RequestHeadRead::WriteError(ErrorResponse::from_codec_error(&err));
            }
            Err(err @ CodecError::BodyTooLarge) => {
                return RequestHeadRead::WriteError(ErrorResponse::from_codec_error(&err));
            }
        }

        if let Some(timeout) = config.header_read_timeout {
            let remaining = timeout.saturating_sub(request_started.elapsed());
            if remaining.is_zero() {
                return RequestHeadRead::Close;
            }
            match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                Ok(Ok(0)) => return RequestHeadRead::Close,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return RequestHeadRead::Close,
                Err(_) => return RequestHeadRead::Close,
            }
        } else {
            match stream.read_buf(buf).await {
                Ok(0) => return RequestHeadRead::Close,
                Ok(_) => {}
                Err(_) => return RequestHeadRead::Close,
            }
        }
    }
}
