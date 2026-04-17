//! Connection state machine for the meguri server.
//!
//! Each connection tracks its own read buffer, parse state, request-body pump,
//! and response serialization. The main loop drives transitions via CQE
//! completions plus periodic dispatch wake ticks.
//!
//! This module is platform-independent (no io_uring dependency) so the
//! FSM can be unit-tested on macOS.

#![allow(dead_code)] // FSM types are only used by the Linux event loop

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use tokio::sync::mpsc;

use harrow_codec_h1::{self as codec, CodecError, ParsedRequest, PayloadDecoder, PayloadItem};
use harrow_core::request::Body;
use harrow_server::h1::{
    EarlyResponseMode, ErrorResponse, ResponseBodyMode, build_request, early_response_control,
    finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
    request_exceeds_body_limit,
};

const MAX_REQUEST_BODY_BUFFER_SIZE: usize = 32 * 1024;
const MAX_RESPONSE_BUFFER_SIZE: usize = 32 * 1024;
const RESPONSE_STREAM_CHUNK_SIZE: usize = 8 * 1024;
const RESPONSE_STREAM_CAPACITY: usize = MAX_RESPONSE_BUFFER_SIZE / RESPONSE_STREAM_CHUNK_SIZE;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Connection states in the lifecycle.
#[derive(Debug)]
pub(crate) enum ConnState {
    /// Waiting for header bytes to arrive via RECV completion.
    Headers,
    /// Request has been handed to Harrow and may still be pumping body bytes.
    Dispatching,
    /// Writing serialized response bytes to the socket.
    Writing,
    /// Connection closed; waiting for removal from slab.
    #[allow(dead_code)]
    Closed,
}

pub(crate) enum ResponseStreamEvent {
    Data(Bytes),
    Done,
    Error,
}

pub(crate) enum ResponseProgress {
    Pending,
    WriteReady,
    Complete,
    StartError,
    StreamError,
}

type DispatchResult = Result<http::Response<harrow_core::response::ResponseBody>, ()>;

pub(crate) struct DispatchHandle {
    inner: Rc<DispatchInner>,
}

pub(crate) struct DispatchSender {
    inner: Weak<DispatchInner>,
}

struct DispatchInner {
    result: RefCell<Option<DispatchResult>>,
}

/// Per-connection state.
pub(crate) struct Conn {
    pub fd: RawFd,
    pub state: ConnState,
    /// Read buffer: holds raw bytes from RECV completions.
    pub buf: BytesMut,
    /// Parsed request headers for the active request.
    pub parsed: Option<ParsedRequest>,
    /// Live request body pump for streaming request payloads into the handler.
    request_body: Option<RequestBodyState>,
    /// Request body handed to Harrow when dispatch starts.
    pending_request_body: Option<Body>,
    /// Completion slot for the spawned local dispatch task.
    dispatch_handle: Option<DispatchHandle>,
    /// Response byte stream produced by the spawned local dispatch task.
    response_rx: Option<mpsc::Receiver<ResponseStreamEvent>>,
    /// Serialized response bytes to write.
    pub response_buf: Vec<u8>,
    /// Number of response bytes already written.
    pub response_written: usize,
    /// Whether the response stream yielded at least one byte chunk.
    response_started: bool,
    /// Whether the response stream finished cleanly.
    response_done: bool,
    /// Whether the response stream failed after starting.
    response_failed: bool,
    /// Whether to keep-alive after this request.
    pub keep_alive: bool,
    /// Whether there is a pending RECV SQE for this connection.
    pub recv_pending: bool,
    /// Whether there is a pending WRITE SQE for this connection.
    pub write_pending: bool,
    /// When this connection was accepted.
    pub accepted_at: Instant,
    /// When the current request started (reset on keep-alive).
    pub request_started_at: Instant,
}

/// Result of processing bytes from a RECV completion.
pub(crate) enum ProcessResult {
    /// Submit a RECV SQE (need more header bytes).
    NeedRecv,
    /// Dispatch the request through Harrow.
    Dispatch,
    /// Write a serialized error response.
    WriteError(Vec<u8>),
    /// Connection should be closed (clean close or error).
    Close,
}

