use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use monoio::io::AsyncWriteRentExt;
use monoio::net::TcpStream;

use harrow_codec_h1::{CONTINUE_100, ParsedRequest, PayloadDecoder, PayloadItem};
use harrow_core::dispatch::SharedState;
use harrow_core::request::Body;
use harrow_server::h1::{ErrorResponse, RequestBodyProgress};

use crate::buffer::DEFAULT_BUFFER_SIZE;
use crate::h1::dispatcher::H1Connection;
use crate::protocol::{self, ProtocolError};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const MAX_REQUEST_BODY_BUFFER_SIZE: usize = 32 * 1024;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum QueueStatus {
    Ready,
    Dropped,
}

pub(crate) struct RequestBodyState {
    decoder: Option<PayloadDecoder>,
    sender: Option<PayloadSender>,
    max_body_size: usize,
}

impl RequestBodyState {
    fn finished(max_body_size: usize) -> Self {
        Self {
            decoder: None,
            sender: None,
            max_body_size,
        }
    }

    pub(crate) async fn start(
        stream: &mut TcpStream,
        parsed: &ParsedRequest,
        max_body_size: usize,
    ) -> Result<(Self, Body), std::io::Error> {
        let Some(decoder) = PayloadDecoder::from_parsed(parsed) else {
            return Ok((
                Self::finished(max_body_size),
                protocol::body_from_bytes(Bytes::new()),
            ));
        };

        if parsed.expect_continue {
            let (result, _) = stream.write_all(CONTINUE_100.to_vec()).await;
            result?;
        }

        let (sender, body) = payload_channel(MAX_REQUEST_BODY_BUFFER_SIZE);

        Ok((
            Self {
                decoder: Some(decoder),
                sender: Some(sender),
                max_body_size,
            },
            body,
        ))
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.decoder.is_none()
    }

    pub(crate) fn abort(&mut self) {
        self.decoder = None;
        self.sender = None;
    }

    pub(crate) async fn pump_once(&mut self, conn: &mut H1Connection) -> RequestBodyProgress {
        if self.decoder.is_none() {
            return RequestBodyProgress::Eof;
        }

        loop {
            match self.wait_for_capacity().await {
                QueueStatus::Ready => {}
                QueueStatus::Dropped => {
                    self.abort();
                    return RequestBodyProgress::ReceiverClosed;
                }
            }

            let decode = {
                let decoder = self.decoder.as_mut().expect("decoder checked above");
                decoder.decode(&mut conn.buf, Some(self.max_body_size))
            };

            match decode {
                Err(err) => {
                    return self.finish_response_error(ErrorResponse::from_codec_error(&err));
                }
                Ok(Some(PayloadItem::Chunk(chunk))) => {
                    return self.send_chunk(chunk).await;
                }
                Ok(Some(PayloadItem::Eof)) => {
                    self.finish_eof();
                    return RequestBodyProgress::Eof;
                }
                Ok(None) => {}
            }

            let timeout = match conn.effective_read_timeout(conn.config.body_read_timeout) {
                Ok(timeout) => timeout,
                Err(ProtocolError::Timeout) => {
                    return self.finish_response_error(ErrorResponse::RequestTimeout);
                }
                Err(_) => return self.finish_connection_closed(),
            };

            match conn.read_more(DEFAULT_BUFFER_SIZE, timeout).await {
                Ok(0) => {
                    return self.finish_response_error(ErrorResponse::BadRequest);
                }
                Ok(_) => {}
                Err(ProtocolError::Timeout) => {
                    return self.finish_response_error(ErrorResponse::RequestTimeout);
                }
                Err(ProtocolError::BodyTooLarge) => {
                    return self.finish_response_error(ErrorResponse::PayloadTooLarge);
                }
                Err(ProtocolError::Parse(_) | ProtocolError::ProtocolViolation(_)) => {
                    return self.finish_response_error(ErrorResponse::BadRequest);
                }
                Err(ProtocolError::Io(_) | ProtocolError::StreamClosed) => {
                    return self.finish_connection_closed();
                }
            }
        }
    }

    async fn wait_for_capacity(&self) -> QueueStatus {
        let Some(sender) = self.sender.as_ref() else {
            return QueueStatus::Dropped;
        };

        sender.ready().await
    }

    async fn send_chunk(&mut self, mut chunk: Bytes) -> RequestBodyProgress {
        let Some(sender) = self.sender.as_ref() else {
            self.abort();
            return RequestBodyProgress::ReceiverClosed;
        };

        while !chunk.is_empty() {
            match sender.ready().await {
                QueueStatus::Ready => {}
                QueueStatus::Dropped => {
                    self.abort();
                    return RequestBodyProgress::ReceiverClosed;
                }
            }

            let capacity = sender.available_capacity();
            if capacity == 0 {
                continue;
            }

            let emitted = chunk.len().min(capacity);
            let next = if emitted == chunk.len() {
                std::mem::take(&mut chunk)
            } else {
                chunk.split_to(emitted)
            };
            sender.feed_data(next);
        }

        RequestBodyProgress::Progress
    }

    fn finish_response_error(&mut self, error: ErrorResponse) -> RequestBodyProgress {
        self.abort();
        RequestBodyProgress::ResponseError(error)
    }

    fn finish_connection_closed(&mut self) -> RequestBodyProgress {
        self.abort();
        RequestBodyProgress::ConnectionClosed
    }

    fn finish_eof(&mut self) {
        self.decoder = None;
        if let Some(sender) = self.sender.take() {
            sender.feed_eof();
        }
    }
}

