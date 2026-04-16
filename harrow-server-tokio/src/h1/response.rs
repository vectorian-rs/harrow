use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use harrow_codec_h1::write_response_head;
use harrow_server::h1::{ResponseBodyMode, ResponseWritePlan};

pub(crate) async fn write_response<S>(
    stream: &mut S,
    response: http::Response<harrow_core::response::ResponseBody>,
    keep_alive: bool,
    is_head_request: bool,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let (mut parts, mut body) = response.into_parts();

    if !keep_alive && !parts.headers.contains_key(http::header::CONNECTION) {
        parts
            .headers
            .insert(http::header::CONNECTION, "close".parse().unwrap());
    }

    let plan = ResponseWritePlan::new(&parts.headers, is_head_request, parts.status);
    let head = write_response_head(parts.status, &parts.headers, plan.is_chunked());
    stream.write_all(&head).await?;

    match plan.mode {
        ResponseBodyMode::None => Ok(()),
        ResponseBodyMode::Fixed => write_body_direct(stream, &mut body).await,
        ResponseBodyMode::Chunked => write_body_chunked(stream, &mut body).await,
    }
}

async fn write_body_direct<S>(
    stream: &mut S,
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
            stream.write_all(&data).await?;
        }
    }

    Ok(())
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
