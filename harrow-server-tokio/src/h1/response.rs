use std::collections::VecDeque;
use std::future::poll_fn;
use std::io::IoSlice;

use bytes::{Buf, Bytes, BytesMut};
use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use harrow_io::BufPool;
use harrow_server::h1::{
    ResponseBodyMode, finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
};

use crate::h1::error;

const MAX_BUFFERED_WRITE_SIZE: usize = 16 * 1024;
const MAX_INLINE_WRITE_SIZE: usize = 1024;
const MAX_CHUNK_HEADER_LEN: usize = 2 * std::mem::size_of::<usize>() + 2;
const MAX_WRITE_IO_SLICES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlushMode {
    Buffered,
    Force,
    Streaming,
}

enum WriteCommand {
    Raw {
        bytes: Bytes,
        completion: oneshot::Sender<std::io::Result<()>>,
    },
    Error {
        status: u16,
        body: &'static str,
        completion: oneshot::Sender<std::io::Result<()>>,
    },
    Response {
        response: http::Response<harrow_core::response::ResponseBody>,
        keep_alive: bool,
        is_head_request: bool,
        completion: oneshot::Sender<std::io::Result<()>>,
    },
    Shutdown {
        completion: oneshot::Sender<std::io::Result<()>>,
    },
}

struct WriteState {
    staging: BytesMut,
    pending: VecDeque<Bytes>,
    pending_bytes: usize,
}

#[derive(Clone, Copy)]
struct QueueMark {
    pending_len: usize,
    pending_bytes: usize,
    staging_len: usize,
}

impl WriteState {
    fn new() -> Self {
        Self {
            staging: BufPool::acquire_write(),
            pending: VecDeque::new(),
            pending_bytes: 0,
        }
    }

    fn release(self) {
        BufPool::release_write(self.staging);
    }

    fn mark(&self) -> QueueMark {
        QueueMark {
            pending_len: self.pending.len(),
            pending_bytes: self.pending_bytes,
            staging_len: self.staging.len(),
        }
    }

    fn rollback(&mut self, mark: QueueMark) {
        while self.pending.len() > mark.pending_len {
            self.pending.pop_back();
        }
        self.staging.truncate(mark.staging_len);
        self.pending_bytes = mark.pending_bytes;
    }

    fn has_pending(&self) -> bool {
        self.pending_bytes != 0
    }

    fn queue_slice(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        if !self.pending.is_empty() && self.staging.len() + data.len() > MAX_BUFFERED_WRITE_SIZE {
            self.flush_staging_to_pending();
        }

        self.staging.extend_from_slice(data);
        self.pending_bytes += data.len();
    }

    fn queue_bytes(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }

        if data.len() <= MAX_INLINE_WRITE_SIZE {
            self.queue_slice(data.as_ref());
            return;
        }

        self.flush_staging_to_pending();
        self.pending_bytes += data.len();
        self.pending.push_back(data);
    }

    fn queue_error_bytes(&mut self, bytes: std::borrow::Cow<'static, [u8]>) {
        match bytes {
            std::borrow::Cow::Borrowed(bytes) => self.queue_bytes(Bytes::from_static(bytes)),
            std::borrow::Cow::Owned(bytes) => self.queue_bytes(Bytes::from(bytes)),
        }
    }

    fn encode_response_head(
        &mut self,
        status: http::StatusCode,
        headers: &http::HeaderMap,
        chunked: bool,
    ) {
        let before = self.staging.len();
        harrow_codec_h1::write_response_head_into_bytes_mut(
            status,
            headers,
            chunked,
            &mut self.staging,
        );
        self.pending_bytes += self.staging.len() - before;
    }

    fn flush_staging_to_pending(&mut self) {
        if self.staging.is_empty() {
            return;
        }

        self.pending.push_back(self.staging.split().freeze());
    }

    fn build_io_slices<'a>(&'a self, slices: &mut [IoSlice<'a>]) -> usize {
        let mut count = 0;
        for bytes in &self.pending {
            if count == slices.len() {
                break;
            }
            slices[count] = IoSlice::new(bytes.as_ref());
            count += 1;
        }

        if count < slices.len() && !self.staging.is_empty() {
            slices[count] = IoSlice::new(self.staging.as_ref());
            count += 1;
        }

        count
    }

    fn first_slice(&self) -> &[u8] {
        if let Some(front) = self.pending.front() {
            front.as_ref()
        } else {
            self.staging.as_ref()
        }
    }

    fn consume(&mut self, written: usize) {
        self.pending_bytes = self.pending_bytes.saturating_sub(written);
        let mut remaining = written;

        while remaining > 0 {
            if let Some(front) = self.pending.front_mut() {
                if remaining >= front.len() {
                    remaining -= front.len();
                    self.pending.pop_front();
                } else {
                    front.advance(remaining);
                    break;
                }
            } else {
                self.staging.advance(remaining);
                break;
            }
        }
    }
}

