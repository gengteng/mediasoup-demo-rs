mod codec;
mod participant;
mod room;
mod rooms_registry;

use crate::participant::messages::ClientMessage;
use crate::participant::ParticipantConnection;
use crate::room::RoomId;
use crate::rooms_registry::ServerState;
use axum::extract::{
    ws::{Message, WebSocket},
    Extension, Query, WebSocketUpgrade,
};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::AddExtensionLayer;
use log::LevelFilter;
use serde::Deserialize;
use std::env::current_dir;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use tokio::signal::ctrl_c;

/// A demo of Mediasoup in Rust
#[derive(Debug, StructOpt)]
#[structopt(name = "mediasoup-demo", about = "A demo of Mediasoup.")]
struct Opts {
    /// log level
    #[structopt(short = "l", long, default_value = "INFO")]
    log_level: LevelFilter,

    /// log root path
    #[structopt(short = "r", long, default_value = "./log")]
    log_root: PathBuf,

    /// http port
    #[structopt(short = "p", long, default_value = "8080")]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Opts {
        log_level,
        log_root,
        port,
    } = Opts::from_args();

    println!("Log level is {}.", log_level);
    println!(
        "Log root directory is {}.",
        current_dir()?.join(&log_root).canonicalize()?.display()
    );
    println!("Http port is {}.", port);

    let _handle = init_logger(log_level, log_root)?;

    let app = axum::Router::new()
        .layer(AddExtensionLayer::new(ServerState::default()))
        .route("/ws", axum::handler::get(ws_handler));

    let sock_addr = SocketAddr::from(([0, 0, 0, 0], port));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        log::info!("Http server started.");
        let graceful_server = axum::Server::bind(&sock_addr)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async {
                rx.await.ok();
            });

        // Await the `server` receiving the signal...
        if let Err(e) = graceful_server.await {
            log::error!("Http server error: {}", e);
        }
    });

    if ctrl_c().await.is_err() {
        anyhow::bail!("Signal listen error.");
    }
    println!("\nGot Ctrl-C, press again to exit.");

    if ctrl_c().await.is_err() {
        anyhow::bail!("Signal listen error.");
    }

    tx.send(()).unwrap_or_default();
    server_handle.await?;
    println!("\nAxum server shutdown gracefully.");

    println!("Bye!");

    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebsocketUpgradeQuery {
    pub room_id: Option<RoomId>,
}

async fn ws_handler(
    Query(WebsocketUpgradeQuery { room_id }): Query<WebsocketUpgradeQuery>,
    ws: WebSocketUpgrade,
    Extension(server_state): Extension<ServerState>,
) -> impl IntoResponse {
    let get_room = match room_id {
        None => server_state
            .rooms_registry
            .create_room(&server_state.worker_manger)
            .await
            .map_err(anyhow::Error::msg),
        Some(room_id) => server_state
            .rooms_registry
            .get_or_create_room(&server_state.worker_manger, room_id)
            .await
            .map_err(anyhow::Error::msg),
    };

    let room = match get_room {
        Ok(room) => room,
        Err(error) => {
            log::error!("get room error: {}", error);

            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot get or create a room",
            );
        }
    };

    let participant_connection = match ParticipantConnection::new(room).await {
        Ok(participant_connection) => participant_connection,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create transport",
            )
        }
    };

    (
        ws.on_upgrade(move |socket| async move {
            if let Err(e) = handle_socket(socket, participant_connection).await {
                log::error!("Websocket error: {}", e);
            }
        })
        .into_response()
        .status(),
        "Failed to upgrade",
    )
}

async fn handle_socket(mut socket: WebSocket, mut pc: ParticipantConnection) -> anyhow::Result<()> {
    let server_init_message = pc.build_init_message();

    socket
        .send(Message::Text(serde_json::to_string(&server_init_message)?))
        .await?;

    let mut server_message_receiver = pc.init_listen();

    loop {
        tokio::select! {
            server_message_recv = server_message_receiver.recv() => {
                if let Some(_message) = server_message_recv {
                }
            }
            websocket_recv = socket.recv() => {
                if let Some(result) = websocket_recv {
                    let message = result?;
                    match message {
                        Message::Text(text) => {
                            let client_message: participant::messages::ClientMessage =
                                serde_json::from_str(&text)?;

                            match client_message {
                                ClientMessage::Init { .. } => {}
                                ClientMessage::ConnectProducerTransport { .. } => {}
                                ClientMessage::Produce { .. } => {}
                                ClientMessage::ConnectConsumerTransport { .. } => {}
                                ClientMessage::Consume { .. } => {}
                                ClientMessage::ConsumerResume { .. } => {}
                            }
                        }
                        Message::Binary(_bytes) => {}
                        Message::Ping(_ping) => {}
                        Message::Pong(_pong) => {}
                        Message::Close(close) => {
                            if let Some(close_frame) = close {
                                log::debug!(
                                    "Received close frame: ({}, {})",
                                    close_frame.code,
                                    close_frame.reason
                                );
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

fn init_logger<P: AsRef<Path>>(level: LevelFilter, path: P) -> anyhow::Result<log4rs::Handle> {
    use log4rs::append::console::ConsoleAppender;
    use log4rs::append::rolling_file::policy::compound::roll::fixed_window::FixedWindowRoller;
    use log4rs::append::rolling_file::policy::compound::trigger::size::SizeTrigger;
    use log4rs::append::rolling_file::policy::compound::CompoundPolicy;
    use log4rs::append::rolling_file::RollingFileAppender;
    use log4rs::config::Appender;
    use log4rs::config::Root;
    use log4rs::Config;

    let archived_pattern = path
        .as_ref()
        .join("archived/msd-{}.log")
        .display()
        .to_string();
    let rolling_file_appender = RollingFileAppender::builder().build(
        path.as_ref().join("msd.log"),
        Box::new(CompoundPolicy::new(
            Box::new(SizeTrigger::new(1024 * 1024)),
            Box::new(FixedWindowRoller::builder().build(&archived_pattern, 20)?),
        )),
    )?;
    let console_appender = ConsoleAppender::builder().build();

    let config = Config::builder()
        .appender(Appender::builder().build("rolling", Box::new(rolling_file_appender)))
        .appender(Appender::builder().build("console", Box::new(console_appender)))
        .build(
            Root::builder()
                .appender("rolling")
                .appender("console")
                .build(level),
        )?;

    Ok(log4rs::init_config(config)?)
}
