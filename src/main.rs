use anyhow::Result;
use clap::Parser;
use rocket::{launch, routes, catchers, get, State, response::status, http::Status};
use rocket::serde::json::Json;
use rocket_db_pools::{Database, Connection};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, error};
use uuid::Uuid;

mod config;
mod database;
mod auth;
mod rtmp_server;
mod transcoder;
mod multicast;
mod webrtc_server;
mod hls_server;
mod api;
mod stream_manager;

use config::AppConfig;
use database::{Db, setup_database};
use auth::AuthManager;
use rtmp_server::RtmpServer;
use transcoder::TranscoderManager;
use multicast::MulticastManager;
use webrtc_server::WebRtcServer;
use hls_server::HlsServer;
use stream_manager::StreamManager;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

pub struct AppState {
    pub config: AppConfig,
    pub stream_manager: Arc<StreamManager>,
    pub auth_manager: Arc<AuthManager>,
    pub rtmp_server: Arc<RtmpServer>,
    pub transcoder_manager: Arc<TranscoderManager>,
    pub multicast_manager: Arc<MulticastManager>,
    pub webrtc_server: Arc<WebRtcServer>,
    pub hls_server: Arc<HlsServer>,
}

#[get("/health")]
async fn health_check() -> Result<Json<serde_json::Value>, Status> {
    Ok(Json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now(),
        "version": env!("CARGO_PKG_VERSION")
    })))
}

#[get("/")]
async fn index() -> &'static str {
    include_str!("../static/index.html")
}

#[get("/player")]
async fn player() -> &'static str {
    include_str!("../static/player.html")
}

async fn initialize_services(config: &AppConfig, pool: &PgPool) -> Result<AppState> {
    info!("Initializing services...");

    let auth_manager = Arc::new(AuthManager::new(config.auth.clone()));
    let multicast_manager = Arc::new(MulticastManager::new(config.multicast.clone()).await?);
    let transcoder_manager = Arc::new(TranscoderManager::new(config.transcoding.clone()).await?);
    let hls_server = Arc::new(HlsServer::new(config.hls.clone()).await?);
    let webrtc_server = Arc::new(WebRtcServer::new(config.webrtc.clone()).await?);
    
    let stream_manager = Arc::new(StreamManager::new(
        pool.clone(),
        multicast_manager.clone(),
        transcoder_manager.clone(),
        hls_server.clone(),
        webrtc_server.clone(),
    ).await?);

    let rtmp_server = Arc::new(RtmpServer::new(
        config.rtmp.clone(),
        stream_manager.clone(),
    ).await?);

    // Start background services
    tokio::spawn({
        let rtmp_server = rtmp_server.clone();
        async move {
            if let Err(e) = rtmp_server.start().await {
                error!("RTMP server failed: {}", e);
            }
        }
    });

    tokio::spawn({
        let multicast_manager = multicast_manager.clone();
        async move {
            if let Err(e) = multicast_manager.start().await {
                error!("Multicast manager failed: {}", e);
            }
        }
    });

    tokio::spawn({
        let webrtc_server = webrtc_server.clone();
        async move {
            if let Err(e) = webrtc_server.start().await {
                error!("WebRTC server failed: {}", e);
            }
        }
    });

    info!("All services initialized successfully");

    Ok(AppState {
        config: config.clone(),
        stream_manager,
        auth_manager,
        rtmp_server,
        transcoder_manager,
        multicast_manager,
        webrtc_server,
        hls_server,
    })
}

#[launch]
async fn rocket() -> _ {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    
    let config = AppConfig::load(&args.config)
        .expect("Failed to load configuration");

    info!("Starting Multicast Streaming Server v{}", env!("CARGO_PKG_VERSION"));
    info!("Configuration loaded from: {}", args.config);

    let pool = setup_database(&config.database.url)
        .await
        .expect("Failed to setup database");

    let app_state = initialize_services(&config, &pool)
        .await
        .expect("Failed to initialize services");

    rocket::build()
        .manage(app_state)
        .attach(Db::init())
        .mount("/", routes![
            index,
            player,
            health_check,
        ])
        .mount("/api/v1", routes![
            api::streams::list_streams,
            api::streams::get_stream,
            api::streams::create_stream,
            api::streams::update_stream,
            api::streams::delete_stream,
            api::streams::start_stream,
            api::streams::stop_stream,
            api::auth::login,
            api::auth::register,
            api::auth::refresh_token,
            api::viewers::join_stream,
            api::viewers::leave_stream,
            api::viewers::get_stream_url,
            api::stats::get_server_stats,
            api::stats::get_stream_stats,
        ])
        .register("/", catchers![
            api::errors::not_found,
            api::errors::internal_error,
            api::errors::unauthorized,
            api::errors::bad_request,
        ])
}