/// Result of pumping request-body bytes into the live request body queue.
pub(crate) enum BodyPumpResult {
    /// More bytes are needed from the socket.
    NeedRecv,
    /// The in-memory body buffer is full; stop reading until the handler drains it.
    Blocked,
    /// Reached request-body EOF.
    Eof,
    /// The handler dropped the request body before EOF.
    ReceiverClosed,
    /// The request body is malformed or exceeded policy.
    ResponseError(ErrorResponse),
}

/// Result of processing a WRITE completion.
pub(crate) enum WriteResult {
    /// Submit another WRITE SQE (more bytes to send).
    WriteMore,
    /// Wait for the response producer to yield the next bytes.
    AwaitResponse,
    /// Submit a RECV SQE (keep-alive, start next request).
    RecvNext,
    /// Connection should be closed.
    Close,
}

impl Conn {
    pub fn new(fd: RawFd) -> Self {
        let now = Instant::now();
        Self {
            fd,
            state: ConnState::Headers,
            buf: BytesMut::with_capacity(codec::DEFAULT_BUFFER_SIZE),
            parsed: None,
            request_body: None,
            pending_request_body: None,
            dispatch_handle: None,
            response_rx: None,
            response_buf: Vec::new(),
            response_written: 0,
            response_started: false,
            response_done: false,
            response_failed: false,
            keep_alive: true,
            recv_pending: false,
            write_pending: false,
            accepted_at: now,
            request_started_at: now,
        }
    }

    /// Process bytes from a RECV completion while waiting for request headers.
    pub fn on_recv(&mut self, nbytes: usize, max_body: usize) -> ProcessResult {
        if nbytes == 0 {
            return ProcessResult::Close;
        }

        match self.state {
            ConnState::Headers => self.process_headers(max_body),
            ConnState::Dispatching => ProcessResult::Dispatch,
            _ => ProcessResult::Close,
        }
    }

