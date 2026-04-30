use std::net::SocketAddr;

use harrow::{App, Response, serve};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = App::new().get("/", |_req| async { Response::text("hello from hyper") });

    let addr: SocketAddr = "127.0.0.1:8080".parse()?;
    serve(app, addr).await
}