pub(crate) struct WriteRunner {
    sender: mpsc::UnboundedSender<WriteCommand>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl WriteRunner {
    pub(crate) fn spawn<S>(stream: S) -> Self
    where
        S: AsyncWrite + Unpin + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel();
        let task = tokio::task::spawn_local(run_write_loop(stream, receiver));
        Self { sender, task }
    }

    pub(crate) async fn write_raw(&self, bytes: &'static [u8]) -> std::io::Result<()> {
        self.send(WriteCommand::Raw {
            bytes: Bytes::from_static(bytes),
            completion: oneshot::channel().0,
        })
        .await
    }

    pub(crate) async fn write_error(&self, status: u16, body: &'static str) -> std::io::Result<()> {
        self.send(WriteCommand::Error {
            status,
            body,
            completion: oneshot::channel().0,
        })
        .await
    }

    pub(crate) async fn write_response(
        &self,
        response: http::Response<harrow_core::response::ResponseBody>,
        keep_alive: bool,
        is_head_request: bool,
    ) -> std::io::Result<()> {
        self.send(WriteCommand::Response {
            response,
            keep_alive,
            is_head_request,
            completion: oneshot::channel().0,
        })
        .await
    }

    pub(crate) async fn shutdown(&self) -> std::io::Result<()> {
        self.send(WriteCommand::Shutdown {
            completion: oneshot::channel().0,
        })
        .await
    }

    pub(crate) async fn finish(self) -> std::io::Result<()> {
        let Self { sender, task } = self;
        drop(sender);
        task.await.map_err(join_error)?
    }

    async fn send(&self, mut command: WriteCommand) -> std::io::Result<()> {
        let (completion, receiver) = oneshot::channel();
        match &mut command {
            WriteCommand::Raw {
                completion: slot, ..
            }
            | WriteCommand::Error {
                completion: slot, ..
            }
            | WriteCommand::Response {
                completion: slot, ..
            }
            | WriteCommand::Shutdown { completion: slot } => *slot = completion,
        }

        self.sender.send(command).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "write runner closed")
        })?;

        receiver.await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "write runner dropped")
        })?
    }
}