    fn process_headers(&mut self, max_body: usize) -> ProcessResult {
        match codec::try_parse_request(&self.buf) {
            Ok(parsed) => {
                let header_len = parsed.header_len;
                let content_length = parsed.content_length;

                self.buf.advance(header_len);

                if request_exceeds_body_limit(content_length, max_body) {
                    return ProcessResult::WriteError(wire_error(
                        ErrorResponse::PayloadTooLarge,
                        false,
                    ));
                }

                self.keep_alive = parsed.keep_alive;
                self.request_body = None;
                self.pending_request_body = None;

                let has_body = content_length.is_some_and(|cl| cl > 0) || parsed.chunked;
                if has_body {
                    let (request_body, body) = RequestBodyState::start(&parsed);
                    self.request_body = Some(request_body);
                    self.pending_request_body = Some(body);
                } else {
                    self.pending_request_body = Some(empty_body());
                }

                self.parsed = Some(parsed);
                self.state = ConnState::Dispatching;
                ProcessResult::Dispatch
            }
            Err(CodecError::Incomplete) => {
                if self.buf.len() >= codec::MAX_HEADER_BUF {
                    return ProcessResult::WriteError(wire_error(
                        ErrorResponse::RequestHeadersTooLarge,
                        false,
                    ));
                }
                ProcessResult::NeedRecv
            }
            Err(CodecError::Invalid(_)) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::BadRequest, false))
            }
            Err(CodecError::BodyTooLarge) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::PayloadTooLarge, false))
            }
        }
    }

    pub fn build_harrow_request(&mut self) -> Option<http::Request<Body>> {
        let parsed = self.parsed.as_ref()?;
        let body = self.pending_request_body.take()?;
        build_request(parsed, body).ok()
    }

    pub fn set_dispatch_handle(&mut self, handle: DispatchHandle) {
        self.dispatch_handle = Some(handle);
    }

    pub fn poll_dispatch_result(&mut self) -> Option<DispatchResult> {
        let result = self.dispatch_handle.as_ref()?.take();
        if result.is_some() {
            self.dispatch_handle = None;
        }
        result
    }

    pub fn set_response_receiver(&mut self, rx: mpsc::Receiver<ResponseStreamEvent>) {
        self.response_rx = Some(rx);
        self.response_started = false;
        self.response_done = false;
        self.response_failed = false;
    }

    pub fn poll_response_stream(&mut self) -> ResponseProgress {
        if !self.response_buf.is_empty() && self.response_written < self.response_buf.len() {
            return ResponseProgress::WriteReady;
        }

        loop {
            let Some(rx) = self.response_rx.as_mut() else {
                return if self.response_done {
                    ResponseProgress::Complete
                } else if self.response_failed {
                    ResponseProgress::StreamError
                } else {
                    ResponseProgress::Pending
                };
            };

            match rx.try_recv() {
                Ok(ResponseStreamEvent::Data(chunk)) if chunk.is_empty() => continue,
                Ok(ResponseStreamEvent::Data(chunk)) => {
                    if matches!(self.state, ConnState::Dispatching)
                        && self.request_body_in_progress()
                    {
                        let control = early_response_control(EarlyResponseMode::DropRequestBody);
                        self.keep_alive = control.keep_alive;
                        self.abort_request_body();
                    }

                    self.response_buf.clear();
                    self.response_buf.extend_from_slice(&chunk);
                    self.response_written = 0;
                    self.response_started = true;
                    self.state = ConnState::Writing;
                    return ResponseProgress::WriteReady;
                }
                Ok(ResponseStreamEvent::Done) => {
                    self.response_rx = None;
                    self.response_done = true;
                    return if self.response_started {
                        ResponseProgress::Complete
                    } else {
                        ResponseProgress::StartError
                    };
                }
                Ok(ResponseStreamEvent::Error) => {
                    self.response_rx = None;
                    if self.response_started {
                        self.response_failed = true;
                        self.keep_alive = false;
                        return ResponseProgress::StreamError;
                    }
                    return ResponseProgress::StartError;
                }
                Err(mpsc::error::TryRecvError::Empty) => return ResponseProgress::Pending,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.response_rx = None;
                    return if self.response_done {
                        ResponseProgress::Complete
                    } else if self.response_started {
                        self.response_failed = true;
                        self.keep_alive = false;
                        ResponseProgress::StreamError
                    } else {
                        ResponseProgress::StartError
                    };
                }
            }
        }
    }

    pub fn has_active_dispatch(&self) -> bool {
        self.dispatch_handle.is_some()
    }

    pub fn has_active_response_stream(&self) -> bool {
        self.response_rx.is_some()
    }

    pub fn request_body_in_progress(&self) -> bool {
        self.request_body.is_some()
    }

    pub fn abort_request_body(&mut self) {
        self.request_body = None;
    }

    pub fn pump_request_body(&mut self, max_body: usize) -> BodyPumpResult {
        let Some(request_body) = self.request_body.as_mut() else {
            return BodyPumpResult::Eof;
        };

        match request_body.pump(&mut self.buf, max_body) {
            BodyPumpResult::Eof => {
                self.request_body = None;
                BodyPumpResult::Eof
            }
            BodyPumpResult::ReceiverClosed => {
                self.request_body = None;
                BodyPumpResult::ReceiverClosed
            }
            BodyPumpResult::ResponseError(error) => {
                self.request_body = None;
                BodyPumpResult::ResponseError(error)
            }
            other => other,
        }
    }

    /// Process a WRITE completion. Returns the next action.
    pub fn on_write(&mut self, nbytes: usize) -> WriteResult {
        self.response_written += nbytes;

        if self.response_written < self.response_buf.len() {
            return WriteResult::WriteMore;
        }

        self.response_buf.clear();
        self.response_written = 0;

        if self.response_done {
            self.finish_response()
        } else if self.response_failed {
            WriteResult::Close
        } else {
            WriteResult::AwaitResponse
        }
    }

    pub fn set_serialized_response(&mut self, response: Vec<u8>, keep_alive: bool) {
        self.response_buf = response;
        self.response_written = 0;
        self.response_started = true;
        self.response_done = true;
        self.response_failed = false;
        self.keep_alive = keep_alive;
        self.request_body = None;
        self.pending_request_body = None;
        self.dispatch_handle = None;
        self.response_rx = None;
        self.state = ConnState::Writing;
    }

    pub fn set_error_response(&mut self, error: ErrorResponse) {
        self.set_serialized_response(wire_error(error, false), false);
    }

    pub fn set_internal_server_error(&mut self) {
        self.set_serialized_response(
            b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 21\r\nconnection: close\r\n\r\ninternal server error".to_vec(),
            false,
        );
    }

    /// Reset connection state for the next request (keep-alive).
    fn reset(&mut self) {
        self.state = ConnState::Headers;
        self.parsed = None;
        self.request_body = None;
        self.pending_request_body = None;
        self.dispatch_handle = None;
        self.response_rx = None;
        self.response_buf.clear();
        self.response_written = 0;
        self.response_started = false;
        self.response_done = false;
        self.response_failed = false;
        self.keep_alive = true;
        self.request_started_at = Instant::now();
        // Don't clear `buf` — leftover bytes from a pipelined request stay available.
    }

    pub fn resume_after_keep_alive(&mut self, max_body: usize) -> ProcessResult {
        if self.buf.is_empty() {
            ProcessResult::NeedRecv
        } else {
            self.process_headers(max_body)
        }
    }

    fn finish_response(&mut self) -> WriteResult {
        if self.keep_alive {
            self.reset();
            WriteResult::RecvNext
        } else {
            WriteResult::Close
        }
    }

    /// Check whether the connection has exceeded its lifetime limit.
    pub fn is_expired(&self, max_lifetime: Option<Duration>) -> bool {
        max_lifetime.is_some_and(|d| self.accepted_at.elapsed() >= d)
    }

    pub fn read_timed_out_for_phase(
        &self,
        header_timeout: Option<Duration>,
        body_timeout: Option<Duration>,
    ) -> bool {
        let timeout = match self.state {
            ConnState::Headers => header_timeout,
            ConnState::Dispatching if self.request_body_in_progress() => body_timeout,
            _ => None,
        };

        timeout.is_some_and(|d| self.request_started_at.elapsed() >= d)
    }
}

