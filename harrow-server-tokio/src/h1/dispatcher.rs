use std::sync::Arc;
use std::time::Instant;

use bytes::Buf;
use tokio::io::AsyncRead;

use harrow_codec_h1::CONTINUE_100;
use harrow_core::dispatch::{self, SharedState};
use harrow_io::BufPool;
use harrow_server::h1::{
    EarlyResponseMode, ErrorResponse, RequestBodyDecision, decide_request_body_progress,
    early_response_control,
};
use harrow_server::h1_lifecycle::{Event, trace::Trace};

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
    // Lifecycle events for `h1_lifecycle::Machine`; see `trace` there for the
    // mapping notes. Recorded only with the `lifecycle-trace` feature.
    let mut trace = Trace::new();
    trace.record(Event::Accept);

    'connection: loop {
        // Waiting for the next request head. Stopping here (shutdown,
        // connection lifetime) cancels that wait: `Timeout`, then the
        // cancelled read completes (`ClosedCqe`).
        if shutdown.is_shutdown() {
            trace.record(Event::Shutdown);
            trace.record(Event::Timeout);
            trace.record(Event::ClosedCqe);
            break;
        }

        if let Some(max_life) = config.connection_timeout
            && accepted_at.elapsed() >= max_life
        {
            trace.record(Event::Timeout);
            trace.record(Event::ClosedCqe);
            break;
        }

        let parsed = match request_head::read_request_head(
            &mut read_stream,
            &mut buf,
            config,
            &mut trace,
            shutdown,
        )
        .await
        {
            request_head::RequestHeadRead::Parsed(parsed) => *parsed,
            request_head::RequestHeadRead::WriteError(error_response) => {
                write_error_response(&writer, error_response, &mut trace).await;
                break;
            }
            request_head::RequestHeadRead::Close(reason) => {
                match reason {
                    // The model treats peer EOF like any failed read.
                    request_head::CloseReason::Eof | request_head::CloseReason::IoError => {
                        trace.record(Event::IoError);
                    }
                    request_head::CloseReason::Timeout => {
                        trace.record(Event::Timeout);
                        trace.record(Event::ClosedCqe);
                    }
                    // Same as noticing shutdown at the top of the loop.
                    request_head::CloseReason::Shutdown => {
                        trace.record(Event::Shutdown);
                        trace.record(Event::Timeout);
                        trace.record(Event::ClosedCqe);
                    }
                }
                break;
            }
        };

        let keep_alive = parsed.keep_alive;
        let content_length = parsed.content_length;

        if harrow_server::h1::request_exceeds_body_limit(content_length, config.max_body_size) {
            write_error_response(&writer, ErrorResponse::PayloadTooLarge, &mut trace).await;
            break;
        }

        buf.advance(parsed.header_len);

        let is_head_request = parsed.method == http::Method::HEAD;
        let (mut request_body_state, body) = request_body::RequestBodyState::start(&parsed, config);
        if parsed.expect_continue && writer.write_raw(CONTINUE_100).await.is_err() {
            trace.record(Event::HeadersParsed { has_body: true });
            trace.record(Event::IoError);
            break 'connection;
        }
        let request = harrow_server::h1::build_request(&parsed, body)?;
        let mut response_fut = std::pin::pin!(dispatch::dispatch(Arc::clone(&shared), request));

        let mut request_body_complete = request_body_state.is_complete();
        trace.record(Event::HeadersParsed {
            has_body: !request_body_complete,
        });
        let mut connection_reusable = keep_alive && !shutdown.is_shutdown();
        let mut drain_request_body = false;
        let response = loop {
            if request_body_complete {
                let response = response_fut.await;
                trace.record(Event::DispatchDone);
                break response;
            }

            tokio::select! {
                biased;
                response = &mut response_fut => {
                    // The handler answered before the request body ended.
                    trace.record(Event::EarlyResponse);
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
                        RequestBodyDecision::Continue => trace.record(Event::BodyNeedMore),
                        RequestBodyDecision::BodyComplete { keep_alive, drain_request_body: should_drain } => {
                            trace.record(Event::BodyDone);
                            request_body_complete = true;
                            connection_reusable = keep_alive;
                            drain_request_body |= should_drain;
                        }
                        RequestBodyDecision::WriteError(error_response) => {
                            write_error_response(&writer, error_response, &mut trace).await;
                            break 'connection;
                        }
                        RequestBodyDecision::CloseConnection => {
                            trace.record(Event::IoError);
                            break 'connection;
                        }
                    }
                }
            }
        };

        // Shutdown may have started while the handler ran: tell the client
        // this connection closes instead of advertising keep-alive.
        if shutdown.is_shutdown() {
            connection_reusable = false;
        }
        if writer
            .write_response(response, connection_reusable, is_head_request)
            .await
            .is_err()
        {
            trace.record(Event::IoError);
            break;
        }
        trace.record(Event::WriteDone {
            keep_alive: connection_reusable,
            buffered_next_request: !buf.is_empty(),
        });

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

/// Write an error response and close: the model's `ProtocolError` /
/// `BodyLimitExceeded`, then `WriteDone` (or `IoError` if the write fails).
async fn write_error_response(
    writer: &response::WriteRunner,
    error: ErrorResponse,
    trace: &mut Trace,
) {
    trace.record(match error {
        ErrorResponse::PayloadTooLarge => Event::BodyLimitExceeded,
        _ => Event::ProtocolError,
    });
    match writer.write_error(error.status_u16(), error.body()).await {
        Ok(()) => trace.record(Event::WriteDone {
            keep_alive: false,
            buffered_next_request: false,
        }),
        Err(_) => trace.record(Event::IoError),
    }
}
