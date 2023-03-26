use std::time::Duration;

use axum::routing::get;
use axum::Server;
use engineio_server::{
    errors::Error,
    layer::{EngineIoConfig, EngineIoHandler, EngineIoLayer},
    socket::Socket,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(Clone)]
struct MyHandler;

#[engineio_server::async_trait]
impl EngineIoHandler for MyHandler {
    fn on_connect(&self, socket: &Socket<Self>) {
        println!("socket connect {}", socket.sid);
    }
    fn on_disconnect(&self, socket: &Socket<Self>) {
        println!("socket disconnect {}", socket.sid);
    }

    async fn on_message(&self, msg: String, socket: &Socket<Self>) -> Result<(), Error> {
        println!("Ping pong message {:?}", msg);
        socket.emit(msg).await
    }

    async fn on_binary(&self, data: Vec<u8>, socket: &Socket<Self>) -> Result<(), Error> {
        println!("Ping pong binary message {:?}", data);
        socket.emit_binary(data).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_line_number(true)
        .with_max_level(Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = EngineIoConfig::builder()
        .ping_interval(Duration::from_millis(3000))
        .ping_timeout(Duration::from_millis(2000))
        .max_payload(1e6 as u64)
        .build();
    info!("Starting server");
    let app = axum::Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .layer(EngineIoLayer::from_config(MyHandler, config));

    Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
