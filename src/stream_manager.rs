use anyhow::{Result, Context};
use bytes::Bytes;
use futures::stream::StreamExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::database::{Stream, StreamSession, User, StreamMetrics};
use crate::multicast::{MulticastManager, MulticastPacket, PayloadType};
use crate::transcoder::{TranscoderManager, TranscodingRequest, InputSource, OutputFormat};
use crate::webrtc_server::WebRtcServer;
use crate::hls_server::HlsServer;

pub struct StreamManager {
    db_pool: PgPool,
    multicast_manager: Arc<MulticastManager>,
    transcoder_manager: Arc<TranscoderManager>,
    webrtc_server: Arc<WebRtcServer>,
    hls_server: Arc<HlsServer>,
    active_streams: Arc<RwLock<HashMap<Uuid, ActiveStreamInfo>>>,
    stream_sessions: Arc<RwLock<HashMap<Uuid, StreamSessionInfo>>>,
    viewer_tracking: Arc<RwLock<HashMap<Uuid, ViewerTracking>>>,
    metrics_collector: Arc<MetricsCollector>,
}

#[derive(Debug, Clone)]
pub struct ActiveStreamInfo {
    pub stream_id: Uuid,
    pub stream_key: String,
    pub user_id: Uuid,
    pub title: String,
    pub multicast_address: String,
    pub multicast_port: u16,
    pub transcoding_job_id: Option<Uuid>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_frame_at: chrono::DateTime<chrono::Utc>,
    pub viewer_count: usize,
    pub total_viewers: usize,
    pub bitrate_kbps: u32,
    pub frame_rate: f32,
    pub resolution: (u32, u32),
    pub is_recording: bool,
    pub quality_profiles: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StreamSessionInfo {
    pub session_id: Uuid,
    pub stream_id: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub viewers: HashMap<Uuid, ViewerInfo>,
    pub metrics: StreamMetricsData,
}

#[derive(Debug, Clone)]
pub struct ViewerInfo {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub ip_address: String,
    pub user_agent: Option<String>,
    pub joined_at: chrono::DateTime<chrono::Utc>,
    pub connection_type: ConnectionType,
    pub quality_profile: String,
    pub last_heartbeat: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub enum ConnectionType {
    Multicast,
    WebRtc,
    Hls,
}

#[derive(Debug, Clone)]
pub struct ViewerTracking {
    pub stream_id: Uuid,
    pub viewers: HashMap<SocketAddr, ViewerSession>,
    pub total_join_count: usize,
    pub peak_concurrent: usize,
}

#[derive(Debug, Clone)]
pub struct ViewerSession {
    pub viewer_id: Uuid,
    pub joined_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub bytes_received: u64,
    pub quality_profile: String,
    pub connection_quality: ConnectionQuality,
}

#[derive(Debug, Clone)]
pub struct ConnectionQuality {
    pub latency_ms: u32,
    pub packet_loss: f32,
    pub jitter_ms: u32,
    pub bandwidth_kbps: u32,
}

#[derive(Debug, Clone)]
pub struct StreamMetricsData {
    pub frames_processed: u64,
    pub bytes_transmitted: u64,
    pub dropped_frames: u32,
    pub encoding_bitrate: u32,
    pub network_throughput: u64,
    pub cpu_usage: f32,
    pub memory_usage: u64,
}

pub struct MetricsCollector {
    db_pool: PgPool,
    active_streams: Arc<RwLock<HashMap<Uuid, ActiveStreamInfo>>>,
}

impl StreamManager {
    pub async fn new(
        db_pool: PgPool,
        multicast_manager: Arc<MulticastManager>,
        transcoder_manager: Arc<TranscoderManager>,
        hls_server: Arc<HlsServer>,
        webrtc_server: Arc<WebRtcServer>,
    ) -> Result<Self> {
        let active_streams = Arc::new(RwLock::new(HashMap::new()));
        let stream_sessions = Arc::new(RwLock::new(HashMap::new()));
        let viewer_tracking = Arc::new(RwLock::new(HashMap::new()));
        
        let metrics_collector = Arc::new(MetricsCollector {
            db_pool: db_pool.clone(),
            active_streams: active_streams.clone(),
        });

        let manager = Self {
            db_pool,
            multicast_manager,
            transcoder_manager,
            webrtc_server,
            hls_server,
            active_streams,
            stream_sessions,
            viewer_tracking,
            metrics_collector,
        };

        // Start background tasks
        manager.start_background_tasks().await?;

        Ok(manager)
    }