async fn run_write_loop<S>(
    mut stream: S,
    mut receiver: mpsc::UnboundedReceiver<WriteCommand>,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut state = WriteState::new();
    let result = async {
        while let Some(command) = receiver.recv().await {
            let stop = match process_write_command(&mut stream, &mut state, command).await? {
                CommandState::Continue => false,
                CommandState::Stop => true,
                CommandState::Buffered => {
                    tokio::task::yield_now().await;

                    let mut disconnected = false;
                    let mut stop = false;
                    loop {
                        match receiver.try_recv() {
                            Ok(next) => {
                                match process_write_command(&mut stream, &mut state, next).await? {
                                    CommandState::Continue => {}
                                    CommandState::Stop => {
                                        stop = true;
                                        break;
                                    }
                                    CommandState::Buffered => {}
                                }
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }

                    flush_pending(&mut stream, &mut state).await?;
                    stop || disconnected
                }
            };

            if stop {
                break;
            }
        }

        flush_pending(&mut stream, &mut state).await?;
        Ok(())
    }
    .await;

    state.release();
    result
}

fn join_error(err: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(format!("write runner join error: {err}"))
}

fn copy_io_error(err: &std::io::Error) -> std::io::Error {
    std::io::Error::new(err.kind(), err.to_string())
}

enum CommandState {
    Continue,
    Buffered,
    Stop,
}

async fn process_write_command<S>(
    stream: &mut S,
    state: &mut WriteState,
    command: WriteCommand,
) -> std::io::Result<CommandState>
where
    S: AsyncWrite + Unpin,
{
    match command {
        WriteCommand::Raw { bytes, completion } => {
            let flush_mode = write_raw_to_state(state, &bytes);
            match maybe_flush(stream, state, flush_mode).await {
                Ok(()) => {
                    let _ = completion.send(Ok(()));
                    Ok(CommandState::Continue)
                }
                Err(err) => {
                    let _ = completion.send(Err(copy_io_error(&err)));
                    Ok(CommandState::Stop)
                }
            }
        }
        WriteCommand::Error {
            status,
            body,
            completion,
        } => {
            let flush_mode = write_error_to_state(state, status, body);
            match maybe_flush(stream, state, flush_mode).await {
                Ok(()) => {
                    let _ = completion.send(Ok(()));
                    Ok(CommandState::Continue)
                }
                Err(err) => {
                    let _ = completion.send(Err(copy_io_error(&err)));
                    Ok(CommandState::Stop)
                }
            }
        }
        WriteCommand::Response {
            response,
            keep_alive,
            is_head_request,
            completion,
        } => {
            let flush_mode =
                write_response_to_stream(stream, state, response, keep_alive, is_head_request)
                    .await;

            let flush_mode = match flush_mode {
                Ok(flush_mode) => flush_mode,
                Err(err) => {
                    let _ = completion.send(Err(copy_io_error(&err)));
                    return Ok(CommandState::Stop);
                }
            };

            match flush_mode {
                FlushMode::Buffered => {
                    let _ = completion.send(Ok(()));
                    Ok(CommandState::Buffered)
                }
                FlushMode::Force | FlushMode::Streaming => {
                    match maybe_flush(stream, state, flush_mode).await {
                        Ok(()) => {
                            let _ = completion.send(Ok(()));
                            Ok(CommandState::Continue)
                        }
                        Err(err) => {
                            let _ = completion.send(Err(copy_io_error(&err)));
                            Ok(CommandState::Stop)
                        }
                    }
                }
            }
        }
        WriteCommand::Shutdown { completion } => {
            let result = match flush_pending(stream, state).await {
                Ok(()) => stream.shutdown().await,
                Err(err) => Err(err),
            };
            let _ = completion.send(result);
            Ok(CommandState::Stop)
        }
    }
}

fn write_raw_to_state(state: &mut WriteState, bytes: &[u8]) -> FlushMode {
    state.queue_slice(bytes);
    FlushMode::Force
}

fn write_error_to_state(state: &mut WriteState, status: u16, body: &'static str) -> FlushMode {
    let bytes = error::error_bytes(status, body);
    state.queue_error_bytes(bytes);
    FlushMode::Force
}

async fn write_response_to_stream<S>(
    stream: &mut S,
    state: &mut WriteState,
    response: http::Response<harrow_core::response::ResponseBody>,
    keep_alive: bool,
    is_head_request: bool,
) -> std::io::Result<FlushMode>
where
    S: AsyncWrite + Unpin,
{
    let prepared =
        prepare_response(response, keep_alive, is_head_request).map_err(std::io::Error::other)?;
    let mut body = prepared.body;
    let mark = state.mark();
    state.encode_response_head(
        prepared.status,
        &prepared.headers,
        prepared.plan.is_chunked(),
    );

    let result = match prepared.plan.mode {
        ResponseBodyMode::None => Ok(FlushMode::Buffered),
        ResponseBodyMode::Fixed => write_body_direct(state, &mut body, prepared.expected_len).await,
        ResponseBodyMode::Chunked => write_body_chunked(stream, state, &mut body).await,
    };

    if result.is_err() {
        state.rollback(mark);
    }

    result
}

async fn write_body_direct(
    state: &mut WriteState,
    body: &mut harrow_core::response::ResponseBody,
    expected_len: usize,
) -> std::io::Result<FlushMode> {
    let mut written = 0usize;

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            record_fixed_response_bytes(&mut written, &data, expected_len)
                .map_err(std::io::Error::other)?;
            state.queue_bytes(data);
        }
    }

    finish_fixed_response_body(written, expected_len).map_err(std::io::Error::other)?;
    Ok(FlushMode::Buffered)
}

