use std::time::{Duration, Instant};

use monoio::io::{AsyncReadRent, CancelableAsyncReadRent, Canceller};

use crate::buffer::{DEFAULT_BUFFER_SIZE, acquire_buffer, release_buffer};
use crate::codec;
use crate::h1::dispatcher::H1Connection;
use crate::protocol::ProtocolError;

/// Maximum size of the header read buffer (64 KiB).
const MAX_HEADER_BUF: usize = 64 * 1024;

impl H1Connection {
    /// Read HTTP headers from the stream into `buf`.
    ///
    /// Uses a wall-clock deadline for the entire header read phase to prevent
    /// Slowloris attacks (trickling bytes to keep per-read timeouts from firing).
    ///
    /// # Cancellation Safety
    /// This function uses `cancelable_read` to ensure that when a timeout fires,
    /// the kernel operation is explicitly cancelled before the buffer is dropped.
    pub(crate) async fn read_headers(&mut self) -> Result<codec::ParsedRequest, ProtocolError> {
        loop {
            match codec::try_parse_request(&self.buf) {
                Ok(parsed) => {
                    let _ = self.buf.split_to(parsed.header_len);
                    return Ok(parsed);
                }
                Err(codec::CodecError::Incomplete) => {}
                Err(codec::CodecError::Invalid(msg)) => {
                    return Err(ProtocolError::Parse(msg));
                }
                Err(codec::CodecError::BodyTooLarge) => {
                    return Err(ProtocolError::BodyTooLarge);
                }
            }

            if self.buf.len() >= MAX_HEADER_BUF {
                return Err(ProtocolError::ProtocolViolation(
                    "request headers too large".into(),
                ));
            }

            let n = self
                .read_more(
                    DEFAULT_BUFFER_SIZE,
                    self.effective_read_timeout(self.config.header_read_timeout)?,
                )
                .await?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Err(ProtocolError::StreamClosed);
                }
                return Err(ProtocolError::Parse(
                    "unexpected eof during header read".into(),
                ));
            }
        }
    }

    pub(crate) fn check_connection_deadline(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self
            .connection_deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            tracing::warn!("connection timed out");
            return Err(Box::new(ProtocolError::Timeout));
        }
        Ok(())
    }

    pub(crate) fn effective_read_timeout(
        &self,
        phase_timeout: Option<Duration>,
    ) -> Result<Option<Duration>, ProtocolError> {
        let connection_timeout = match self.connection_deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(remaining) => Some(remaining),
                None => return Err(ProtocolError::Timeout),
            },
            None => None,
        };

        Ok(match (phase_timeout, connection_timeout) {
            (Some(phase), Some(connection)) => Some(phase.min(connection)),
            (Some(phase), None) => Some(phase),
            (None, Some(connection)) => Some(connection),
            (None, None) => None,
        })
    }

    pub(crate) async fn read_more(
        &mut self,
        min_capacity: usize,
        timeout: Option<Duration>,
    ) -> Result<usize, ProtocolError> {
        let read_buf = acquire_buffer(min_capacity);
        let (result, read_buf) = if let Some(timeout) = timeout {
            let canceller = Canceller::new();
            let handle = canceller.handle();
            let recv_fut = self.stream.cancelable_read(read_buf, handle);
            let mut recv_fut = std::pin::pin!(recv_fut);

            monoio::select! {
                result = &mut recv_fut => result,
                _ = monoio::time::sleep(timeout) => {
                    let _ = canceller.cancel();
                    let (_, read_buf) = recv_fut.await;
                    release_buffer(read_buf);
                    return Err(ProtocolError::Timeout);
                }
            }
        } else {
            self.stream.read(read_buf).await
        };

        let n = match result {
            Ok(n) => n,
            Err(err) => {
                release_buffer(read_buf);
                return Err(ProtocolError::Io(err));
            }
        };

        if n > 0 {
            self.buf.extend_from_slice(&read_buf[..n]);
        }
        release_buffer(read_buf);
        Ok(n)
    }
}
