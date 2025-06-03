use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::{get, post, put, delete, State};
use rocket_db_pools::{Connection, Database};
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::api::{ApiResponse, PaginationParams, PaginatedResponse};
use crate::auth::{AuthenticatedUser, StreamerUser};
use crate::database::{Db, Stream, CreateStreamRequest, UpdateStreamRequest};
use crate::stream_manager::ConnectionType;
use crate::AppState;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StreamResponse {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_live: bool,
    pub is_private: bool,
    pub viewer_count: i32,
    pub max_viewers: i32,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub thumbnail_url: Option<String>,
    pub stream_urls: Option<StreamUrls>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StreamUrls {
    pub multicast: Option<String>,
    pub webrtc: Option<String>,
    pub hls: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CreateStreamRequestApi {
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_private: Option<bool>,
    pub recording_enabled: Option<bool>,
    pub transcoding_profiles: Option<Vec<String>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct UpdateStreamRequestApi {
    pub title: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_private: Option<bool>,
    pub recording_enabled: Option<bool>,
    pub transcoding_profiles: Option<Vec<String>>,
}

#[get("/streams?<page>&<limit>")]
pub async fn list_streams(
    mut db: Connection<Db>,
    page: Option<u32>,
    limit: Option<u32>,
) -> Result<Json<ApiResponse<PaginatedResponse<StreamResponse>>>, status::Custom<Json<ApiResponse<()>>>> {
    let pagination = PaginationParams { page, limit };
    
    let streams = match Stream::find_live_streams(&mut **db, pagination.limit() as i64, pagination.offset() as i64).await {
        Ok(streams) => streams,
        Err(e) => {
            error!("Database error listing streams: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let stream_responses: Vec<StreamResponse> = streams.into_iter().map(|stream| {
        StreamResponse {
            id: stream.id,
            title: stream.title,
            description: stream.description,
            category: stream.category,
            is_live: stream.is_live,
            is_private: stream.is_private,
            viewer_count: stream.viewer_count,
            max_viewers: stream.max_viewers,
            started_at: stream.started_at,
            created_at: stream.created_at,
            thumbnail_url: stream.thumbnail_url,
            stream_urls: None, // Don't include URLs in list view
        }
    }).collect();

    // For simplicity, we'll use the count of returned items as total
    // In production, you'd want a separate count query
    let total = stream_responses.len() as u64;

    let response = PaginatedResponse::new(stream_responses, total, pagination.page(), pagination.limit());
    Ok(Json(ApiResponse::success(response)))
}

#[get("/streams/<stream_id>")]
pub async fn get_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<StreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
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

    let stream_urls = if stream.is_live {
        let multicast_url = if let (Some(addr), Some(port)) = (&stream.multicast_address, stream.multicast_port) {
            Some(format!("udp://{}:{}", addr, port))
        } else {
            None
        };

        let webrtc_url = state.stream_manager.get_stream_url(stream_id, ConnectionType::WebRtc).await.ok();
        let hls_url = state.stream_manager.get_stream_url(stream_id, ConnectionType::Hls).await.ok();

        Some(StreamUrls {
            multicast: multicast_url,
            webrtc: webrtc_url,
            hls: hls_url,
        })
    } else {
        None
    };

    let response = StreamResponse {
        id: stream.id,
        title: stream.title,
        description: stream.description,
        category: stream.category,
        is_live: stream.is_live,
        is_private: stream.is_private,
        viewer_count: stream.viewer_count,
        max_viewers: stream.max_viewers,
        started_at: stream.started_at,
        created_at: stream.created_at,
        thumbnail_url: stream.thumbnail_url,
        stream_urls,
    };

    Ok(Json(ApiResponse::success(response)))
}

#[post("/streams", data = "<request>")]
pub async fn create_stream(
    mut db: Connection<Db>,
    user: StreamerUser,
    request: Json<CreateStreamRequestApi>,
) -> Result<Json<ApiResponse<StreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    // Validate title
    if request.title.trim().is_empty() || request.title.len() > 255 {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Title must be between 1 and 255 characters".to_string())),
        ));
    }

    // Default transcoding profiles if not specified
    let transcoding_profiles = request.transcoding_profiles.unwrap_or_else(|| vec!["720p".to_string()]);

    let create_request = CreateStreamRequest {
        title: request.title,
        description: request.description,
        category: request.category,
        is_private: request.is_private.unwrap_or(false),
        recording_enabled: request.recording_enabled.unwrap_or(false),
        transcoding_profiles,
    };

    let stream = match Stream::create(&mut **db, user.0.id, create_request).await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Database error creating stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = StreamResponse {
        id: stream.id,
        title: stream.title,
        description: stream.description,
        category: stream.category,
        is_live: stream.is_live,
        is_private: stream.is_private,
        viewer_count: stream.viewer_count,
        max_viewers: stream.max_viewers,
        started_at: stream.started_at,
        created_at: stream.created_at,
        thumbnail_url: stream.thumbnail_url,
        stream_urls: None,
    };

    info!("Created stream '{}' for user '{}'", stream.title, user.0.username);
    Ok(Json(ApiResponse::success(response)))
}