async fn write_body_chunked<S>(
    stream: &mut S,
    state: &mut WriteState,
    body: &mut harrow_core::response::ResponseBody,
) -> std::io::Result<FlushMode>
where
    S: AsyncWrite + Unpin,
{
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            buffer_chunk(state, &data);
            flush_pending(stream, state).await?;
        }
    }

    state.queue_slice(harrow_codec_h1::CHUNK_TERMINATOR);
    Ok(FlushMode::Streaming)
}

async fn flush_pending<S>(stream: &mut S, state: &mut WriteState) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    while state.has_pending() {
        let written = poll_fn(|cx| poll_write_pending(stream, state, cx)).await?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to drain response pending writes",
            ));
        }
        state.consume(written);
    }

    Ok(())
}

fn poll_write_pending<S>(
    stream: &mut S,
    state: &WriteState,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<std::io::Result<usize>>
where
    S: AsyncWrite + Unpin,
{
    let mut slices: [IoSlice<'_>; MAX_WRITE_IO_SLICES] = std::array::from_fn(|_| IoSlice::new(&[]));
    let count = state.build_io_slices(&mut slices);
    if count == 0 {
        return std::task::Poll::Ready(Ok(0));
    }

    let stream = std::pin::Pin::new(stream);
    if count > 1 && stream.as_ref().is_write_vectored() {
        stream.poll_write_vectored(cx, &slices[..count])
    } else {
        stream.poll_write(cx, state.first_slice())
    }
}

async fn maybe_flush<S>(
    stream: &mut S,
    state: &mut WriteState,
    flush_mode: FlushMode,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match flush_mode {
        FlushMode::Buffered => Ok(()),
        FlushMode::Force | FlushMode::Streaming => flush_pending(stream, state).await,
    }
}

fn buffer_chunk(state: &mut WriteState, data: &Bytes) {
    let mut hex = [0u8; MAX_CHUNK_HEADER_LEN];
    let header = encode_chunk_header(data.len(), &mut hex);
    if encoded_chunk_len(data.len()) <= MAX_BUFFERED_WRITE_SIZE
        && data.len() <= MAX_INLINE_WRITE_SIZE
    {
        state.queue_slice(header);
        state.queue_slice(data.as_ref());
        state.queue_slice(b"\r\n");
        return;
    }

    state.queue_slice(header);
    state.flush_staging_to_pending();
    state.queue_bytes(data.clone());
    state.queue_slice(b"\r\n");
}

fn encoded_chunk_len(len: usize) -> usize {
    hex_len(len) + 2 + len + 2
}

fn encode_chunk_header(len: usize, buf: &mut [u8; MAX_CHUNK_HEADER_LEN]) -> &[u8] {
    let digits = hex_len(len);
    let mut value = len;

    for idx in 0..digits {
        let digit = (value & 0x0f) as u8;
        buf[digits - idx - 1] = match digit {
            0..=9 => b'0' + digit,
            _ => b'a' + (digit - 10),
        };
        value >>= 4;
    }

    buf[digits] = b'\r';
    buf[digits + 1] = b'\n';
    &buf[..digits + 2]
}

fn hex_len(len: usize) -> usize {
    if len == 0 {
        1
    } else {
        (usize::BITS as usize - len.leading_zeros() as usize).div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use tokio::io::AsyncReadExt;

    #[derive(Default)]
    struct CountingWriter {
        writes: Vec<Vec<u8>>,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes.push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Default)]
    struct SharedWriter {
        writes: Rc<RefCell<Vec<Vec<u8>>>>,
    }

    impl AsyncWrite for SharedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes.borrow_mut().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn fixed_length_response_shorter_than_declared_errors() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "10")
            .into_inner();
        let mut stream = CountingWriter::default();
        let mut state = WriteState::new();

        let err = write_response_to_stream(&mut stream, &mut state, response, false, false)
            .await
            .expect_err("fixed-length mismatch should error");
        state.release();

        assert!(err.to_string().contains("shorter than declared"));
    }

    #[tokio::test]
    async fn small_fixed_response_head_and_body_are_coalesced() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "5")
            .into_inner();
        let mut stream = CountingWriter::default();
        let mut state = WriteState::new();

        let flush_mode = write_response_to_stream(&mut stream, &mut state, response, false, false)
            .await
            .expect("response write should succeed");
        assert_eq!(flush_mode, FlushMode::Buffered);
        maybe_flush(&mut stream, &mut state, flush_mode)
            .await
            .expect("buffered response flush should succeed");
        flush_pending(&mut stream, &mut state)
            .await
            .expect("response buffer flush should succeed");
        state.release();

        assert_eq!(stream.writes.len(), 1);
        let payload = String::from_utf8(stream.writes.pop().unwrap()).expect("writer output utf8");
        assert!(payload.contains("HTTP/1.1 200 OK\r\n"));
        assert!(payload.ends_with("\r\n\r\nhello"));
    }

    #[tokio::test]
    async fn small_chunked_response_is_buffered_without_temp_vecs() {
        let response = harrow_core::response::Response::streaming(
            http::StatusCode::OK,
            futures_util::stream::iter(vec![Ok::<
                http_body::Frame<Bytes>,
                Box<dyn std::error::Error + Send + Sync>,
            >(http_body::Frame::data(
                Bytes::from_static(b"hello"),
            ))]),
        )
        .into_inner();
        let mut stream = CountingWriter::default();
        let mut state = WriteState::new();

        let flush_mode = write_response_to_stream(&mut stream, &mut state, response, false, false)
            .await
            .expect("response write should succeed");
        assert_eq!(flush_mode, FlushMode::Streaming);
        maybe_flush(&mut stream, &mut state, flush_mode)
            .await
            .expect("streaming response flush should succeed");
        state.release();

        assert_eq!(stream.writes.len(), 2);
        let payload = String::from_utf8(stream.writes.remove(0)).expect("writer output utf8");
        assert!(payload.contains("transfer-encoding: chunked\r\n"));
        assert!(payload.ends_with("\r\n\r\n5\r\nhello\r\n"));
        assert_eq!(
            stream.writes.pop().unwrap(),
            harrow_codec_h1::CHUNK_TERMINATOR
        );
    }

    #[tokio::test]
    async fn write_runner_writes_response_via_local_task() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut server) = tokio::io::duplex(1024);
                let runner = WriteRunner::spawn(client);
                let response = harrow_core::response::Response::text("hello")
                    .header("content-length", "5")
                    .into_inner();

                runner
                    .write_response(response, false, false)
                    .await
                    .expect("write runner should deliver response");
                runner.finish().await.expect("write runner should join");

                let mut buf = Vec::new();
                server.read_to_end(&mut buf).await.unwrap();
                let rendered = String::from_utf8(buf).unwrap();
                assert!(rendered.ends_with("\r\n\r\nhello"));
            })
            .await;
    }

    #[tokio::test]
    async fn write_runner_batches_small_fixed_responses_before_flush() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let writer = SharedWriter::default();
                let writes = Rc::clone(&writer.writes);
                let runner = WriteRunner::spawn(writer);

                for body in ["hello", "world"] {
                    let content_length = body.len().to_string();
                    let response = harrow_core::response::Response::text(body)
                        .header("content-length", &content_length)
                        .into_inner();

                    runner
                        .write_response(response, true, false)
                        .await
                        .expect("write runner should queue buffered response");
                }

                runner.finish().await.expect("write runner should join");

                let writes = writes.borrow();
                assert_eq!(writes.len(), 1);
                let rendered = String::from_utf8(writes[0].clone()).expect("writer output utf8");
                assert!(rendered.contains("\r\n\r\nhelloHTTP/1.1 200 OK\r\n"));
                assert!(rendered.ends_with("\r\n\r\nworld"));
            })
            .await;
    }
}
