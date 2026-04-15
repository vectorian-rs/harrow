use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use harrow_codec_h1::write_response_head;

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
    let has_content_length = parts.headers.contains_key(http::header::CONTENT_LENGTH);
    let body_permitted = harrow_server::h1::response_body_permitted(is_head_request, parts.status);

    if !keep_alive && !parts.headers.contains_key(http::header::CONNECTION) {
        parts
            .headers
            .insert(http::header::CONNECTION, "close".parse().unwrap());
    }

    let chunked = body_permitted && !has_content_length;
    let head = write_response_head(parts.status, &parts.headers, chunked);
    stream.write_all(&head).await?;

    if !body_permitted {
        return Ok(());
    }

    if has_content_length {
        write_body_direct(stream, &mut body).await
    } else {
        write_body_chunked(stream, &mut body).await
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