    async fn start_background_tasks(&self) -> Result<()> {
        // Metrics collection task
        let metrics_collector = self.metrics_collector.clone();
        tokio::spawn(async move {
            metrics_collector.start_collection().await;
        });

        // Stream health monitoring
        let active_streams = self.active_streams.clone();
        let db_pool = self.db_pool.clone();
        tokio::spawn(async move {
            Self::stream_health_monitor(active_streams, db_pool).await;
        });

        // Viewer cleanup task
        let viewer_tracking = self.viewer_tracking.clone();
        tokio::spawn(async move {
            Self::viewer_cleanup_task(viewer_tracking).await;
        });

        info!("Started stream manager background tasks");
        Ok(())
    }

    pub async fn authenticate_stream_key(&self, stream_key: &str) -> Result<Option<Uuid>> {
        // Find stream by stream key
        if let Some(stream) = Stream::find_by_stream_key(&self.db_pool, stream_key).await? {
            // Verify the user is active and can stream
            if let Some(user) = User::find_by_id(&self.db_pool, stream.user_id).await? {
                if user.is_active {
                    return Ok(Some(stream.id));
                }
            }
        }
        Ok(None)
    }

    pub async fn start_stream(&self, stream_id: Uuid) -> Result<()> {
        // Get stream from database
        let mut stream = Stream::find_by_id(&self.db_pool, stream_id).await?
            .ok_or_else(|| anyhow::anyhow!("Stream not found"))?;

        if stream.is_live {
            return Err(anyhow::anyhow!("Stream is already live"));
        }

        // Create multicast group
        let (multicast_addr, multicast_port) = self.multicast_manager
            .create_group(stream.stream_key.clone()).await?;

        // Update stream in database
        stream.start_streaming(&self.db_pool, &multicast_addr.to_string(), multicast_port as i32).await?;

        // Start transcoding job
        let transcoding_request = TranscodingRequest {
            stream_key: stream.stream_key.clone(),
            input_source: InputSource::Rtmp { stream_key: stream.stream_key.clone() },
            profiles: stream.transcoding_profiles.clone(),
            output_format: OutputFormat::Multicast { 
                address: multicast_addr.to_string(), 
                port: multicast_port 
            },
            realtime: true,
            hardware_acceleration: true,
        };

        let transcoding_job_id = self.transcoder_manager
            .start_transcoding(transcoding_request).await?;

        // Create active stream info
        let active_stream = ActiveStreamInfo {
            stream_id: stream.id,
            stream_key: stream.stream_key.clone(),
            user_id: stream.user_id,
            title: stream.title.clone(),
            multicast_address: multicast_addr.to_string(),
            multicast_port,
            transcoding_job_id: Some(transcoding_job_id),
            started_at: chrono::Utc::now(),
            last_frame_at: chrono::Utc::now(),
            viewer_count: 0,
            total_viewers: 0,
            bitrate_kbps: 0,
            frame_rate: 0.0,
            resolution: (0, 0),
            is_recording: stream.recording_enabled,
            quality_profiles: stream.transcoding_profiles.clone(),
        };

        // Add to active streams
        {
            let mut streams = self.active_streams.write().await;
            streams.insert(stream_id, active_stream);
        }

        // Initialize viewer tracking
        {
            let mut tracking = self.viewer_tracking.write().await;
            tracking.insert(stream_id, ViewerTracking {
                stream_id,
                viewers: HashMap::new(),
                total_join_count: 0,
                peak_concurrent: 0,
            });
        }

        // Create stream session
        let session_id = Uuid::new_v4();
        let session = StreamSessionInfo {
            session_id,
            stream_id,
            started_at: chrono::Utc::now(),
            viewers: HashMap::new(),
            metrics: StreamMetricsData {
                frames_processed: 0,
                bytes_transmitted: 0,
                dropped_frames: 0,
                encoding_bitrate: 0,
                network_throughput: 0,
                cpu_usage: 0.0,
                memory_usage: 0,
            },
        };

        {
            let mut sessions = self.stream_sessions.write().await;
            sessions.insert(session_id, session);
        }

        // Start HLS stream
        self.hls_server.start_stream(&stream.stream_key, multicast_addr, multicast_port).await?;

        // Start WebRTC stream
        self.webrtc_server.start_stream(&stream.stream_key, multicast_addr, multicast_port).await?;

        info!("Started stream '{}' (ID: {}) at {}:{}", 
              stream.title, stream_id, multicast_addr, multicast_port);

        Ok(())
    }

