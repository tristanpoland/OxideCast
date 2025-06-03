use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::{get, State};
use rocket_db_pools::{Connection, Database};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::api::ApiResponse;
use crate::auth::{AuthenticatedUser, AdminUser};
use crate::database::{Db, Stream};
use crate::AppState;

#[derive(serde::Serialize)]
pub struct ServerStats {
    pub active_streams: usize,
    pub total_viewers: usize,
    pub total_streams_created: u64,
    pub multicast_stats: MulticastStatsResponse,
    pub transcoding_stats: TranscodingStatsResponse,
    pub webrtc_stats: WebRtcStatsResponse,
    pub hls_stats: HlsStatsResponse,
    pub uptime_seconds: u64,
    pub version: String,
}

#[derive(serde::Serialize)]
pub struct MulticastStatsResponse {
    pub active_groups: usize,
    pub total_viewers: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
}

#[derive(serde::Serialize)]
pub struct TranscodingStatsResponse {
    pub total_jobs: usize,
    pub running_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub available_slots: usize,
    pub total_frames_processed: u64,
    pub average_fps: f32,
}

#[derive(serde::Serialize)]
pub struct WebRtcStatsResponse {
    pub active_connections: usize,
    pub total_connections: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
}

#[derive(serde::Serialize)]
pub struct HlsStatsResponse {
    pub active_streams: usize,
    pub total_segments: usize,
    pub total_viewers: usize,
}

#[derive(serde::Serialize)]
pub struct StreamStats {
    pub stream_id: Uuid,
    pub title: String,
    pub is_live: bool,
    pub viewer_count: i32,
    pub max_viewers: i32,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub duration_seconds: Option<i64>,
    pub multicast_stats: Option<MulticastStreamStats>,
    pub webrtc_stats: Option<WebRtcStreamStats>,
    pub hls_stats: Option<HlsStreamStats>,
    pub transcoding_stats: Option<TranscodingStreamStats>,
}

#[derive(serde::Serialize)]
pub struct MulticastStreamStats {
    pub multicast_address: String,
    pub multicast_port: u16,
    pub viewer_count: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
}

#[derive(serde::Serialize)]
pub struct WebRtcStreamStats {
    pub viewer_count: usize,
    pub total_connections: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
}

#[derive(serde::Serialize)]
pub struct HlsStreamStats {
    pub segment_count: usize,
    pub total_duration: f32,
    pub current_sequence: u64,
    pub viewer_count: usize,
}

#[derive(serde::Serialize)]
pub struct TranscodingStreamStats {
    pub job_status: String,
    pub frames_processed: u64,
    pub bitrate_kbps: u32,
    pub fps: f32,
    pub profiles: Vec<String>,
}

