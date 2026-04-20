use bytes::Bytes;
use harrow_server::h1::{
    ResponseBodyMode, finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
};
use http_body_util::BodyExt;
use monoio::io::AsyncWriteRentExt;

use crate::codec;
use crate::h1::dispatcher::H1Connection;

const MAX_BUFFERED_WRITE_SIZE: usize = 16 * 1024;
const MAX_INLINE_WRITE_SIZE: usize = 1024;
const MAX_CHUNK_HEADER_LEN: usize = 2 * std::mem::size_of::<usize>() + 2;

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
        self.flush_write_buffer().await?;
        codec::write_response_head_into_bytes_mut(
            prepared.status,
            &prepared.headers,
            prepared.plan.is_chunked(),
            &mut self.write_buf,
        );

        match prepared.plan.mode {
            ResponseBodyMode::None => {}
            ResponseBodyMode::Fixed => {
                self.write_body_direct(prepared.body, prepared.expected_len)
                    .await?
            }
            ResponseBodyMode::Chunked => self.write_body_chunked(prepared.body).await?,
        }

        self.flush_write_buffer().await?;
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
                self.queue_fixed_data(data).await?;
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
                self.write_chunk(data).await?;
            }
        }

        self.write_buf.extend_from_slice(codec::CHUNK_TERMINATOR);
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

    async fn queue_fixed_data(&mut self, data: Bytes) -> Result<(), Box<dyn std::error::Error>> {
        if data.is_empty() {
            return Ok(());
        }

        if data.len() <= MAX_INLINE_WRITE_SIZE {
            if self.write_buf.len() + data.len() > MAX_BUFFERED_WRITE_SIZE {
                self.flush_write_buffer().await?;
            }
            self.write_buf.extend_from_slice(data.as_ref());
            return Ok(());
        }

        if !self.write_buf.is_empty() {
            self.flush_write_buffer().await?;
        }

        let (result, _) = self.stream.write_all(data).await;
        result?;
        Ok(())
    }

    async fn write_chunk(&mut self, data: Bytes) -> Result<(), Box<dyn std::error::Error>> {
        let total_len = encoded_chunk_len(data.len());

        if data.len() <= MAX_INLINE_WRITE_SIZE && total_len <= MAX_BUFFERED_WRITE_SIZE {
            if self.write_buf.len() + total_len > MAX_BUFFERED_WRITE_SIZE {
                self.flush_write_buffer().await?;
            }

            let mut header = [0u8; MAX_CHUNK_HEADER_LEN];
            let encoded_header = encode_chunk_header(data.len(), &mut header);
            self.write_buf.extend_from_slice(encoded_header);
            self.write_buf.extend_from_slice(data.as_ref());
            self.write_buf.extend_from_slice(b"\r\n");
            self.flush_write_buffer().await?;
            return Ok(());
        }

        if !self.write_buf.is_empty() {
            self.flush_write_buffer().await?;
        }

        let mut header = [0u8; MAX_CHUNK_HEADER_LEN];
        let encoded_header = encode_chunk_header(data.len(), &mut header);
        self.write_buf.extend_from_slice(encoded_header);
        self.flush_write_buffer().await?;

        let (result, _) = self.stream.write_all(data).await;
        result?;

        self.write_buf.extend_from_slice(b"\r\n");
        self.flush_write_buffer().await?;
        Ok(())
    }

    async fn flush_write_buffer(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.write_buf.is_empty() {
            return Ok(());
        }

        let (result, mut buf) = self.stream.write_all(self.write_buf.split()).await;
        result?;
        buf.clear();
        self.write_buf = buf;
        Ok(())
    }
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