    pub async fn stop_stream(&self, stream_id: Uuid) -> Result<()> {
        let active_stream = {
            let mut streams = self.active_streams.write().await;
            streams.remove(&stream_id)
        };

        if let Some(stream_info) = active_stream {
            // Update database
            let mut stream = Stream::find_by_id(&self.db_pool, stream_id).await?
                .ok_or_else(|| anyhow::anyhow!("Stream not found"))?;
            
            stream.stop_streaming(&self.db_pool).await?;

            // Stop transcoding
            if let Some(job_id) = stream_info.transcoding_job_id {
                self.transcoder_manager.stop_transcoding(job_id).await?;
            }

            // Remove multicast group
            self.multicast_manager.remove_group(&stream_info.stream_key).await?;

            // Stop HLS stream
            self.hls_server.stop_stream(&stream_info.stream_key).await?;

            // Stop WebRTC stream
            self.webrtc_server.stop_stream(&stream_info.stream_key).await?;

            // Clean up viewer tracking
            {
                let mut tracking = self.viewer_tracking.write().await;
                tracking.remove(&stream_id);
            }

            // Finalize stream session
            self.finalize_stream_session(stream_id).await?;

            info!("Stopped stream '{}' (ID: {})", stream_info.title, stream_id);
        }

        Ok(())
    }

    pub async fn handle_media_data(&self, stream_key: &str, data: Bytes, payload_type: PayloadType) -> Result<()> {
        // Find active stream
        let stream_info = {
            let streams = self.active_streams.read().await;
            streams.values().find(|s| s.stream_key == stream_key).cloned()
        };

        if let Some(mut stream_info) = stream_info {
            // Update last frame time
            stream_info.last_frame_at = chrono::Utc::now();
            
            // Create multicast packet
            let packet = MulticastPacket {
                stream_key: stream_key.to_string(),
                sequence_number: self.get_next_sequence_number().await,
                timestamp: chrono::Utc::now().timestamp() as u32,
                payload_type,
                data: data.clone(),
            };

            // Send to multicast
            self.multicast_manager.send_packet(packet).await?;

            // Update stream metrics
            self.update_stream_metrics(stream_info.stream_id, data.len() as u64).await?;

            // Update active stream info
            {
                let mut streams = self.active_streams.write().await;
                if let Some(active_stream) = streams.get_mut(&stream_info.stream_id) {
                    active_stream.last_frame_at = stream_info.last_frame_at;
                }
            }
        }

        Ok(())
    }

    pub async fn add_viewer(&self, stream_id: Uuid, viewer_info: ViewerInfo) -> Result<Uuid> {
        let stream_key = {
            let streams = self.active_streams.read().await;
            streams.get(&stream_id).map(|s| s.stream_key.clone())
        };

        if let Some(stream_key) = stream_key {
            // Add to multicast group
            let viewer_addr = SocketAddr::new(
                viewer_info.ip_address.parse()?,
                8000 // Default port
            );

            let viewer_id = self.multicast_manager.add_viewer(
                &stream_key, 
                viewer_addr, 
                viewer_info.quality_profile.clone()
            ).await?;

            // Add to viewer tracking
            {
                let mut tracking = self.viewer_tracking.write().await;
                if let Some(stream_tracking) = tracking.get_mut(&stream_id) {
                    stream_tracking.viewers.insert(viewer_addr, ViewerSession {
                        viewer_id,
                        joined_at: chrono::Utc::now(),
                        last_activity: chrono::Utc::now(),
                        bytes_received: 0,
                        quality_profile: viewer_info.quality_profile.clone(),
                        connection_quality: ConnectionQuality {
                            latency_ms: 0,
                            packet_loss: 0.0,
                            jitter_ms: 0,
                            bandwidth_kbps: 0,
                        },
                    });
                    
                    stream_tracking.total_join_count += 1;
                    if stream_tracking.viewers.len() > stream_tracking.peak_concurrent {
                        stream_tracking.peak_concurrent = stream_tracking.viewers.len();
                    }
                }
            }

            // Update viewer count in stream
            self.update_viewer_count(stream_id).await?;

            info!("Added viewer {} to stream {}", viewer_id, stream_id);
            Ok(viewer_id)
        } else {
            Err(anyhow::anyhow!("Stream not found"))
        }
    }

