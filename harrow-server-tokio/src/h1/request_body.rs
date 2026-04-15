use std::time::Instant;

use bytes::{Bytes, BytesMut};
use futures_util::stream;
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use harrow_codec_h1::{CONTINUE_100, CodecError, ParsedRequest, PayloadDecoder, PayloadItem};
use harrow_core::request::Body;

use crate::ServerConfig;

const REQUEST_BODY_CHANNEL_CAPACITY: usize = 8;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BodyFrame = Result<Frame<Bytes>, BoxError>;

pub(crate) enum PumpStatus {
    Progress,
    Eof,
    ResponseError { status: u16, body: &'static str },
    ConnectionClosed,
    ReceiverClosed,
}

pub(crate) struct RequestBodyState {
    decoder: Option<PayloadDecoder>,
    sender: Option<mpsc::Sender<BodyFrame>>,
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

    pub(crate) async fn start<S>(
        stream: &mut S,
        parsed: &ParsedRequest,
        config: &ServerConfig,
    ) -> std::io::Result<(Self, Body)>
    where
        S: AsyncWrite + Unpin,
    {
        let Some(decoder) = PayloadDecoder::from_parsed(parsed) else {
            return Ok((Self::finished(config.max_body_size), empty_body()));
        };

        if parsed.expect_continue {
            stream.write_all(CONTINUE_100).await?;
        }

        let (sender, receiver) = mpsc::channel(REQUEST_BODY_CHANNEL_CAPACITY);
        let stream = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        let body = StreamBody::new(stream).boxed_unsync();

        Ok((
            Self {
                decoder: Some(decoder),
                sender: Some(sender),
                body_deadline: config.body_read_timeout.map(|t| Instant::now() + t),
                max_body_size: config.max_body_size,
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

    pub(crate) async fn pump_once<S>(&mut self, stream: &mut S, buf: &mut BytesMut) -> PumpStatus
    where
        S: AsyncRead + Unpin,
    {
        let Some(decoder) = self.decoder.as_mut() else {
            return PumpStatus::Eof;
        };

        loop {
            match decoder.decode(buf, Some(self.max_body_size)) {
                Err(CodecError::BodyTooLarge) => {
                    return self.finish_response_error(413, "payload too large");
                }
                Err(CodecError::Invalid(_)) => {
                    return self.finish_response_error(400, "bad request");
                }
                Err(CodecError::Incomplete) => {
                    return self.finish_response_error(400, "bad request");
                }
                Ok(Some(PayloadItem::Chunk(chunk))) => {
                    return self.send_chunk(chunk).await;
                }
                Ok(Some(PayloadItem::Eof)) => {
                    self.finish_eof();
                    return PumpStatus::Eof;
                }
                Ok(None) => {}
            }

            if let Some(deadline) = self.body_deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return self.finish_response_error(408, "request timeout");
                }
                match tokio::time::timeout(remaining, stream.read_buf(buf)).await {
                    Ok(Ok(0)) => {
                        return self.finish_connection_closed();
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => return self.finish_connection_closed(),
                    Err(_) => {
                        return self.finish_response_error(408, "request timeout");
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

    async fn send_chunk(&mut self, chunk: Bytes) -> PumpStatus {
        let Some(sender) = self.sender.as_mut() else {
            self.decoder = None;
            return PumpStatus::ReceiverClosed;
        };

        match sender.send(Ok(Frame::data(chunk))).await {
            Ok(()) => PumpStatus::Progress,
            Err(_) => {
                self.abort();
                PumpStatus::ReceiverClosed
            }
        }
    }

    fn finish_response_error(&mut self, status: u16, body: &'static str) -> PumpStatus {
        self.abort();
        PumpStatus::ResponseError { status, body }
    }

    fn finish_connection_closed(&mut self) -> PumpStatus {
        self.abort();
        PumpStatus::ConnectionClosed
    }

    fn finish_eof(&mut self) {
        self.decoder = None;
        self.sender = None;
    }
}

fn empty_body() -> Body {
    Full::new(Bytes::new())
        .map_err(|e| -> BoxError { match e {} })
        .boxed_unsync()
}
