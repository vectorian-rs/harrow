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
    /// Shutdown started while the connection was idle (no bytes of the next
    /// request received yet).
    Shutdown,
}

pub(crate) async fn read_request_head<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    config: &ServerConfig,
    trace: &mut Trace,
    shutdown: &harrow_server::ShutdownSignal,
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

        let deadline = config
            .header_read_timeout
            .map(|timeout| request_started + timeout);
        // Idle: nothing of the next request has arrived, so shutdown may end
        // the wait. Once bytes arrive, the request is allowed to finish.
        let idle = buf.is_empty();
        let read = read_with_deadline(stream, buf, deadline);
        let outcome = if idle {
            tokio::select! {
                biased;
                () = shutdown.wait() => return RequestHeadRead::Close(CloseReason::Shutdown),
                outcome = read => outcome,
            }
        } else {
            read.await
        };
        if let Some(reason) = outcome {
            return RequestHeadRead::Close(reason);
        }
    }
}

/// Read once into `buf`. `None` means bytes arrived; `Some(reason)` means the
/// connection should close.
async fn read_with_deadline<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    deadline: Option<std::time::Instant>,
) -> Option<CloseReason>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let read = stream.read_buf(buf);
    let result = match deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Some(CloseReason::Timeout);
            }
            match tokio::time::timeout(remaining, read).await {
                Ok(result) => result,
                Err(_) => return Some(CloseReason::Timeout),
            }
        }
        None => read.await,
    };
    match result {
        Ok(0) => Some(CloseReason::Eof),
        Ok(_) => None,
        Err(_) => Some(CloseReason::IoError),
    }
}
