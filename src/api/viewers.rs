use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::{get, post, delete, State};
use rocket_db_pools::{Connection, Database};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::api::ApiResponse;
use crate::auth::AuthenticatedUser;
use crate::database::{Db, Stream};
use crate::stream_manager::{ViewerInfo, ConnectionType};
use crate::AppState;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JoinStreamRequest {
    pub connection_type: String, // "multicast", "webrtc", "hls"
    pub quality_profile: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JoinStreamResponse {
    pub viewer_id: Uuid,
    pub stream_url: String,
    pub connection_type: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StreamUrlRequest {
    pub connection_type: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StreamUrlResponse {
    pub url: String,
    pub connection_type: String,
}

fn parse_connection_type(connection_type: &str) -> Result<ConnectionType, String> {
    match connection_type.to_lowercase().as_str() {
        "multicast" => Ok(ConnectionType::Multicast),
        "webrtc" => Ok(ConnectionType::WebRtc),
        "hls" => Ok(ConnectionType::Hls),
        _ => Err(format!("Unsupported connection type: {}", connection_type)),
    }
}

#[post("/streams/<stream_id>/join", data = "<request>")]
pub async fn join_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: Option<AuthenticatedUser>,
    stream_id: Uuid,
    request: Json<JoinStreamRequest>,
) -> Result<Json<ApiResponse<JoinStreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    // Validate stream exists and is live
    let stream = match Stream::find_by_id(&mut **db, stream_id).await {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            return Err(status::Custom(
                Status::NotFound,
                Json(ApiResponse::error("Stream not found".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error getting stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    if !stream.is_live {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Stream is not live".to_string())),
        ));
    }

    // Parse connection type
    let connection_type = match parse_connection_type(&request.connection_type) {
        Ok(ct) => ct,
        Err(msg) => {
            return Err(status::Custom(
                Status::BadRequest,
                Json(ApiResponse::error(msg)),
            ));
        }
    };

    // Get stream URL
    let stream_url = match state.stream_manager.get_stream_url(stream_id, connection_type.clone()).await {
        Ok(url) => url,
        Err(e) => {
            error!("Failed to get stream URL: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Failed to get stream URL".to_string())),
            ));
        }
    };

    // Create viewer info
    let viewer_info = ViewerInfo {
        id: Uuid::new_v4(),
        user_id: user.as_ref().map(|u| u.id),
        ip_address: "127.0.0.1".to_string(), // Should get from request headers
        user_agent: request.user_agent.clone(),
        joined_at: chrono::Utc::now(),
        connection_type: connection_type.clone(),
        quality_profile: request.quality_profile.unwrap_or_else(|| "720p".to_string()),
        last_heartbeat: chrono::Utc::now(),
    };

    // Add viewer to stream
    let viewer_id = match state.stream_manager.add_viewer(stream_id, viewer_info).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to add viewer: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Failed to join stream".to_string())),
            ));
        }
    };

    let response = JoinStreamResponse {
        viewer_id,
        stream_url,
        connection_type: request.connection_type,
    };

    let username = user.as_ref().map(|u| u.username.as_str()).unwrap_or("anonymous");
    info!("User '{}' joined stream '{}' via {}", username, stream.title, request.connection_type);

    Ok(Json(ApiResponse::success(response)))
}

#[delete("/streams/<stream_id>/leave")]
pub async fn leave_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: Option<AuthenticatedUser>,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<()>>, status::Custom<Json<ApiResponse<()>>>> {
    // Validate stream exists
    let stream = match Stream::find_by_id(&mut **db, stream_id).await {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            return Err(status::Custom(
                Status::NotFound,
                Json(ApiResponse::error("Stream not found".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error getting stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // For simplicity, we'll use a placeholder address
    // In production, you'd track viewers by session ID or similar
    let viewer_addr = SocketAddr::from(([127, 0, 0, 1], 8000));

    // Remove viewer from stream
    if let Err(e) = state.stream_manager.remove_viewer(stream_id, viewer_addr).await {
        warn!("Failed to remove viewer (may already be gone): {}", e);
    }

    let username = user.as_ref().map(|u| u.username.as_str()).unwrap_or("anonymous");
    info!("User '{}' left stream '{}'", username, stream.title);

    Ok(Json(ApiResponse::success(())))
}

#[get("/streams/<stream_id>/url?<connection_type>")]
pub async fn get_stream_url(
    mut db: Connection<Db>,
    state: &State<AppState>,
    stream_id: Uuid,
    connection_type: String,
) -> Result<Json<ApiResponse<StreamUrlResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    // Validate stream exists and is live
    let stream = match Stream::find_by_id(&mut **db, stream_id).await {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            return Err(status::Custom(
                Status::NotFound,
                Json(ApiResponse::error("Stream not found".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error getting stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    if !stream.is_live {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Stream is not live".to_string())),
        ));
    }

    // Parse connection type
    let conn_type = match parse_connection_type(&connection_type) {
        Ok(ct) => ct,
        Err(msg) => {
            return Err(status::Custom(
                Status::BadRequest,
                Json(ApiResponse::error(msg)),
            ));
        }
    };

    // Get stream URL
    let url = match state.stream_manager.get_stream_url(stream_id, conn_type).await {
        Ok(url) => url,
        Err(e) => {
            error!("Failed to get stream URL: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Failed to get stream URL".to_string())),
            ));
        }
    };

    let response = StreamUrlResponse {
        url,
        connection_type,
    };

    Ok(Json(ApiResponse::success(response)))
}