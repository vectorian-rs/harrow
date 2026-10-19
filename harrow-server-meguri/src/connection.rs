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
    /// The connection's socket, owned by this `Conn` from accept until the
    /// event loop's `close_conn` closes it and sets it to `-1`. `Conn` has no
    /// `Drop` impl, so dropping it does not close the fd.
    pub fd: RawFd,
    pub state: ConnState,
    /// Read buffer: holds raw bytes from RECV completions. While
    /// `recv_pending`, an in-flight RECV writes into its spare capacity, so
    /// it must not be reallocated, and the `Conn` must not be dropped.
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
    /// Serialized response bytes to write. While `write_pending`, an
    /// in-flight SEND reads `response_buf[response_written..]`, so it must
    /// not be modified or replaced, and the `Conn` must not be dropped.
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
    /// Whether there is a pending RECV SQE for this connection. Set when the
    /// SQE is queued and cleared only by its CQE, so it is exactly "the
    /// kernel may still write into `buf`".
    pub recv_pending: bool,
    /// Whether there is a pending SEND SQE for this connection. Set when the
    /// SQE is queued and cleared only by its CQE ("the kernel may still read
    /// `response_buf`").
    pub write_pending: bool,
    /// Timeout error waiting to be translated into a wire response.
    timeout_error: Option<ErrorResponse>,
    /// When this connection was accepted.
    pub accepted_at: Instant,
    /// When the current request started (reset on keep-alive).
    pub request_started_at: Instant,
    /// Generation counter for lazy timeout-heap invalidation.
    pub deadline_generation: u64,
    /// Most recently tracked deadline for this connection.
    pub tracked_deadline: Option<Instant>,
    /// Tells this connection apart from earlier ones that used the same
    /// slab slot. Encoded into the `user_data` of every SQE for it.
    pub generation: u32,
    /// The spare capacity of `buf` handed to the in-flight RECV.
    recv_target: Option<RecvTarget>,
}

/// Where an in-flight RECV writes: the start of `buf`'s spare capacity at
/// submission time, and how many bytes the SQE allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecvTarget {
    addr: usize,
    len: usize,
}

