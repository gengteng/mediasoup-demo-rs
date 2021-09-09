mod codec;
mod participant;
pub(crate) mod room;
mod rooms_registry;
mod worker;

use crate::participant::ParticipantConnection;
use crate::room::RoomId;
use crate::rooms_registry::{RoomsRegistry, ServerState};
use crate::worker::WorkerPool;
use axum::extract::{Extension, Query, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::AddExtensionLayer;
use log::LevelFilter;
use log::*;
use mediasoup::worker::WorkerSettings;
use serde::Deserialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use tokio::signal::ctrl_c;
use tower_http::services::ServeDir;

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

    /// static file path
    #[structopt(short = "s", long, default_value = "./public")]
    static_path: PathBuf,

    /// http port
    #[structopt(short = "p", long, default_value = "8000")]
    port: u16,

    /// thread
    #[structopt(short = "t", long)]
    threads: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Opts {
        log_level,
        log_root,
        static_path,
        port,
        threads,
    } = Opts::from_args();

    let _handle = init_logger(log_level, &log_root)?;

    info!("Log level is {}.", log_level);
    tokio::fs::create_dir_all(&log_root).await?;
    info!("Log root directory is {}.", log_root.display());
    info!("Static file path is {}", static_path.display());
    let threads = match threads {
        None => {
            let cores = num_cpus::get();
            info!(
                "'threads' argument are not provided, the number of CPU cores ({}) will be used.",
                cores
            );
            cores
        }
        Some(threads) => {
            info!("Worker threads count is {}", threads);
            threads
        }
    };

    let mut worker_settings = WorkerSettings::default();
    worker_settings.rtc_ports_range = 40000..=40050;

    let worker_pool = WorkerPool::new(threads, worker_settings).await?;
    let rooms_registry = RoomsRegistry::default();

    let app = axum::Router::new()
        .nest(
            "/",
            axum::service::get(ServeDir::new(static_path))
                .handle_error(|_| Ok::<_, Infallible>((StatusCode::INTERNAL_SERVER_ERROR, ""))),
        )
        .route("/ws", axum::handler::get(ws_handler))
        .layer(AddExtensionLayer::new(ServerState {
            worker_pool,
            rooms_registry,
        }));

    let sock_addr = SocketAddr::from(([0, 0, 0, 0], port));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        log::info!("Http server started ({}).", sock_addr);
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
    println!("\rGot Ctrl-C, press again to exit.");

    if ctrl_c().await.is_err() {
        anyhow::bail!("Signal listen error.");
    }
    tx.send(()).unwrap_or_default();
    server_handle.await?;
    info!("\rHttp server shutdown gracefully.");

    info!("Bye!");

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
    ws.on_upgrade(move |socket| async move {
        {
            let worker = server_state.worker_pool.next();
            let get_room = match room_id {
                None => server_state
                    .rooms_registry
                    .create_room(worker)
                    .await
                    .map_err(anyhow::Error::msg),
                Some(room_id) => server_state
                    .rooms_registry
                    .get_or_create_room(worker, room_id)
                    .await
                    .map_err(anyhow::Error::msg),
            };

            let room = match get_room {
                Ok(room) => {
                    debug!("Room {} created.", room.id());
                    room
                }
                Err(error) => {
                    error!("get room error: {}", error);
                    return;
                }
            };

            let participant_connection = match ParticipantConnection::new(room).await {
                Ok(participant_connection) => participant_connection,
                Err(e) => {
                    error!("Failed to create transport: {}", e);
                    return;
                }
            };

            if let Err(e) = participant_connection.run(socket, server_state).await {
                error!("participant error: {}", e);
            }
        }
    })
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