#[put("/streams/<stream_id>", data = "<request>")]
pub async fn update_stream(
    mut db: Connection<Db>,
    user: StreamerUser,
    stream_id: Uuid,
    request: Json<UpdateStreamRequestApi>,
) -> Result<Json<ApiResponse<StreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    let mut stream = match Stream::find_by_id(&mut **db, stream_id).await {
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

    // Check ownership
    if stream.user_id != user.0.id {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("You don't own this stream".to_string())),
        ));
    }

    // Validate title if provided
    if let Some(ref title) = request.title {
        if title.trim().is_empty() || title.len() > 255 {
            return Err(status::Custom(
                Status::BadRequest,
                Json(ApiResponse::error("Title must be between 1 and 255 characters".to_string())),
            ));
        }
    }

    let update_request = UpdateStreamRequest {
        title: request.title,
        description: request.description,
        category: request.category,
        is_private: request.is_private,
        recording_enabled: request.recording_enabled,
        transcoding_profiles: request.transcoding_profiles,
    };

    if let Err(e) = stream.update(&mut **db, update_request).await {
        error!("Database error updating stream: {}", e);
        return Err(status::Custom(
            Status::InternalServerError,
            Json(ApiResponse::error("Internal server error".to_string())),
        ));
    }

    let response = StreamResponse {
        id: stream.id,
        title: stream.title,
        description: stream.description,
        category: stream.category,
        is_live: stream.is_live,
        is_private: stream.is_private,
        viewer_count: stream.viewer_count,
        max_viewers: stream.max_viewers,
        started_at: stream.started_at,
        created_at: stream.created_at,
        thumbnail_url: stream.thumbnail_url,
        stream_urls: None,
    };

    info!("Updated stream '{}' for user '{}'", stream.title, user.0.username);
    Ok(Json(ApiResponse::success(response)))
}

#[delete("/streams/<stream_id>")]
pub async fn delete_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: StreamerUser,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<()>>, status::Custom<Json<ApiResponse<()>>>> {
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

    // Check ownership
    if stream.user_id != user.0.id {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("You don't own this stream".to_string())),
        ));
    }

    // Stop stream if it's live
    if stream.is_live {
        if let Err(e) = state.stream_manager.stop_stream(stream_id).await {
            warn!("Failed to stop stream during deletion: {}", e);
        }
    }

    // Delete from database
    if let Err(e) = sqlx::query("DELETE FROM streams WHERE id = $1")
        .bind(stream_id)
        .execute(&mut **db)
        .await
    {
        error!("Database error deleting stream: {}", e);
        return Err(status::Custom(
            Status::InternalServerError,
            Json(ApiResponse::error("Internal server error".to_string())),
        ));
    }

    info!("Deleted stream '{}' for user '{}'", stream.title, user.0.username);
    Ok(Json(ApiResponse::success(())))
}

#[post("/streams/<stream_id>/start")]
pub async fn start_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: StreamerUser,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<StreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
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

    // Check ownership
    if stream.user_id != user.0.id {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("You don't own this stream".to_string())),
        ));
    }

    // Check if already live
    if stream.is_live {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Stream is already live".to_string())),
        ));
    }

    // Start the stream
    if let Err(e) = state.stream_manager.start_stream(stream_id).await {
        error!("Failed to start stream: {}", e);
        return Err(status::Custom(
            Status::InternalServerError,
            Json(ApiResponse::error("Failed to start stream".to_string())),
        ));
    }

    // Get updated stream info
    let updated_stream = match Stream::find_by_id(&mut **db, stream_id).await {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Stream disappeared after starting".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error getting updated stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = StreamResponse {
        id: updated_stream.id,
        title: updated_stream.title,
        description: updated_stream.description,
        category: updated_stream.category,
        is_live: updated_stream.is_live,
        is_private: updated_stream.is_private,
        viewer_count: updated_stream.viewer_count,
        max_viewers: updated_stream.max_viewers,
        started_at: updated_stream.started_at,
        created_at: updated_stream.created_at,
        thumbnail_url: updated_stream.thumbnail_url,
        stream_urls: None,
    };

    info!("Started stream '{}' for user '{}'", updated_stream.title, user.0.username);
    Ok(Json(ApiResponse::success(response)))
}

#[post("/streams/<stream_id>/stop")]
pub async fn stop_stream(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: StreamerUser,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<StreamResponse>>, status::Custom<Json<ApiResponse<()>>>> {
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

    // Check ownership
    if stream.user_id != user.0.id {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("You don't own this stream".to_string())),
        ));
    }

    // Check if not live
    if !stream.is_live {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Stream is not live".to_string())),
        ));
    }

    // Stop the stream
    if let Err(e) = state.stream_manager.stop_stream(stream_id).await {
        error!("Failed to stop stream: {}", e);
        return Err(status::Custom(
            Status::InternalServerError,
            Json(ApiResponse::error("Failed to stop stream".to_string())),
        ));
    }

    // Get updated stream info
    let updated_stream = match Stream::find_by_id(&mut **db, stream_id).await {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Stream disappeared after stopping".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error getting updated stream: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = StreamResponse {
        id: updated_stream.id,
        title: updated_stream.title,
        description: updated_stream.description,
        category: updated_stream.category,
        is_live: updated_stream.is_live,
        is_private: updated_stream.is_private,
        viewer_count: updated_stream.viewer_count,
        max_viewers: updated_stream.max_viewers,
        started_at: updated_stream.started_at,
        created_at: updated_stream.created_at,
        thumbnail_url: updated_stream.thumbnail_url,
        stream_urls: None,
    };

    info!("Stopped stream '{}' for user '{}'", updated_stream.title, user.0.username);
    Ok(Json(ApiResponse::success(response)))
}