use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use harrow_server::h1::{
    ResponseBodyMode, finish_fixed_response_body, prepare_response, record_fixed_response_bytes,
};

pub(crate) async fn write_response<S>(
    stream: &mut S,
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
    stream.write_all(&prepared.head).await?;

    match prepared.plan.mode {
        ResponseBodyMode::None => Ok(()),
        ResponseBodyMode::Fixed => {
            write_body_direct(stream, &mut body, prepared.expected_len).await
        }
        ResponseBodyMode::Chunked => write_body_chunked(stream, &mut body).await,
    }
}

async fn write_body_direct<S>(
    stream: &mut S,
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
            stream.write_all(&data).await?;
        }
    }

    finish_fixed_response_body(written, expected_len).map_err(std::io::Error::other)
}

async fn write_body_chunked<S>(
    stream: &mut S,
    body: &mut harrow_core::response::ResponseBody,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut chunk_buf = Vec::with_capacity(128);

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            chunk_buf.clear();
            harrow_codec_h1::encode_chunk_into(&data, &mut chunk_buf);
            stream.write_all(&chunk_buf).await?;
        }
    }

    stream.write_all(harrow_codec_h1::CHUNK_TERMINATOR).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_length_response_shorter_than_declared_errors() {
        let response = harrow_core::response::Response::text("hello")
            .header("content-length", "10")
            .into_inner();
        let (mut stream, _peer) = tokio::io::duplex(1024);

        let err = write_response(&mut stream, response, false, false)
            .await
            .expect_err("fixed-length mismatch should error");

        assert!(err.to_string().contains("shorter than declared"));
    }
}
