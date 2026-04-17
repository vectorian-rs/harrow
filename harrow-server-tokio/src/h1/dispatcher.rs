use std::sync::Arc;
use std::time::Instant;

use bytes::Buf;
use tokio::io::AsyncRead;

use harrow_codec_h1::CONTINUE_100;
use harrow_core::dispatch::{self, SharedState};
use harrow_io::BufPool;
use harrow_server::h1::{
    EarlyResponseMode, RequestBodyDecision, decide_request_body_progress, early_response_control,
};

use crate::ServerConfig;
use crate::h1::{request_body, request_head, response};

pub(crate) async fn handle_connection_with_shutdown<S>(
    mut read_stream: S,
    write_stream: impl tokio::io::AsyncWrite + Unpin + 'static,
    shared: Arc<SharedState>,
    config: &ServerConfig,
    shutdown: &harrow_server::ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncRead + Unpin,
{
    let accepted_at = Instant::now();
    let mut buf = BufPool::acquire_read();
    let writer = response::WriteRunner::spawn(write_stream);

    'connection: loop {
        if shutdown.is_shutdown() {
            break;
        }

        if let Some(max_life) = config.connection_timeout
            && accepted_at.elapsed() >= max_life
        {
            break;
        }

        let parsed = match request_head::read_request_head(&mut read_stream, &mut buf, config).await
        {
            request_head::RequestHeadRead::Parsed(parsed) => *parsed,
            request_head::RequestHeadRead::WriteError(error_response) => {
                let _ = writer
                    .write_error(error_response.status_u16(), error_response.body())
                    .await;
                break;
            }
            request_head::RequestHeadRead::Close => break,
        };

        let keep_alive = parsed.keep_alive;
        let content_length = parsed.content_length;

        if harrow_server::h1::request_exceeds_body_limit(content_length, config.max_body_size) {
            let error_response = harrow_server::h1::ErrorResponse::PayloadTooLarge;
            let _ = writer
                .write_error(error_response.status_u16(), error_response.body())
                .await;
            break;
        }

        buf.advance(parsed.header_len);

        let is_head_request = parsed.method == http::Method::HEAD;
        let (mut request_body_state, body) = request_body::RequestBodyState::start(&parsed, config);
        if parsed.expect_continue && writer.write_raw(CONTINUE_100).await.is_err() {
            break 'connection;
        }
        let request = harrow_server::h1::build_request(&parsed, body)?;
        let mut response_fut = std::pin::pin!(dispatch::dispatch(Arc::clone(&shared), request));

        let mut request_body_complete = request_body_state.is_complete();
        let mut connection_reusable = keep_alive && !shutdown.is_shutdown();
        let mut drain_request_body = false;
        let response = loop {
            if request_body_complete {
                break response_fut.await;
            }

            tokio::select! {
                biased;
                response = &mut response_fut => {
                    let control = early_response_control(EarlyResponseMode::DrainRequestBody);
                    connection_reusable = control.keep_alive;
                    request_body_state.detach_receiver();
                    drain_request_body = control.drain_request_body;
                    break response;
                }
                pump = request_body_state.pump_once(&mut read_stream, &mut buf) => {
                    match decide_request_body_progress(
                        pump,
                        connection_reusable,
                        EarlyResponseMode::DrainRequestBody,
                    ) {
                        RequestBodyDecision::Continue => {}
                        RequestBodyDecision::BodyComplete { keep_alive, drain_request_body: should_drain } => {
                            request_body_complete = true;
                            connection_reusable = keep_alive;
                            drain_request_body |= should_drain;
                        }
                        RequestBodyDecision::WriteError(error_response) => {
                            let _ = writer
                                .write_error(error_response.status_u16(), error_response.body())
                                .await;
                            break 'connection;
                        }
                        RequestBodyDecision::CloseConnection => {
                            break 'connection;
                        }
                    }
                }
            }
        };

        if writer
            .write_response(response, connection_reusable, is_head_request)
            .await
            .is_err()
        {
            break;
        }

        if drain_request_body {
            if writer.shutdown().await.is_err() {
                break;
            }
            request_body_state
                .drain_to_eof(&mut read_stream, &mut buf)
                .await;
        }

        if !connection_reusable {
            break;
        }
    }

    BufPool::release_read(buf);
    let _ = writer.finish().await;
    Ok(())
}