impl DispatchHandle {
    fn take(&self) -> Option<DispatchResult> {
        self.inner.result.borrow_mut().take()
    }
}

impl DispatchSender {
    pub fn send(self, result: DispatchResult) {
        if let Some(inner) = self.inner.upgrade() {
            *inner.result.borrow_mut() = Some(result);
        }
    }
}

pub(crate) fn dispatch_slot() -> (DispatchSender, DispatchHandle) {
    let inner = Rc::new(DispatchInner {
        result: RefCell::new(None),
    });
    (
        DispatchSender {
            inner: Rc::downgrade(&inner),
        },
        DispatchHandle { inner },
    )
}

pub(crate) fn response_channel() -> (
    mpsc::Sender<ResponseStreamEvent>,
    mpsc::Receiver<ResponseStreamEvent>,
) {
    mpsc::channel(RESPONSE_STREAM_CAPACITY.max(1))
}

pub(crate) async fn stream_response(
    tx: mpsc::Sender<ResponseStreamEvent>,
    response: http::Response<harrow_core::response::ResponseBody>,
    keep_alive: bool,
    is_head_request: bool,
) {
    let completed = stream_response_inner(&tx, response, keep_alive, is_head_request)
        .await
        .is_ok();
    let terminal = if completed {
        ResponseStreamEvent::Done
    } else {
        ResponseStreamEvent::Error
    };
    let _ = tx.send(terminal).await;
}

async fn stream_response_inner(
    tx: &mpsc::Sender<ResponseStreamEvent>,
    response: http::Response<harrow_core::response::ResponseBody>,
    keep_alive: bool,
    is_head_request: bool,
) -> Result<(), ()> {
    let prepared = prepare_response(response, keep_alive, is_head_request).map_err(|_| ())?;
    let mut body = prepared.body;

    let head = codec::write_response_head(
        prepared.status,
        &prepared.headers,
        prepared.plan.is_chunked(),
    );
    if !send_response_bytes(tx, Bytes::from(head)).await {
        return Ok(());
    }

    match prepared.plan.mode {
        ResponseBodyMode::None => Ok(()),
        ResponseBodyMode::Fixed => {
            let mut written = 0usize;

            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| ())?;
                if let Ok(data) = frame.into_data()
                    && !data.is_empty()
                {
                    record_fixed_response_bytes(&mut written, &data, prepared.expected_len)
                        .map_err(|_| ())?;
                    if !send_response_bytes(tx, data).await {
                        return Ok(());
                    }
                }
            }

            finish_fixed_response_body(written, prepared.expected_len).map_err(|_| ())
        }
        ResponseBodyMode::Chunked => {
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| ())?;
                if let Ok(data) = frame.into_data()
                    && !data.is_empty()
                    && !send_response_bytes(tx, Bytes::from(codec::encode_chunk(&data))).await
                {
                    return Ok(());
                }
            }

            let _ = send_response_bytes(tx, Bytes::from_static(codec::CHUNK_TERMINATOR)).await;
            Ok(())
        }
    }
}

