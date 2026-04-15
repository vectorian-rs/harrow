use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;

use harrow_core::dispatch::dispatch;
use harrow_core::request::Body;

use crate::buffer::DEFAULT_BUFFER_SIZE;
use crate::codec;
use crate::h1::dispatcher::H1Connection;
use crate::protocol::ProtocolError;

impl H1Connection {
    /// Read the request body based on Content-Length or chunked encoding.
    pub(crate) async fn read_body(
        &mut self,
        content_length: Option<u64>,
        chunked: bool,
        max_body: usize,
    ) -> Result<Bytes, ProtocolError> {
        if chunked {
            return self.read_chunked_body(max_body).await;
        }

        let length = match content_length {
            Some(0) | None => return Ok(Bytes::new()),
            Some(len) => len as usize,
        };

        while self.buf.len() < length {
            let needed = length - self.buf.len();
            let n = self
                .read_more(
                    needed.min(DEFAULT_BUFFER_SIZE),
                    self.effective_read_timeout(self.config.body_read_timeout)?,
                )
                .await?;
            if n == 0 {
                return Err(ProtocolError::Parse(
                    "unexpected eof during body read".into(),
                ));
            }
        }

        Ok(self.buf.split_to(length).freeze())
    }

    /// Read a chunked transfer-encoded body.
    async fn read_chunked_body(&mut self, max_body: usize) -> Result<Bytes, ProtocolError> {
        loop {
            match codec::decode_chunked_with_limit(&self.buf, (max_body > 0).then_some(max_body)) {
                Ok(Some((body, consumed))) => {
                    let _ = self.buf.split_to(consumed);
                    return Ok(body);
                }
                Ok(None) => {
                    let n = self
                        .read_more(
                            DEFAULT_BUFFER_SIZE,
                            self.effective_read_timeout(self.config.body_read_timeout)?,
                        )
                        .await?;
                    if n == 0 {
                        return Err(ProtocolError::Parse(
                            "unexpected eof during chunked body read".into(),
                        ));
                    }
                }
                Err(codec::CodecError::BodyTooLarge) => return Err(ProtocolError::BodyTooLarge),
                Err(codec::CodecError::Incomplete) => continue,
                Err(codec::CodecError::Invalid(msg)) => return Err(ProtocolError::Parse(msg)),
            }
        }
    }

    /// Build request and dispatch through Harrow.
    pub(crate) async fn dispatch_request(
        &self,
        parsed: &codec::ParsedRequest,
        body_bytes: Bytes,
    ) -> http::Response<harrow_core::response::ResponseBody> {
        let mut builder = http::Request::builder()
            .method(&parsed.method)
            .uri(&parsed.uri)
            .version(parsed.version);

        for (name, value) in parsed.headers.iter() {
            builder = builder.header(name, value);
        }

        let body: Body = {
            use http_body_util::Full;
            Full::new(body_bytes)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { match e {} })
                .boxed()
        };

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

        dispatch(Arc::clone(&self.config.shared), req).await
    }
}
