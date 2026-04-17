use bytes::{BufMut, Bytes, BytesMut};
use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use harrow_io::BufPool;
use harrow_server::h1::{
    ResponseBodyMode, finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
};

use crate::h1::error;

const MAX_BUFFERED_WRITE_SIZE: usize = 16 * 1024;
const MAX_CHUNK_HEADER_LEN: usize = 2 * std::mem::size_of::<usize>() + 2;

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
    let mut write_buf = BufPool::acquire_write();
    let result = async {
        while let Some(command) = receiver.recv().await {
            let stop = match command {
                WriteCommand::Raw { bytes, completion } => {
                    let result = write_raw_to_stream(&mut stream, &mut write_buf, &bytes).await;
                    let stop = result.is_err();
                    let _ = completion.send(result);
                    stop
                }
                WriteCommand::Error {
                    status,
                    body,
                    completion,
                } => {
                    let result =
                        write_error_to_stream(&mut stream, &mut write_buf, status, body).await;
                    let stop = result.is_err();
                    let _ = completion.send(result);
                    stop
                }
                WriteCommand::Response {
                    response,
                    keep_alive,
                    is_head_request,
                    completion,
                } => {
                    let result = write_response_to_stream(
                        &mut stream,
                        &mut write_buf,
                        response,
                        keep_alive,
                        is_head_request,
                    )
                    .await;
                    let stop = result.is_err();
                    let _ = completion.send(result);
                    stop
                }
                WriteCommand::Shutdown { completion } => {
                    let result = stream.shutdown().await;
                    let _ = completion.send(result);
                    true
                }
            };

            if stop {
                break;
            }
        }

        Ok(())
    }
    .await;

    BufPool::release_write(write_buf);
    result
}

fn join_error(err: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(format!("write runner join error: {err}"))
}

async fn write_raw_to_stream<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    bytes: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_buf.clear();
    buffer_data(stream, write_buf, bytes).await?;
    flush_buffer(stream, write_buf).await
}

async fn write_error_to_stream<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    status: u16,
    body: &'static str,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = error::error_bytes(status, body);
    write_raw_to_stream(stream, write_buf, bytes.as_ref()).await
}

async fn write_response_to_stream<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    response: http::Response<harrow_core::response::ResponseBody>,
    keep_alive: bool,
    is_head_request: bool,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let prepared =
        prepare_response(response, keep_alive, is_head_request).map_err(std::io::Error::other)?;
    let mut body = prepared.body;
    write_buf.clear();
    harrow_codec_h1::write_response_head_into_bytes_mut(
        prepared.status,
        &prepared.headers,
        prepared.plan.is_chunked(),
        write_buf,
    );

    match prepared.plan.mode {
        ResponseBodyMode::None => flush_buffer(stream, write_buf).await,
        ResponseBodyMode::Fixed => {
            write_body_direct(stream, write_buf, &mut body, prepared.expected_len).await
        }
        ResponseBodyMode::Chunked => write_body_chunked(stream, write_buf, &mut body).await,
    }
}

async fn write_body_direct<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    body: &mut harrow_core::response::ResponseBody,
    expected_len: usize,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut written = 0usize;

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            record_fixed_response_bytes(&mut written, &data, expected_len)
                .map_err(std::io::Error::other)?;
            buffer_bytes(stream, write_buf, &data).await?;
        }
    }

    finish_fixed_response_body(written, expected_len).map_err(std::io::Error::other)?;
    flush_buffer(stream, write_buf).await
}

async fn write_body_chunked<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    body: &mut harrow_core::response::ResponseBody,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            buffer_chunk(stream, write_buf, &data).await?;
            flush_buffer(stream, write_buf).await?;
        }
    }

    buffer_data(stream, write_buf, harrow_codec_h1::CHUNK_TERMINATOR).await?;
    flush_buffer(stream, write_buf).await
}

async fn flush_buffer<S>(stream: &mut S, write_buf: &mut BytesMut) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    if write_buf.is_empty() {
        return Ok(());
    }

    stream.write_all(write_buf.as_ref()).await?;
    write_buf.clear();
    Ok(())
}

async fn buffer_data<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    data: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }

    if data.len() > MAX_BUFFERED_WRITE_SIZE {
        flush_buffer(stream, write_buf).await?;
        stream.write_all(data).await?;
        return Ok(());
    }

    if write_buf.len() + data.len() > MAX_BUFFERED_WRITE_SIZE {
        flush_buffer(stream, write_buf).await?;
    }

    write_buf.extend_from_slice(data);
    Ok(())
}

async fn buffer_bytes<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    data: &Bytes,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    buffer_data(stream, write_buf, data.as_ref()).await
}

async fn buffer_chunk<S>(
    stream: &mut S,
    write_buf: &mut BytesMut,
    data: &Bytes,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let encoded_len = encoded_chunk_len(data.len());
    if encoded_len > MAX_BUFFERED_WRITE_SIZE {
        flush_buffer(stream, write_buf).await?;
        write_chunk_direct(stream, data).await?;
        return Ok(());
    }

    if write_buf.len() + encoded_len > MAX_BUFFERED_WRITE_SIZE {
        flush_buffer(stream, write_buf).await?;
    }

    append_chunk(write_buf, data);
    Ok(())
}

async fn write_chunk_direct<S>(stream: &mut S, data: &Bytes) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut hex = [0u8; MAX_CHUNK_HEADER_LEN];
    let header = encode_chunk_header(data.len(), &mut hex);
    stream.write_all(header).await?;
    stream.write_all(data.as_ref()).await?;
    stream.write_all(b"\r\n").await
}

fn append_chunk(write_buf: &mut BytesMut, data: &Bytes) {
    let mut hex = [0u8; MAX_CHUNK_HEADER_LEN];
    let header = encode_chunk_header(data.len(), &mut hex);
    write_buf.reserve(header.len() + data.len() + 2);
    write_buf.extend_from_slice(header);
    write_buf.extend_from_slice(data.as_ref());
    write_buf.put_slice(b"\r\n");
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
    use std::pin::Pin;
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

    #[tokio::test]
    async fn fixed_length_response_shorter_than_declared_errors() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "10")
            .into_inner();
        let mut stream = CountingWriter::default();
        let mut write_buf = BytesMut::with_capacity(1024);

        let err = write_response_to_stream(&mut stream, &mut write_buf, response, false, false)
            .await
            .expect_err("fixed-length mismatch should error");

        assert!(err.to_string().contains("shorter than declared"));
    }

    #[tokio::test]
    async fn small_fixed_response_head_and_body_are_coalesced() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "5")
            .into_inner();
        let mut stream = CountingWriter::default();
        let mut write_buf = BytesMut::with_capacity(1024);

        write_response_to_stream(&mut stream, &mut write_buf, response, false, false)
            .await
            .expect("response write should succeed");

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
        let mut write_buf = BytesMut::with_capacity(1024);

        write_response_to_stream(&mut stream, &mut write_buf, response, false, false)
            .await
            .expect("response write should succeed");

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
}
