use bytes::BytesMut;
use tokio::io::AsyncReadExt;

use harrow_codec_h1::{CodecError, ParsedRequest, try_parse_request};
use harrow_server::h1::ErrorResponse;
use harrow_server::h1_lifecycle::{Event, trace::Trace};

use crate::ServerConfig;

pub(crate) enum RequestHeadRead {
    Parsed(Box<ParsedRequest>),
    WriteError(ErrorResponse),
    Close(CloseReason),
}

/// Why the connection closes without a request head.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum CloseReason {
    /// The peer closed the connection.
    Eof,
    /// The read failed.
    IoError,
    /// `header_read_timeout` elapsed.
    Timeout,
}

pub(crate) async fn read_request_head<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    config: &ServerConfig,
    trace: &mut Trace,
) -> RequestHeadRead
where
    S: tokio::io::AsyncRead + Unpin,
{
    let request_started = std::time::Instant::now();
    let mut first_attempt = true;

    loop {
        match try_parse_request(buf) {
            Ok(parsed) => return RequestHeadRead::Parsed(Box::new(parsed)),
            Err(CodecError::Incomplete) => {}
            Err(
                err @ (CodecError::HeadersTooLarge
                | CodecError::Invalid(_)
                | CodecError::BodyTooLarge
                | CodecError::ExpectationFailed),
            ) => {
                return RequestHeadRead::WriteError(ErrorResponse::from_codec_error(&err));
            }
        }
        // The model's first `Recv` is armed by `Accept` / keep-alive reuse;
        // every further read is "headers need more".
        if !std::mem::take(&mut first_attempt) {
            trace.record(Event::HeadersNeedMore);
        }

        if let Some(timeout) = config.header_read_timeout {
            let remaining = timeout.saturating_sub(request_started.elapsed());
            if remaining.is_zero() {
                return RequestHeadRead::Close(CloseReason::Timeout);
            }
            match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                Ok(Ok(0)) => return RequestHeadRead::Close(CloseReason::Eof),
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return RequestHeadRead::Close(CloseReason::IoError),
                Err(_) => return RequestHeadRead::Close(CloseReason::Timeout),
            }
        } else {
            match stream.read_buf(buf).await {
                Ok(0) => return RequestHeadRead::Close(CloseReason::Eof),
                Ok(_) => {}
                Err(_) => return RequestHeadRead::Close(CloseReason::IoError),
            }
        }
    }
}
