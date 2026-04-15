use std::sync::Arc;

use bytes::Bytes;
use futures_channel::mpsc;
use futures_util::StreamExt;
use futures_util::stream;
use http_body::Frame;
use http_body_util::StreamBody;
use monoio::io::AsyncWriteRentExt;
use monoio::net::TcpStream;

use harrow_codec_h1::{CONTINUE_100, CodecError, ParsedRequest, PayloadDecoder, PayloadItem};
use harrow_core::dispatch::{SharedState, dispatch};
use harrow_core::request::Body;

use crate::buffer::DEFAULT_BUFFER_SIZE;
use crate::h1::dispatcher::H1Connection;
use crate::protocol::{self, ProtocolError};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BodyFrame = Result<Frame<Bytes>, BoxError>;

pub(crate) enum PumpStatus {
    Progress,
    Eof,
    ResponseError {
        status: http::StatusCode,
        body: &'static str,
    },
    ConnectionClosed,
    ReceiverClosed,
}

pub(crate) struct RequestBodyState {
    decoder: Option<PayloadDecoder>,
    sender: Option<mpsc::UnboundedSender<BodyFrame>>,
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

        let (sender, receiver) = mpsc::unbounded();
        let stream = stream::unfold(receiver, |mut receiver| async move {
            receiver.next().await.map(|item| (item, receiver))
        });
        let body = http_body_util::BodyExt::boxed(StreamBody::new(stream));

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

    pub(crate) async fn pump_once(&mut self, conn: &mut H1Connection) -> PumpStatus {
        let Some(decoder) = self.decoder.as_mut() else {
            return PumpStatus::Eof;
        };

        loop {
            match decoder.decode(&mut conn.buf, Some(self.max_body_size)) {
                Err(CodecError::BodyTooLarge) => {
                    return self.finish_response_error(
                        http::StatusCode::PAYLOAD_TOO_LARGE,
                        "payload too large",
                    );
                }
                Err(CodecError::Invalid(_)) => {
                    return self
                        .finish_response_error(http::StatusCode::BAD_REQUEST, "bad request");
                }
                Err(CodecError::Incomplete) => {
                    return self
                        .finish_response_error(http::StatusCode::BAD_REQUEST, "bad request");
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

            let timeout = match conn.effective_read_timeout(conn.config.body_read_timeout) {
                Ok(timeout) => timeout,
                Err(ProtocolError::Timeout) => {
                    return self.finish_response_error(
                        http::StatusCode::REQUEST_TIMEOUT,
                        "request timeout",
                    );
                }
                Err(_) => return self.finish_connection_closed(),
            };

            match conn.read_more(DEFAULT_BUFFER_SIZE, timeout).await {
                Ok(0) => {
                    return self
                        .finish_response_error(http::StatusCode::BAD_REQUEST, "bad request");
                }
                Ok(_) => {}
                Err(ProtocolError::Timeout) => {
                    return self.finish_response_error(
                        http::StatusCode::REQUEST_TIMEOUT,
                        "request timeout",
                    );
                }
                Err(ProtocolError::BodyTooLarge) => {
                    return self.finish_response_error(
                        http::StatusCode::PAYLOAD_TOO_LARGE,
                        "payload too large",
                    );
                }
                Err(ProtocolError::Parse(_) | ProtocolError::ProtocolViolation(_)) => {
                    return self
                        .finish_response_error(http::StatusCode::BAD_REQUEST, "bad request");
                }
                Err(ProtocolError::Io(_) | ProtocolError::StreamClosed) => {
                    return self.finish_connection_closed();
                }
            }
        }
    }

    async fn send_chunk(&mut self, chunk: Bytes) -> PumpStatus {
        let Some(sender) = self.sender.as_mut() else {
            self.abort();
            return PumpStatus::ReceiverClosed;
        };

        if sender.unbounded_send(Ok(Frame::data(chunk))).is_ok() {
            PumpStatus::Progress
        } else {
            self.abort();
            PumpStatus::ReceiverClosed
        }
    }

    fn finish_response_error(
        &mut self,
        status: http::StatusCode,
        body: &'static str,
    ) -> PumpStatus {
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

pub(crate) async fn dispatch_request(
    shared: Arc<SharedState>,
    parsed: &ParsedRequest,
    body: Body,
) -> http::Response<harrow_core::response::ResponseBody> {
    let mut builder = http::Request::builder()
        .method(&parsed.method)
        .uri(&parsed.uri)
        .version(parsed.version);

    for (name, value) in parsed.headers.iter() {
        builder = builder.header(name, value);
    }

    let req = match builder.body(body) {
        Ok(req) => req,
        Err(e) => {
            return harrow_core::response::Response::new(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("request build error: {e}"),
            )
            .into_inner();
        }
    };

    dispatch(shared, req).await
}