#[get("/stats")]
pub async fn get_server_stats(
    mut db: Connection<Db>,
    state: &State<AppState>,
    _admin: AdminUser,
) -> Result<Json<ApiResponse<ServerStats>>, status::Custom<Json<ApiResponse<()>>>> {
    // Get active streams
    let active_streams = state.stream_manager.get_active_streams().await;

    // Get total viewers across all streams
    let total_viewers = active_streams.iter().map(|s| s.viewer_count).sum();

    // Get total streams count from database
    let total_streams_created = match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM streams")
        .fetch_one(&mut **db)
        .await
    {
        Ok(count) => count as u64,
        Err(e) => {
            error!("Database error getting stream count: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // Get multicast stats
    let multicast_stats = state.multicast_manager.get_stats().await;
    let multicast_stats_response = MulticastStatsResponse {
        active_groups: multicast_stats.active_groups,
        total_viewers: multicast_stats.total_viewers,
        packets_sent: multicast_stats.packets_sent,
        bytes_transmitted: multicast_stats.bytes_transmitted,
    };

    // Get transcoding stats
    let transcoding_stats = state.transcoder_manager.get_stats().await;
    let transcoding_stats_response = TranscodingStatsResponse {
        total_jobs: transcoding_stats.total_jobs,
        running_jobs: transcoding_stats.running_jobs,
        completed_jobs: transcoding_stats.completed_jobs,
        failed_jobs: transcoding_stats.failed_jobs,
        available_slots: transcoding_stats.available_slots,
        total_frames_processed: transcoding_stats.total_frames_processed,
        average_fps: transcoding_stats.average_fps,
    };

    // Get WebRTC stats (simplified)
    let webrtc_stats_response = WebRtcStatsResponse {
        active_connections: active_streams.len(), // Simplified
        total_connections: active_streams.len(),
        packets_sent: 0,
        bytes_transmitted: 0,
    };

    // Get HLS stats (simplified)
    let hls_stats_response = HlsStatsResponse {
        active_streams: active_streams.len(),
        total_segments: 0,
        total_viewers: total_viewers,
    };

    let server_stats = ServerStats {
        active_streams: active_streams.len(),
        total_viewers,
        total_streams_created,
        multicast_stats: multicast_stats_response,
        transcoding_stats: transcoding_stats_response,
        webrtc_stats: webrtc_stats_response,
        hls_stats: hls_stats_response,
        uptime_seconds: multicast_stats.uptime,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    Ok(Json(ApiResponse::success(server_stats)))
}

#[get("/streams/<stream_id>/stats")]
pub async fn get_stream_stats(
    mut db: Connection<Db>,
    state: &State<AppState>,
    user: AuthenticatedUser,
    stream_id: Uuid,
) -> Result<Json<ApiResponse<StreamStats>>, status::Custom<Json<ApiResponse<()>>>> {
    // Get stream from database
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

    // Check if user can view stats (owner or admin)
    if stream.user_id != user.id {
        // For public streams, anyone can view basic stats
        // For private streams, only the owner can view stats
        if stream.is_private {
            return Err(status::Custom(
                Status::Forbidden,
                Json(ApiResponse::error("You don't have permission to view these stats".to_string())),
            ));
        }
    }

    // Calculate duration if stream is live
    let duration_seconds = if stream.is_live {
        stream.started_at.map(|start| {
            chrono::Utc::now().signed_duration_since(start).num_seconds()
        })
    } else {
        stream.started_at.zip(stream.ended_at).map(|(start, end)| {
            end.signed_duration_since(start).num_seconds()
        })
    };

    // Get multicast stats if available
    let multicast_stats = if let Some(stream_info) = state.stream_manager.get_stream_info(stream_id).await {
        if let Some(multicast_group) = state.multicast_manager.get_group_info(&stream_info.stream_key).await {
            Some(MulticastStreamStats {
                multicast_address: multicast_group.multicast_address.to_string(),
                multicast_port: multicast_group.port,
                viewer_count: multicast_group.viewer_count,
                packets_sent: multicast_group.packet_count,
                bytes_transmitted: multicast_group.bytes_sent,
            })
        } else {
            None
        }
    } else {
        None
    };

    // Get WebRTC stats if available
    let webrtc_stats = if let Some(stream_info) = state.stream_manager.get_stream_info(stream_id).await {
        state.webrtc_server.get_stream_stats(&stream_info.stream_key).await.map(|stats| {
            WebRtcStreamStats {
                viewer_count: stats.viewer_count,
                total_connections: stats.total_connections,
                packets_sent: stats.packets_sent,
                bytes_transmitted: stats.bytes_transmitted,
            }
        })
    } else {
        None
    };

    // Get HLS stats if available
    let hls_stats = if let Some(stream_info) = state.stream_manager.get_stream_info(stream_id).await {
        state.hls_server.get_stream_stats(&stream_info.stream_key).await.map(|stats| {
            HlsStreamStats {
                segment_count: stats.segment_count,
                total_duration: stats.total_duration,
                current_sequence: stats.current_sequence,
                viewer_count: stats.viewer_count,
            }
        })
    } else {
        None
    };

    // Get transcoding stats if available
    let transcoding_stats = if let Some(stream_info) = state.stream_manager.get_stream_info(stream_id).await {
        if let Some(job_id) = stream_info.transcoding_job_id {
            state.transcoder_manager.get_job_status(job_id).await.map(|job| {
                TranscodingStreamStats {
                    job_status: format!("{:?}", job.status),
                    frames_processed: job.progress.frames_processed,
                    bitrate_kbps: job.progress.bitrate_kbps,
                    fps: job.progress.fps,
                    profiles: job.output_profiles,
                }
            })
        } else {
            None
        }
    } else {
        None
    };

    let stats = StreamStats {
        stream_id: stream.id,
        title: stream.title,
        is_live: stream.is_live,
        viewer_count: stream.viewer_count,
        max_viewers: stream.max_viewers,
        started_at: stream.started_at,
        duration_seconds,
        multicast_stats,
        webrtc_stats,
        hls_stats,
        transcoding_stats,
    };

    Ok(Json(ApiResponse::success(stats)))
}