    pub async fn remove_viewer(&self, stream_id: Uuid, viewer_addr: SocketAddr) -> Result<()> {
        let stream_key = {
            let streams = self.active_streams.read().await;
            streams.get(&stream_id).map(|s| s.stream_key.clone())
        };

        if let Some(stream_key) = stream_key {
            // Remove from multicast group
            self.multicast_manager.remove_viewer(&stream_key, viewer_addr).await?;

            // Remove from viewer tracking
            {
                let mut tracking = self.viewer_tracking.write().await;
                if let Some(stream_tracking) = tracking.get_mut(&stream_id) {
                    stream_tracking.viewers.remove(&viewer_addr);
                }
            }

            // Update viewer count
            self.update_viewer_count(stream_id).await?;

            debug!("Removed viewer {} from stream {}", viewer_addr, stream_id);
        }

        Ok(())
    }

    pub async fn get_stream_url(&self, stream_id: Uuid, connection_type: ConnectionType) -> Result<String> {
        let stream_info = {
            let streams = self.active_streams.read().await;
            streams.get(&stream_id).cloned()
        };

        if let Some(stream_info) = stream_info {
            match connection_type {
                ConnectionType::Multicast => {
                    Ok(format!("udp://{}:{}", stream_info.multicast_address, stream_info.multicast_port))
                },
                ConnectionType::WebRtc => {
                    self.webrtc_server.get_stream_url(&stream_info.stream_key).await
                },
                ConnectionType::Hls => {
                    self.hls_server.get_stream_url(&stream_info.stream_key).await
                },
            }
        } else {
            Err(anyhow::anyhow!("Stream not found or not live"))
        }
    }

    pub async fn get_active_streams(&self) -> Vec<ActiveStreamInfo> {
        let streams = self.active_streams.read().await;
        streams.values().cloned().collect()
    }

    pub async fn get_stream_info(&self, stream_id: Uuid) -> Option<ActiveStreamInfo> {
        let streams = self.active_streams.read().await;
        streams.get(&stream_id).cloned()
    }

    async fn update_viewer_count(&self, stream_id: Uuid) -> Result<()> {
        let viewer_count = {
            let tracking = self.viewer_tracking.read().await;
            tracking.get(&stream_id).map(|t| t.viewers.len()).unwrap_or(0)
        };

        // Update in memory
        {
            let mut streams = self.active_streams.write().await;
            if let Some(stream) = streams.get_mut(&stream_id) {
                stream.viewer_count = viewer_count;
            }
        }

        // Update in database
        let mut stream = Stream::find_by_id(&self.db_pool, stream_id).await?
            .ok_or_else(|| anyhow::anyhow!("Stream not found"))?;
        
        stream.update_viewer_count(&self.db_pool, viewer_count as i32).await?;

        Ok(())
    }

    async fn update_stream_metrics(&self, stream_id: Uuid, bytes_transmitted: u64) -> Result<()> {
        // Update in-memory metrics
        {
            let mut sessions = self.stream_sessions.write().await;
            for session in sessions.values_mut() {
                if session.stream_id == stream_id {
                    session.metrics.bytes_transmitted += bytes_transmitted;
                    session.metrics.frames_processed += 1;
                }
            }
        }

        Ok(())
    }

