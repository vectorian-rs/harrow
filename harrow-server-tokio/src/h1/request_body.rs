use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncRead, AsyncReadExt};

use harrow_codec_h1::{ParsedRequest, PayloadDecoder, PayloadItem};
use harrow_core::request::Body;
use harrow_server::h1::{ErrorResponse, RequestBodyProgress};

use crate::ServerConfig;

const MAX_REQUEST_BODY_BUFFER_SIZE: usize = 32 * 1024;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum QueueStatus {
    Ready,
    Dropped,
}

pub(crate) struct RequestBodyState {
    decoder: Option<PayloadDecoder>,
    sender: Option<PayloadSender>,
    body_deadline: Option<Instant>,
    max_body_size: usize,
}

impl RequestBodyState {
    fn finished(max_body_size: usize) -> Self {
        Self {
            decoder: None,
            sender: None,
            body_deadline: None,
            max_body_size,
        }
    }

    pub(crate) fn start(parsed: &ParsedRequest, config: &ServerConfig) -> (Self, Body) {
        let Some(decoder) = PayloadDecoder::from_parsed(parsed) else {
            return (Self::finished(config.max_body_size), empty_body());
        };

        let (sender, body) = payload_channel(MAX_REQUEST_BODY_BUFFER_SIZE);

        (
            Self {
                decoder: Some(decoder),
                sender: Some(sender),
                body_deadline: config.body_read_timeout.map(|t| Instant::now() + t),
                max_body_size: config.max_body_size,
            },
            body,
        )
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.decoder.is_none()
    }

    pub(crate) fn abort(&mut self) {
        self.decoder = None;
        self.sender = None;
    }

    pub(crate) fn detach_receiver(&mut self) {
        self.sender = None;
    }

    pub(crate) async fn pump_once<S>(
        &mut self,
        stream: &mut S,
        buf: &mut BytesMut,
    ) -> RequestBodyProgress
    where
        S: AsyncRead + Unpin,
    {
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
                decoder.decode(buf, Some(self.max_body_size))
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

            if let Some(deadline) = self.body_deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return self.finish_response_error(ErrorResponse::RequestTimeout);
                }
                match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                    Ok(Ok(0)) => {
                        return self.finish_connection_closed();
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => return self.finish_connection_closed(),
                    Err(_) => {
                        return self.finish_response_error(ErrorResponse::RequestTimeout);
                    }
                }
            } else {
                match stream.read_buf(buf).await {
                    Ok(0) => {
                        return self.finish_connection_closed();
                    }
                    Ok(_) => {}
                    Err(_) => return self.finish_connection_closed(),
                }
            }
        }
    }

    pub(crate) async fn drain_to_eof<S>(&mut self, stream: &mut S, buf: &mut BytesMut)
    where
        S: AsyncRead + Unpin,
    {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };

        loop {
            match decoder.decode(buf, Some(self.max_body_size)) {
                Err(_) => {
                    self.abort();
                    return;
                }
                Ok(Some(PayloadItem::Chunk(_))) => continue,
                Ok(Some(PayloadItem::Eof)) => {
                    self.finish_eof();
                    return;
                }
                Ok(None) => {}
            }

            if let Some(deadline) = self.body_deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.abort();
                    return;
                }
                match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                        self.abort();
                        return;
                    }
                    Ok(Ok(_)) => {}
                }
            } else {
                match stream.read_buf(buf).await {
                    Ok(0) | Err(_) => {
                        self.abort();
                        return;
                    }
                    Ok(_) => {}
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

fn payload_channel(max_buffered_bytes: usize) -> (PayloadSender, Body) {
    let inner = Rc::new(PayloadInner::new(max_buffered_bytes));
    let sender = PayloadSender {
        inner: Rc::downgrade(&inner),
    };
    let body = Body::new(PayloadBody { inner });
    (sender, body)
}

struct PayloadBody {
    inner: Rc<PayloadInner>,
}

impl Drop for PayloadBody {
    fn drop(&mut self) {
        self.inner.receiver_dropped.set(true);
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

        if self.inner.closed.get() {
            return Poll::Ready(None);
        }

        self.inner.register_receiver(cx.waker());

        if let Some(chunk) = self.inner.pop_chunk() {
            Poll::Ready(Some(Ok(Frame::data(chunk))))
        } else if self.inner.closed.get() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.closed.get() && self.inner.buffered_bytes.get() == 0
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

        if inner.receiver_dropped.get() {
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
            inner.closed.set(true);
            inner.wake_receiver();
            inner.wake_sender();
        }
    }
}

struct PayloadInner {
    buffered_bytes: Cell<usize>,
    closed: Cell<bool>,
    receiver_dropped: Cell<bool>,
    chunks: RefCell<VecDeque<Bytes>>,
    max_buffered_bytes: usize,
    receiver_waker: RefCell<Option<Waker>>,
    sender_waker: RefCell<Option<Waker>>,
}

impl PayloadInner {
    fn new(max_buffered_bytes: usize) -> Self {
        Self {
            buffered_bytes: Cell::new(0),
            closed: Cell::new(false),
            receiver_dropped: Cell::new(false),
            chunks: RefCell::new(VecDeque::new()),
            max_buffered_bytes,
            receiver_waker: RefCell::new(None),
            sender_waker: RefCell::new(None),
        }
    }

    fn available_capacity(&self) -> usize {
        self.max_buffered_bytes
            .saturating_sub(self.buffered_bytes.get())
    }

    fn push_chunk(&self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }

        self.buffered_bytes
            .set(self.buffered_bytes.get().saturating_add(chunk.len()));
        self.chunks.borrow_mut().push_back(chunk);
        self.wake_receiver();
    }

    fn pop_chunk(&self) -> Option<Bytes> {
        let chunk = self.chunks.borrow_mut().pop_front()?;
        self.buffered_bytes
            .set(self.buffered_bytes.get().saturating_sub(chunk.len()));
        self.wake_sender();
        Some(chunk)
    }

    fn register_receiver(&self, waker: &Waker) {
        Self::register_waker(&self.receiver_waker, waker);
    }

    fn register_sender(&self, waker: &Waker) {
        Self::register_waker(&self.sender_waker, waker);
    }

    fn register_waker(slot: &RefCell<Option<Waker>>, waker: &Waker) {
        let mut slot = slot.borrow_mut();
        let should_replace = slot.as_ref().is_none_or(|stored| !stored.will_wake(waker));
        if should_replace {
            *slot = Some(waker.clone());
        }
    }

    fn wake_receiver(&self) {
        if let Some(waker) = self.receiver_waker.borrow_mut().take() {
            waker.wake();
        }
    }

    fn wake_sender(&self) {
        if let Some(waker) = self.sender_waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

fn empty_body() -> Body {
    Body::new(Full::new(Bytes::new()).map_err(|e| -> BoxError { match e {} }))
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::time::Duration;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
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

    #[tokio::test(flavor = "current_thread")]
    async fn payload_channel_reports_receiver_drop() {
        let (sender, body) = payload_channel(8);
        drop(body);

        assert_eq!(sender.ready().await, QueueStatus::Dropped);
    }
}