/// `buf` was reallocated or reshaped while a RECV was writing into it, so the
/// received bytes are not where the buffer expects them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecvBufferMoved;

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
            timeout_error: None,
            accepted_at: now,
            request_started_at: now,
            deadline_generation: 0,
            tracked_deadline: None,
            generation: 0,
            recv_target: None,
        }
    }

    /// Whether an SQE that points into this connection's buffers is still
    /// in flight. While it is, the `Conn` (and so `buf` and `response_buf`)
    /// must stay in the slab.
    pub fn has_io_in_flight(&self) -> bool {
        self.recv_pending || self.write_pending
    }

    /// Make sure `buf` has at least `min_spare` bytes of spare capacity and
    /// return the region a RECV SQE may write into. The region is recorded
    /// so [`commit_recv`](Self::commit_recv) can check it did not move.
    ///
    /// Until the RECV completes, `buf` must not be reallocated or dropped:
    /// no `reserve`, `extend_from_slice`, `clear` + refill, or replacing it.
    pub fn prepare_recv(&mut self, min_spare: usize) -> (*mut u8, u32) {
        debug_assert!(
            !self.recv_pending,
            "second RECV submitted into the same buffer"
        );
        let spare = self.buf.capacity() - self.buf.len();
        if spare < min_spare {
            self.buf.reserve(min_spare - spare);
        }
        let spare = self.buf.spare_capacity_mut();
        let len = spare.len().min(u32::MAX as usize);
        let ptr = spare.as_mut_ptr().cast::<u8>();
        self.recv_target = Some(RecvTarget {
            addr: ptr as usize,
            len,
        });
        (ptr, len as u32)
    }

    /// Append the `received` bytes a RECV completion reports to `buf`.
    ///
    /// Returns the number of bytes appended, clamped to what the SQE allowed.
    /// Fails if the buffer no longer ends where the RECV wrote, which means
    /// it was reallocated under the kernel: the bytes are elsewhere, and
    /// exposing the spare capacity would expose uninitialised memory.
    pub fn commit_recv(&mut self, received: usize) -> Result<usize, RecvBufferMoved> {
        let target = self.recv_target.take().ok_or(RecvBufferMoved)?;
        let end = self.buf.as_ptr() as usize + self.buf.len();
        let spare = self.buf.capacity() - self.buf.len();
        if end != target.addr || spare < target.len {
            return Err(RecvBufferMoved);
        }
        // The kernel never reports more than the SQE length; clamp anyway so
        // a wrong `res` can never extend `len` past initialised bytes.
        let n = received.min(target.len);
        // SAFETY: the kernel wrote `n` bytes starting at `target.addr`, which
        // is still the first byte of `buf`'s spare capacity (checked above),
        // and `n <= target.len <= spare`, so the new length stays within the
        // allocation and covers only initialised bytes.
        unsafe { self.buf.set_len(self.buf.len() + n) };
        Ok(n)
    }

    /// Forget the in-flight RECV's target after a failed or ignored
    /// completion; nothing it wrote becomes part of `buf`.
    pub fn discard_recv(&mut self) {
        self.recv_target = None;
    }

    /// Mark the connection closed while SQEs may still be in flight for it.
    /// Drops the request/response plumbing but keeps `buf` and
    /// `response_buf`, which those SQEs may still reference.
    pub fn mark_closed(&mut self) {
        self.state = ConnState::Closed;
        self.request_body = None;
        self.pending_request_body = None;
        self.dispatch_handle = None;
        self.response_rx = None;
        self.timeout_error = None;
        self.keep_alive = false;
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
            Err(CodecError::Incomplete) => ProcessResult::NeedRecv,
            Err(CodecError::HeadersTooLarge) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::RequestHeadersTooLarge, false))
            }
            Err(CodecError::Invalid(_)) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::BadRequest, false))
            }
            Err(CodecError::BodyTooLarge) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::PayloadTooLarge, false))
            }
            Err(CodecError::ExpectationFailed) => {
                ProcessResult::WriteError(wire_error(ErrorResponse::ExpectationFailed, false))
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
                    debug_assert!(!self.write_pending, "response_buf refilled under a SEND");
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

    /// The unwritten part of `response_buf`, for a SEND SQE, or `None` if
    /// everything was written. `response_buf` must not be modified or
    /// replaced until that SEND completes (`write_pending`).
    pub fn prepare_write(&self) -> Option<(*const u8, u32)> {
        debug_assert!(
            !self.write_pending,
            "second SEND submitted for the same buffer"
        );
        let remaining = &self.response_buf[self.response_written..];
        if remaining.is_empty() {
            return None;
        }
        // A short send is handled by `on_write`, so clamping is safe.
        let len = remaining.len().min(u32::MAX as usize) as u32;
        Some((remaining.as_ptr(), len))
    }

    pub fn set_serialized_response(&mut self, response: Vec<u8>, keep_alive: bool) {
        debug_assert!(!self.write_pending, "response_buf replaced under a SEND");
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
        self.timeout_error = None;
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
        debug_assert!(
            !self.has_io_in_flight(),
            "keep-alive reset with I/O in flight"
        );
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
        self.timeout_error = None;
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

    pub fn body_read_timed_out(&self, body_timeout: Option<Duration>) -> bool {
        matches!(self.state, ConnState::Dispatching)
            && self.request_body_in_progress()
            && body_timeout.is_some_and(|d| self.request_started_at.elapsed() >= d)
    }

    pub fn arm_timeout_error(&mut self, error: ErrorResponse) {
        self.timeout_error = Some(error);
    }

    pub fn take_timeout_error(&mut self) -> Option<ErrorResponse> {
        self.timeout_error.take()
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
    fn unsupported_expectation_is_417() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(
            b"POST /x HTTP/1.1\r\nHost: h\r\nExpect: foo\r\nContent-Length: 1\r\n\r\nx",
        );
        let ProcessResult::WriteError(wire) = conn.on_recv(conn.buf.len(), 1024) else {
            panic!("expected an error response");
        };
        assert!(wire.starts_with(b"HTTP/1.1 417 Expectation Failed\r\n"));
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
        assert!(response.contains("content-length: 0\r\n"));
        assert!(!response.contains("transfer-encoding: chunked\r\n"));
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
        // A buffered body with a wrong content-length is rejected earlier, in
        // `prepare_response`; only a streaming body can end short mid-write.
        let response = harrow_core::response::Response::streaming(
            http::StatusCode::OK,
            futures_util::stream::iter(vec![Ok::<
                Frame<Bytes>,
                Box<dyn std::error::Error + Send + Sync>,
            >(Frame::data(Bytes::from_static(
                b"hello",
            )))]),
        )
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

    // --- RECV buffer bookkeeping ---

    /// Stand-in for the kernel: write `data` where the RECV SQE points.
    fn kernel_recv(ptr: *mut u8, len: u32, data: &[u8]) -> usize {
        let n = data.len().min(len as usize);
        // SAFETY: `ptr`/`len` come from `prepare_recv`, which points into the
        // buffer's spare capacity, and `n <= len`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, n) };
        n
    }

    #[test]
    fn recv_appends_after_existing_bytes() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"GET ");
        let (ptr, len) = conn.prepare_recv(64);
        assert!(len as usize >= 64);
        assert_eq!(ptr as usize, conn.buf.as_ptr() as usize + 4);

        let n = kernel_recv(ptr, len, b"/ HTTP/1.1");
        assert_eq!(conn.commit_recv(n), Ok(10));
        assert_eq!(&conn.buf[..], b"GET / HTTP/1.1");
    }

    #[test]
    fn recv_result_is_clamped_to_the_sqe_length() {
        let mut conn = new_conn();
        let (ptr, len) = conn.prepare_recv(16);
        kernel_recv(ptr, len, &vec![b'a'; len as usize]);
        // A bogus `res` larger than the SQE allowed must not extend `len`
        // into bytes the kernel never wrote.
        assert_eq!(conn.commit_recv(len as usize + 4096), Ok(len as usize));
        assert_eq!(conn.buf.len(), len as usize);
        assert!(conn.buf.len() <= conn.buf.capacity());
    }

    #[test]
    fn recv_survives_consuming_parsed_bytes() {
        // Parsing advances the front of `buf`; the RECV target (the end of
        // the initialised bytes) does not move.
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"abcdef");
        let (ptr, len) = conn.prepare_recv(16);
        conn.buf.advance(3);
        let n = kernel_recv(ptr, len, b"gh");
        assert_eq!(conn.commit_recv(n), Ok(2));
        assert_eq!(&conn.buf[..], b"defgh");
    }

    #[test]
    fn recv_detects_a_buffer_reallocated_under_it() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"abc");
        let (_ptr, _len) = conn.prepare_recv(16);
        let old = conn.buf.as_ptr();
        conn.buf.reserve(1 << 20);
        assert_ne!(conn.buf.as_ptr(), old, "test needs a reallocation");
        assert_eq!(conn.commit_recv(8), Err(RecvBufferMoved));
        assert_eq!(&conn.buf[..], b"abc", "no uninitialised bytes exposed");
    }

    #[test]
    fn recv_commit_without_prepare_is_rejected() {
        let mut conn = new_conn();
        assert_eq!(conn.commit_recv(1), Err(RecvBufferMoved));
        let (_ptr, _len) = conn.prepare_recv(16);
        conn.discard_recv();
        assert_eq!(conn.commit_recv(1), Err(RecvBufferMoved));
        assert!(conn.buf.is_empty());
    }

    #[test]
    fn write_region_covers_only_unwritten_bytes() {
        let mut conn = new_conn();
        assert!(conn.prepare_write().is_none());
        conn.set_serialized_response(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), true);
        conn.response_written = 5;
        let (ptr, len) = conn.prepare_write().expect("bytes left");
        assert_eq!(ptr, conn.response_buf[5..].as_ptr());
        assert_eq!(len as usize, conn.response_buf.len() - 5);
    }

    #[test]
    fn mark_closed_keeps_buffers_for_in_flight_sqes() {
        let mut conn = new_conn();
        conn.buf.extend_from_slice(b"partial");
        conn.response_buf = b"HTTP/1.1".to_vec();
        conn.recv_pending = true;
        conn.mark_closed();
        assert!(matches!(conn.state, ConnState::Closed));
        assert!(conn.has_io_in_flight());
        assert_eq!(&conn.buf[..], b"partial");
        assert_eq!(conn.response_buf, b"HTTP/1.1");
    }

    // --- EOF ---

    #[test]
    fn eof_closes() {
        let mut conn = new_conn();
        let result = conn.on_recv(0, 1024);
        assert!(matches!(result, ProcessResult::Close));
    }
}
