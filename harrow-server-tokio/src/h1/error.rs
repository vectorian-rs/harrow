use std::borrow::Cow;

#[cfg(test)]
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub(crate) fn error_bytes(status: u16, body: &str) -> Cow<'static, [u8]> {
    match (status, body) {
        (400, "bad request") => Cow::Borrowed(&b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad request"[..]),
        (400, "request headers too large") => Cow::Borrowed(&b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 25\r\nconnection: close\r\n\r\nrequest headers too large"[..]),
        (408, "request timeout") => Cow::Borrowed(&b"HTTP/1.1 408 Request Timeout\r\ncontent-type: text/plain\r\ncontent-length: 15\r\nconnection: close\r\n\r\nrequest timeout"[..]),
        (413, "payload too large") => Cow::Borrowed(&b"HTTP/1.1 413 Payload Too Large\r\ncontent-type: text/plain\r\ncontent-length: 17\r\nconnection: close\r\n\r\npayload too large"[..]),
        _ => Cow::Owned(
            format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}",
                reason = http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Error"),
                len = body.len(),
            )
            .into_bytes(),
        ),
    }
}

#[cfg(test)]
pub(crate) async fn write_error<S>(stream: &mut S, status: u16, body: &str) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let resp = error_bytes(status, body);
    stream.write_all(resp.as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn render_error(status: u16, body: &str) -> String {
        let (mut client, mut server) = tokio::io::duplex(256);
        write_error(&mut server, status, body).await.unwrap();
        drop(server);

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn write_error_static_templates_match_body_length() {
        for (status, body) in [
            (400, "bad request"),
            (400, "request headers too large"),
            (408, "request timeout"),
            (413, "payload too large"),
        ] {
            let response = render_error(status, body).await;
            let (head, resp_body) = response.split_once("\r\n\r\n").unwrap();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(content_length, resp_body.len(), "{status} {body}");
        }
    }
}
