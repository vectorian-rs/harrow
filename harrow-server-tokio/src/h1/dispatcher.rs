use std::sync::Arc;
use std::time::Instant;

use bytes::Buf;
use tokio::io::AsyncWriteExt;

use harrow_core::dispatch::{self, SharedState};
use harrow_io::BufPool;

use crate::ServerConfig;
use crate::h1::{error, request_body, request_head, response};

pub(crate) async fn handle_connection_with_shutdown<S>(
    mut stream: S,
    shared: Arc<SharedState>,
    config: &ServerConfig,
    shutdown: &harrow_server::ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let accepted_at = Instant::now();
    let mut buf = BufPool::acquire_read();

    'connection: loop {
        if shutdown.is_shutdown() {
            break;
        }

        if let Some(max_life) = config.connection_timeout
            && accepted_at.elapsed() >= max_life
        {
            break;
        }

        let parsed = match request_head::read_request_head(&mut stream, &mut buf, config).await {
            Some(parsed) => parsed,
            None => break,
        };

        let keep_alive = parsed.keep_alive;
        let content_length = parsed.content_length;

        if harrow_server::h1::request_exceeds_body_limit(content_length, config.max_body_size) {
            let error_response = harrow_server::h1::ErrorResponse::PayloadTooLarge;
            error::write_error(
                &mut stream,
                error_response.status_u16(),
                error_response.body(),
            )
            .await;
            break;
        }

        buf.advance(parsed.header_len);

        let is_head_request = parsed.method == http::Method::HEAD;
        let (mut request_body_state, body) =
            match request_body::RequestBodyState::start(&mut stream, &parsed, config).await {
                Ok(state) => state,
                Err(_) => break 'connection,
            };
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
                    connection_reusable = false;
                    request_body_state.detach_receiver();
                    drain_request_body = true;
                    break response;
                }
                pump = request_body_state.pump_once(&mut stream, &mut buf) => {
                    match pump {
                        request_body::PumpStatus::Progress => {}
                        request_body::PumpStatus::Eof => {
                            request_body_complete = true;
                        }
                        request_body::PumpStatus::ResponseError { error: error_response } => {
                            error::write_error(
                                &mut stream,
                                error_response.status_u16(),
                                error_response.body(),
                            )
                            .await;
                            break 'connection;
                        }
                        request_body::PumpStatus::ConnectionClosed => {
                            break 'connection;
                        }
                        request_body::PumpStatus::ReceiverClosed => {
                            request_body_complete = true;
                            connection_reusable = false;
                            drain_request_body = true;
                        }
                    }
                }
            }
        };

        if response::write_response(&mut stream, response, connection_reusable, is_head_request)
            .await
            .is_err()
        {
            break;
        }

        if drain_request_body {
            if stream.shutdown().await.is_err() {
                break;
            }
            request_body_state.drain_to_eof(&mut stream, &mut buf).await;
        }

        if !connection_reusable {
            break;
        }
    }

    BufPool::release_read(buf);
    Ok(())
}
