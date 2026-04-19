use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use harrow_core::dispatch::SharedState;

use crate::ServerConfig;
use crate::h1::dispatcher;

pub(crate) async fn handle_tcp_connection(
    stream: TcpStream,
    shared: Arc<SharedState>,
    config: &ServerConfig,
    shutdown: harrow_server::ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    let (read_stream, write_stream): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    dispatcher::handle_connection_with_shutdown(
        read_stream,
        write_stream,
        shared,
        config,
        &shutdown,
    )
    .await
}

#[doc(hidden)]
pub async fn handle_connection<S>(
    stream: S,
    shared: Arc<SharedState>,
    config: &ServerConfig,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    let shutdown = harrow_server::ShutdownSignal::new();
    let (read_stream, write_stream) = tokio::io::split(stream);
    dispatcher::handle_connection_with_shutdown(
        read_stream,
        write_stream,
        shared,
        config,
        &shutdown,
    )
    .await
}