pub(crate) async fn dispatch_request(
    shared: Arc<SharedState>,
    parsed: &ParsedRequest,
    body: Body,
) -> http::Response<harrow_core::response::ResponseBody> {
    harrow_server::h1::dispatch_parsed_request(shared, parsed, body).await
}

fn payload_channel(max_buffered_bytes: usize) -> (PayloadSender, Body) {
    let inner = Arc::new(PayloadInner::new(max_buffered_bytes));
    let sender = PayloadSender {
        inner: Arc::downgrade(&inner),
    };
    let body = Body::new(PayloadBody { inner });
    (sender, body)
}

struct PayloadBody {
    inner: Arc<PayloadInner>,
}

impl Drop for PayloadBody {
    fn drop(&mut self) {
        self.inner.receiver_dropped.store(true, Ordering::Release);
        self.inner.wake_sender();
    }
}

impl HttpBody for PayloadBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(chunk) = self.inner.pop_chunk() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }

        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }

        self.inner.register_receiver(cx.waker());

        if let Some(chunk) = self.inner.pop_chunk() {
            Poll::Ready(Some(Ok(Frame::data(chunk))))
        } else if self.inner.closed.load(Ordering::Acquire) {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
            && self.inner.buffered_bytes.load(Ordering::Acquire) == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

struct PayloadSender {
    inner: Weak<PayloadInner>,
}

impl PayloadSender {
    async fn ready(&self) -> QueueStatus {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<QueueStatus> {
        let Some(inner) = self.inner.upgrade() else {
            return Poll::Ready(QueueStatus::Dropped);
        };

        if inner.receiver_dropped.load(Ordering::Acquire) {
            Poll::Ready(QueueStatus::Dropped)
        } else if inner.available_capacity() > 0 {
            Poll::Ready(QueueStatus::Ready)
        } else {
            inner.register_sender(cx.waker());
            Poll::Pending
        }
    }

    fn available_capacity(&self) -> usize {
        self.inner
            .upgrade()
            .map_or(0, |inner| inner.available_capacity())
    }

    fn feed_data(&self, data: Bytes) {
        if let Some(inner) = self.inner.upgrade() {
            inner.push_chunk(data);
        }
    }

    fn feed_eof(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.closed.store(true, Ordering::Release);
            inner.wake_receiver();
            inner.wake_sender();
        }
    }
}

struct PayloadInner {
    buffered_bytes: AtomicUsize,
    closed: AtomicBool,
    receiver_dropped: AtomicBool,
    chunks: Mutex<VecDeque<Bytes>>,
    max_buffered_bytes: usize,
    receiver_waker: Mutex<Option<Waker>>,
    sender_waker: Mutex<Option<Waker>>,
}

impl PayloadInner {
    fn new(max_buffered_bytes: usize) -> Self {
        Self {
            buffered_bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            receiver_dropped: AtomicBool::new(false),
            chunks: Mutex::new(VecDeque::new()),
            max_buffered_bytes,
            receiver_waker: Mutex::new(None),
            sender_waker: Mutex::new(None),
        }
    }

    fn available_capacity(&self) -> usize {
        self.max_buffered_bytes
            .saturating_sub(self.buffered_bytes.load(Ordering::Acquire))
    }

    fn push_chunk(&self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }

        self.buffered_bytes.fetch_add(chunk.len(), Ordering::AcqRel);
        self.chunks
            .lock()
            .expect("payload chunk lock")
            .push_back(chunk);
        self.wake_receiver();
    }

    fn pop_chunk(&self) -> Option<Bytes> {
        let chunk = self
            .chunks
            .lock()
            .expect("payload chunk lock")
            .pop_front()?;
        self.buffered_bytes.fetch_sub(chunk.len(), Ordering::AcqRel);
        self.wake_sender();
        Some(chunk)
    }

    fn register_receiver(&self, waker: &Waker) {
        Self::register_waker(&self.receiver_waker, waker);
    }

    fn register_sender(&self, waker: &Waker) {
        Self::register_waker(&self.sender_waker, waker);
    }

    fn register_waker(slot: &Mutex<Option<Waker>>, waker: &Waker) {
        let mut slot = slot.lock().expect("payload waker lock");
        let should_replace = slot.as_ref().is_none_or(|stored| !stored.will_wake(waker));
        if should_replace {
            *slot = Some(waker.clone());
        }
    }

    fn wake_receiver(&self) {
        if let Some(waker) = self
            .receiver_waker
            .lock()
            .expect("payload waker lock")
            .take()
        {
            waker.wake();
        }
    }

    fn wake_sender(&self) {
        if let Some(waker) = self.sender_waker.lock().expect("payload waker lock").take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn payload_channel_backpressures_by_buffered_bytes() {
        let (sender, mut body) = payload_channel(8);

        assert_eq!(sender.ready().await, QueueStatus::Ready);
        sender.feed_data(Bytes::from_static(b"abcd"));
        sender.feed_data(Bytes::from_static(b"efgh"));

        let blocked = tokio::time::timeout(Duration::from_millis(25), sender.ready()).await;
        assert!(
            blocked.is_err(),
            "sender should block when the queue is full"
        );

        let first = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .expect("expected first frame")
            .expect("expected frame result")
            .into_data()
            .expect("expected data frame");
        assert_eq!(first, Bytes::from_static(b"abcd"));

        assert_eq!(sender.ready().await, QueueStatus::Ready);
    }
}
