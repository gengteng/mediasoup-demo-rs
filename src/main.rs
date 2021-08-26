mod codec;
mod participant;
mod room;
mod rooms_registry;

use crate::room::RoomId;
use crate::rooms_registry::ServerState;
use axum::extract::{
    ws::{Message, WebSocket},
    Extension, Query, WebSocketUpgrade,
};
use axum::response::IntoResponse;
use axum::AddExtensionLayer;
use log::LevelFilter;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use tokio::signal::ctrl_c;

#[derive(Debug, StructOpt)]
#[structopt(name = "mediasoup-demo", about = "A demo of Mediasoup.")]
struct Opts {
    /// log level
    #[structopt(short = "l", long)]
    log_level: LevelFilter,

    #[structopt(short = "r", long, default_value = "./log")]
    log_root: PathBuf,

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

    let _handle = init_logger(log_level, log_root)?;

    let app = axum::Router::new()
        .layer(AddExtensionLayer::new(ServerState::default()))
        .route("/ws", axum::handler::get(ws_handler));

    tokio::spawn(async move {
        let sock_addr = SocketAddr::from(([0, 0, 0, 0], port));
        if let Err(e) = axum::Server::bind(&sock_addr)
            .serve(app.into_make_service())
            .await
        {
            log::error!("Axum error: {}", e);
        }
    });

    if ctrl_c().await.is_err() {
        anyhow::bail!("Signal listen error");
    }
    println!("\nGot Ctrl-C, press again to exit.");

    if ctrl_c().await.is_err() {
        anyhow::bail!("Signal listen error");
    }
    println!("\nBye!");
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebsocketUpgradeQuery {
    pub room_id: Option<RoomId>,
}

async fn ws_handler(
    Query(WebsocketUpgradeQuery { room_id: _ }): Query<WebsocketUpgradeQuery>,
    ws: WebSocketUpgrade,
    Extension(_server_state): Extension<ServerState>,
) -> impl IntoResponse {
    // TODO: init websocket

    // use axum::body::Full;
    // use axum::http::Response;
    //
    // let get_room = match room_id {
    //     None => server_state
    //         .rooms_registry
    //         .create_room(&server_state.worker_manger)
    //         .await
    //         .map_err(anyhow::Error::msg),
    //     Some(room_id) => server_state
    //         .rooms_registry
    //         .get_or_create_room(&server_state.worker_manger, room_id)
    //         .await
    //         .map_err(anyhow::Error::msg),
    // };
    //
    // let room = match get_room {
    //     Ok(room) => room,
    //     Err(error) => {
    //         log::error!("get room error: {}", error);
    //
    //         return Response::builder()
    //             .status(StatusCode::INTERNAL_SERVER_ERROR)
    //             .body(Full::new(Bytes::new()))
    //             .unwrap();
    //     }
    // };

    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_socket(socket).await {
            log::error!("Websocket error: {}", e);
        }
    })
}

async fn handle_socket(mut socket: WebSocket) -> anyhow::Result<()> {
    if let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            println!("Client says: {:?}", msg);
        } else {
            println!("client disconnected");
            return Ok(());
        }
    }

    loop {
        if socket
            .send(Message::Text(String::from("Hi!")))
            .await
            .is_err()
        {
            println!("client disconnected");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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