async fn send_response_bytes(tx: &mpsc::Sender<ResponseStreamEvent>, mut data: Bytes) -> bool {
    while !data.is_empty() {
        let next = if data.len() <= RESPONSE_STREAM_CHUNK_SIZE {
            std::mem::take(&mut data)
        } else {
            data.split_to(RESPONSE_STREAM_CHUNK_SIZE)
        };

        if tx.send(ResponseStreamEvent::Data(next)).await.is_err() {
            return false;
        }
    }

    true
}

struct RequestBodyState {
    decoder: PayloadDecoder,
    sender: PayloadSender,
    pending_chunk: Option<Bytes>,
}

enum FlushResult {
    Ready,
    Blocked,
    Dropped,
}

impl RequestBodyState {
    fn start(parsed: &ParsedRequest) -> (Self, Body) {
        let decoder = PayloadDecoder::from_parsed(parsed).expect("body-bearing request");
        let (sender, body) = payload_channel(MAX_REQUEST_BODY_BUFFER_SIZE);
        (
            Self {
                decoder,
                sender,
                pending_chunk: None,
            },
            body,
        )
    }

    fn pump(&mut self, buf: &mut BytesMut, max_body: usize) -> BodyPumpResult {
        loop {
            match self.flush_pending_chunk() {
                FlushResult::Ready => {}
                FlushResult::Blocked => return BodyPumpResult::Blocked,
                FlushResult::Dropped => return BodyPumpResult::ReceiverClosed,
            }

            match self.decoder.decode(buf, Some(max_body)) {
                Err(err) => {
                    return BodyPumpResult::ResponseError(ErrorResponse::from_codec_error(&err));
                }
                Ok(Some(PayloadItem::Chunk(chunk))) => {
                    self.pending_chunk = Some(chunk);
                }
                Ok(Some(PayloadItem::Eof)) => {
                    self.sender.feed_eof();
                    return BodyPumpResult::Eof;
                }
                Ok(None) => {
                    return BodyPumpResult::NeedRecv;
                }
            }
        }
    }

    fn flush_pending_chunk(&mut self) -> FlushResult {
        while let Some(chunk) = self.pending_chunk.as_mut() {
            if chunk.is_empty() {
                self.pending_chunk = None;
                continue;
            }

            if self.sender.is_dropped() {
                return FlushResult::Dropped;
            }

            let capacity = self.sender.available_capacity();
            if capacity == 0 {
                return FlushResult::Blocked;
            }

            let emitted = chunk.len().min(capacity);
            let next = if emitted == chunk.len() {
                self.pending_chunk.take().unwrap()
            } else {
                chunk.split_to(emitted)
            };
            self.sender.feed_data(next);
        }

        if self.sender.is_dropped() {
            FlushResult::Dropped
        } else {
            FlushResult::Ready
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
    fn available_capacity(&self) -> usize {
        self.inner
            .upgrade()
            .map_or(0, |inner| inner.available_capacity())
    }

    fn is_dropped(&self) -> bool {
        self.inner
            .upgrade()
            .is_none_or(|inner| inner.receiver_dropped.get())
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

fn wire_error(error: ErrorResponse, keep_alive: bool) -> Vec<u8> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        http::header::CONTENT_LENGTH,
        error.body().len().to_string().parse().unwrap(),
    );
    if !keep_alive {
        headers.insert(http::header::CONNECTION, "close".parse().unwrap());
    }

    let mut resp = codec::write_response_head(error.status(), &headers, false);
    resp.extend_from_slice(error.body().as_bytes());
    resp
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::pin::Pin;

    use super::*;

    fn new_conn() -> Conn {
        Conn::new(0)
    }

    fn collect_body(body: Body) -> Bytes {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                body.collect()
                    .await
                    .expect("body collect should succeed")
                    .to_bytes()
            })
    }

    fn collect_response_stream(
        response: http::Response<harrow_core::response::ResponseBody>,
        keep_alive: bool,
        is_head_request: bool,
    ) -> (Vec<u8>, bool) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let (tx, mut rx) = response_channel();
                stream_response(tx, response, keep_alive, is_head_request).await;

