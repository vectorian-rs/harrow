use bytes::Bytes;
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
        let (mut parts, body) = response.into_parts();

        if !keep_alive {
            parts
                .headers
                .insert(http::header::CONNECTION, "close".parse().unwrap());
        }

        let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);
        let body_permitted =
            harrow_server::h1::response_body_permitted(is_head_request, parts.status);

        let head = codec::write_response_head(
            parts.status,
            &parts.headers,
            body_permitted && !has_content_length,
        );
        let (result, _) = self.stream.write_all(head).await;
        result?;

        if !body_permitted {
            return Ok(());
        }

        if has_content_length {
            self.write_body_direct(body).await?;
        } else {
            self.write_body_chunked(body).await?;
        }

        Ok(())
    }

    async fn write_body_direct(
        &mut self,
        mut body: harrow_core::response::ResponseBody,
    ) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| -> Box<dyn std::error::Error> { e })?;
            if let Ok(data) = frame.into_data() {
                self.write_data_frame(data).await?;
            }
        }
        Ok(())
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