    async fn finalize_stream_session(&self, stream_id: Uuid) -> Result<()> {
        let session = {
            let mut sessions = self.stream_sessions.write().await;
            sessions.values().find(|s| s.stream_id == stream_id).cloned()
        };

        if let Some(session) = session {
            let duration = chrono::Utc::now().signed_duration_since(session.started_at);
            
            // Save session to database
            sqlx::query(
                r#"
                INSERT INTO stream_sessions (id, stream_id, started_at, ended_at, duration_seconds,
                                           peak_viewers, total_viewers, bytes_transmitted, quality_metrics)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                "#
            )
            .bind(session.session_id)
            .bind(stream_id)
            .bind(session.started_at)
            .bind(chrono::Utc::now())
            .bind(duration.num_seconds() as i32)
            .bind(0) // Would calculate from viewer tracking
            .bind(session.viewers.len() as i32)
            .bind(session.metrics.bytes_transmitted as i64)
            .bind(serde_json::json!(session.metrics))
            .execute(&self.db_pool)
            .await?;
        }

        Ok(())
    }

    async fn get_next_sequence_number(&self) -> u32 {
        // Simple implementation - in production, maintain per-stream sequence numbers
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQUENCE_NUMBER: AtomicU32 = AtomicU32::new(0);
        SEQUENCE_NUMBER.fetch_add(1, Ordering::Relaxed)
    }

    async fn stream_health_monitor(
        active_streams: Arc<RwLock<HashMap<Uuid, ActiveStreamInfo>>>,
        db_pool: PgPool,
    ) {
        let mut interval = interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            let now = chrono::Utc::now();
            let mut unhealthy_streams = Vec::new();

            {
                let streams = active_streams.read().await;
                for (stream_id, stream_info) in streams.iter() {
                    // Check if stream hasn't received frames in a while
                    let time_since_last_frame = now.signed_duration_since(stream_info.last_frame_at);
                    if time_since_last_frame.num_seconds() > 60 {
                        unhealthy_streams.push(*stream_id);
                    }
                }
            }

            for stream_id in unhealthy_streams {
                warn!("Stream {} appears unhealthy, marking as offline", stream_id);
                
                // Mark stream as offline in database
                if let Ok(Some(mut stream)) = Stream::find_by_id(&db_pool, stream_id).await {
                    let _ = stream.stop_streaming(&db_pool).await;
                }
            }
        }
    }

    async fn viewer_cleanup_task(viewer_tracking: Arc<RwLock<HashMap<Uuid, ViewerTracking>>>) {
        let mut interval = interval(Duration::from_secs(60));

        loop {
            interval.tick().await;

            let now = chrono::Utc::now();
            
            {
                let mut tracking = viewer_tracking.write().await;
                for stream_tracking in tracking.values_mut() {
                    let mut inactive_viewers = Vec::new();
                    
                    for (addr, viewer) in &stream_tracking.viewers {
                        let time_since_activity = now.signed_duration_since(viewer.last_activity);
                        if time_since_activity.num_seconds() > 300 { // 5 minutes
                            inactive_viewers.push(*addr);
                        }
                    }

                    for addr in inactive_viewers {
                        stream_tracking.viewers.remove(&addr);
                        debug!("Cleaned up inactive viewer: {}", addr);
                    }
                }
            }
        }
    }
}

impl MetricsCollector {
    async fn start_collection(&self) {
        let mut interval = interval(Duration::from_secs(10));

        loop {
            interval.tick().await;

            let streams = self.active_streams.read().await;
            for stream_info in streams.values() {
                if let Err(e) = self.collect_stream_metrics(stream_info).await {
                    error!("Failed to collect metrics for stream {}: {}", stream_info.stream_id, e);
                }
            }
        }
    }

    async fn collect_stream_metrics(&self, stream_info: &ActiveStreamInfo) -> Result<()> {
        // Collect system metrics
        let cpu_usage = Self::get_cpu_usage().await;
        let memory_usage = Self::get_memory_usage().await;

        // Record metrics in database
        StreamMetrics::record(
            &self.db_pool,
            stream_info.stream_id,
            stream_info.viewer_count as i32,
            stream_info.bitrate_kbps as i32,
            stream_info.frame_rate,
            0, // dropped_frames - would need to track this
            0, // network_throughput - would need to measure this
            cpu_usage,
            memory_usage,
        ).await?;

        Ok(())
    }

    async fn get_cpu_usage() -> f32 {
        // Simplified CPU usage - in production, use proper system monitoring
        0.0
    }

    async fn get_memory_usage() -> i64 {
        // Simplified memory usage - in production, use proper system monitoring
        0
    }
}