                let mut bytes = Vec::new();
                let mut saw_done = false;
                while let Some(event) = rx.recv().await {
                    match event {
                        ResponseStreamEvent::Data(chunk) => bytes.extend_from_slice(&chunk),
                        ResponseStreamEvent::Done => {
                            saw_done = true;
                            break;
                        }
                        ResponseStreamEvent::Error => break,
                    }
                }

                (bytes, saw_done)
            })
    }

    // --- Header parsing ---

    #[test]
    fn headers_simple_get() {
        let mut conn = new_conn();
        conn.buf
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::Dispatch));
        assert!(matches!(conn.state, ConnState::Dispatching));
        assert!(conn.keep_alive);
        assert!(!conn.request_body_in_progress());
    }

    #[test]
    fn headers_incomplete() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"GET / HTTP/1.1\r\nHost: loc");
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::NeedRecv));
        assert!(matches!(conn.state, ConnState::Headers));
    }

    #[test]
    fn headers_invalid() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"INVALID\r\n\r\n");
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::WriteError(_)));
    }

    #[test]
    fn headers_connection_close() {
        let mut conn = new_conn();
        conn.buf
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::Dispatch));
        assert!(!conn.keep_alive);
    }

    // --- Streaming request bodies ---

    #[test]
    fn content_length_body_streams_without_prebuffering() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::Dispatch));
        assert!(conn.request_body_in_progress());

        let request = conn.build_harrow_request().expect("request");
        assert!(matches!(conn.pump_request_body(1024), BodyPumpResult::Eof));
        assert_eq!(
            collect_body(request.into_body()),
            Bytes::from_static(b"hello")
        );
    }

    #[test]
    fn content_length_body_needs_more_bytes() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nhello",
        );
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::Dispatch));
        assert!(matches!(
            conn.pump_request_body(1024),
            BodyPumpResult::NeedRecv
        ));
        assert!(conn.request_body_in_progress());
    }

    #[test]
    fn chunked_body_preserves_pipelined_data() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\nGET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let result = conn.on_recv(conn.buf.len(), 1024);
        assert!(matches!(result, ProcessResult::Dispatch));
        let request = conn.build_harrow_request().expect("request");
        assert!(matches!(conn.pump_request_body(1024), BodyPumpResult::Eof));
        assert_eq!(
            collect_body(request.into_body()),
            Bytes::from_static(b"hello")
        );
        assert_eq!(
            conn.buf.as_ref(),
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
    }

    #[test]
    fn request_body_backpressures_by_buffered_bytes() {
        let oversized = "a".repeat(MAX_REQUEST_BODY_BUFFER_SIZE + 1);
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            format!(
                "POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
                oversized.len(),
                oversized
            )
            .as_bytes(),
        );
        let result = conn.on_recv(conn.buf.len(), MAX_REQUEST_BODY_BUFFER_SIZE + 1);
        assert!(matches!(result, ProcessResult::Dispatch));
        let request = conn.build_harrow_request().expect("request");

        let pump = conn.pump_request_body(MAX_REQUEST_BODY_BUFFER_SIZE + 1);
        assert!(matches!(pump, BodyPumpResult::Blocked));

        let mut body = request.into_body();
        let first = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                    .await
                    .expect("expected first frame")
                    .expect("frame should be ok")
                    .into_data()
                    .expect("expected data frame")
            });
        assert_eq!(first.len(), MAX_REQUEST_BODY_BUFFER_SIZE);

        assert!(matches!(
            conn.pump_request_body(MAX_REQUEST_BODY_BUFFER_SIZE + 1),
            BodyPumpResult::Eof
        ));
        let rest = collect_body(body);
        assert_eq!(rest, Bytes::from_static(b"a"));
    }

    #[test]
    fn content_length_too_large() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 9999\r\n\r\n",
        );
        let result = conn.on_recv(conn.buf.len(), 100);
        assert!(matches!(result, ProcessResult::WriteError(_)));
    }

    // --- Write completion ---

    #[test]
    fn write_complete_close() {
        let mut conn = new_conn();
        conn.response_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        conn.state = ConnState::Writing;
        conn.response_done = true;
        conn.keep_alive = false;
        let result = conn.on_write(conn.response_buf.len());
        assert!(matches!(result, WriteResult::Close));
    }

    #[test]
    fn write_complete_keep_alive() {
        let mut conn = new_conn();
        conn.response_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        conn.state = ConnState::Writing;
        conn.response_done = true;
        conn.keep_alive = true;
        let result = conn.on_write(conn.response_buf.len());
        assert!(matches!(result, WriteResult::RecvNext));
        assert!(matches!(conn.state, ConnState::Headers));
    }

    #[test]
    fn write_partial() {
        let mut conn = new_conn();
        conn.response_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        conn.state = ConnState::Writing;
        let result = conn.on_write(5);
        assert!(matches!(result, WriteResult::WriteMore));
        assert_eq!(conn.response_written, 5);
    }

    #[test]
    fn write_complete_waits_for_stream_when_response_not_done() {
        let mut conn = new_conn();
        conn.response_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        conn.state = ConnState::Writing;
        let result = conn.on_write(conn.response_buf.len());
        assert!(matches!(result, WriteResult::AwaitResponse));
    }

    #[test]
    fn resume_after_keep_alive_reuses_buffered_pipelined_request() {
        let mut conn = new_conn();
        conn.response_buf = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        conn.response_done = true;
        conn.state = ConnState::Writing;
        conn.keep_alive = true;
        conn.buf
            .extend_from_slice(b"GET /next HTTP/1.1\r\nHost: localhost\r\n\r\n");

        let result = conn.on_write(conn.response_buf.len());
        assert!(matches!(result, WriteResult::RecvNext));

        let next = conn.resume_after_keep_alive(1024);
        assert!(matches!(next, ProcessResult::Dispatch));
        assert!(matches!(conn.state, ConnState::Dispatching));
        assert_eq!(
            conn.parsed.as_ref().map(|parsed| parsed.uri.path()),
            Some("/next")
        );
    }

    #[test]
    fn stream_response_inserts_connection_close_before_serializing() {
        let response = harrow_core::response::Response::ok().into_inner();

        let (response, saw_done) = collect_response_stream(response, false, false);
        let response = String::from_utf8_lossy(&response);
        assert!(saw_done);
        assert!(response.contains("connection: close\r\n"));
        assert!(response.contains("transfer-encoding: chunked\r\n"));
    }

    #[test]
    fn stream_response_omits_chunked_for_bodyless_status() {
        let response = harrow_core::response::Response::ok()
            .status(http::StatusCode::NO_CONTENT.as_u16())
            .into_inner();

        let (response, saw_done) = collect_response_stream(response, true, false);
        let response = String::from_utf8_lossy(&response);
        assert!(saw_done);
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(!response.contains("transfer-encoding: chunked\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
    }

    #[test]
    fn stream_response_omits_head_body_bytes() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "5")
            .into_inner();

        let (response, saw_done) = collect_response_stream(response, true, true);
        let response = String::from_utf8_lossy(&response);
        assert!(saw_done);
        assert!(response.contains("content-length: 5\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
        assert!(!response.ends_with("\r\n\r\nhello"));
    }

    #[test]
    fn stream_response_fixed_length_mismatch_reports_error() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "10")
            .into_inner();

        let (response, saw_done) = collect_response_stream(response, true, false);
        let response = String::from_utf8_lossy(&response);
        assert!(!saw_done);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("content-length: 10\r\n"));
        assert!(response.ends_with("hello"));
    }

    #[test]
    fn read_timed_out_uses_header_timeout_for_headers_state() {
        let mut conn = new_conn();
        conn.request_started_at = Instant::now() - Duration::from_secs(2);
        assert!(
            conn.read_timed_out_for_phase(
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(10)),
            )
        );
    }

    #[test]
    fn read_timed_out_uses_body_timeout_for_body_phase() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nhello",
        );
        let _ = conn.on_recv(conn.buf.len(), 1024);
        conn.request_started_at = Instant::now() - Duration::from_secs(2);
        assert!(
            !conn.read_timed_out_for_phase(
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(3)),
            )
        );
        assert!(
            conn.read_timed_out_for_phase(
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(1)),
            )
        );
    }

    // --- EOF ---

    #[test]
    fn eof_closes() {
        let mut conn = new_conn();
        let result = conn.on_recv(0, 1024);
        assert!(matches!(result, ProcessResult::Close));
    }
}
