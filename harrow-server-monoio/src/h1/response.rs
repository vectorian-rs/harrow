use bytes::Bytes;
use harrow_server::h1::{
    ResponseBodyMode, finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
};
use http_body_util::BodyExt;
use monoio::io::AsyncWriteRentExt;

use crate::codec;
use crate::h1::dispatcher::H1Connection;

impl H1Connection {
    /// Write the full HTTP response (head + body) to the stream.
    pub(crate) async fn write_response(
        &mut self,
        response: http::Response<harrow_core::response::ResponseBody>,
        keep_alive: bool,
        is_head_request: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let prepared = prepare_response(response, keep_alive, is_head_request)
            .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        let head = codec::write_response_head(
            prepared.status,
            &prepared.headers,
            prepared.plan.is_chunked(),
        );
        let (result, _) = self.stream.write_all(head).await;
        result?;

        match prepared.plan.mode {
            ResponseBodyMode::None => {}
            ResponseBodyMode::Fixed => {
                self.write_body_direct(prepared.body, prepared.expected_len)
                    .await?
            }
            ResponseBodyMode::Chunked => self.write_body_chunked(prepared.body).await?,
        }

        Ok(())
    }

    async fn write_body_direct(
        &mut self,
        mut body: harrow_core::response::ResponseBody,
        expected_len: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut written = 0usize;

        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
            if let Ok(data) = frame.into_data()
                && !data.is_empty()
            {
                record_fixed_response_bytes(&mut written, &data, expected_len)
                    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
                self.write_data_frame(data).await?;
            }
        }

        finish_fixed_response_body(written, expected_len)
            .map_err(|e| -> Box<dyn std::error::Error> { e })
    }

    async fn write_body_chunked(
        &mut self,
        mut body: harrow_core::response::ResponseBody,
    ) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
            if let Ok(data) = frame.into_data()
                && !data.is_empty()
            {
                let chunk = codec::encode_chunk(&data);
                let (result, _) = self.stream.write_all(chunk).await;
                result?;
            }
        }

        let (result, _) = self
            .stream
            .write_all(codec::CHUNK_TERMINATOR.to_vec())
            .await;
        result?;
        Ok(())
    }

    pub(crate) async fn write_status(
        &mut self,
        status: http::StatusCode,
        body: &'static str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.write_response(
            harrow_core::response::Response::new(status, body).into_inner(),
            false,
            false,
        )
        .await
    }

    async fn write_data_frame(&mut self, data: Bytes) -> Result<(), Box<dyn std::error::Error>> {
        if data.is_empty() {
            return Ok(());
        }

        let (result, _) = self.stream.write_all(data.to_vec()).await;
        result?;
        Ok(())
    }
}
