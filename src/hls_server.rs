use anyhow::{Result, Context};
use bytes::Bytes;
use m3u8_rs::{Playlist, MediaPlaylist, MediaSegment};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock, Mutex};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::HlsConfig;

pub struct HlsServer {
    config: HlsConfig,
    active_streams: Arc<RwLock<HashMap<String, HlsStream>>>,
    segment_writers: Arc<RwLock<HashMap<String, Arc<Mutex<SegmentWriter>>>>>,
}

#[derive(Debug, Clone)]
pub struct HlsStream {
    pub stream_key: String,
    pub multicast_address: Ipv4Addr,
    pub multicast_port: u16,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub segments: Vec<HlsSegment>,
    pub current_sequence: u64,
    pub target_duration: f32,
    pub playlist_path: PathBuf,
    pub segments_dir: PathBuf,
    pub viewer_count: usize,
    pub total_duration: f32,
}

#[derive(Debug, Clone)]
pub struct HlsSegment {
    pub sequence_number: u64,
    pub duration: f32,
    pub filename: String,
    pub file_path: PathBuf,
    pub size_bytes: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub is_complete: bool,
}

pub struct SegmentWriter {
    stream_key: String,
    current_segment: Option<HlsSegment>,
    segment_file: Option<BufWriter<File>>,
    segment_start_time: Option<SystemTime>,
    bytes_written: u64,
    config: HlsConfig,
    active_streams: Arc<RwLock<HashMap<String, HlsStream>>>,
}

#[derive(Debug, Clone)]
pub struct HlsViewerSession {
    pub id: Uuid,
    pub stream_key: String,
    pub ip_address: String,
    pub user_agent: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_request: chrono::DateTime<chrono::Utc>,
    pub segments_downloaded: u64,
    pub bytes_downloaded: u64,
}

impl HlsServer {
    pub async fn new(config: HlsConfig) -> Result<Self> {
        // Create segment directory
        fs::create_dir_all(&config.segment_directory).await
            .context("Failed to create HLS segment directory")?;

        Ok(Self {
            config,
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            segment_writers: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting HLS server");

        // Start background tasks
        self.start_background_tasks().await?;

        info!("HLS server started successfully");
        Ok(())
    }

    async fn start_background_tasks(&self) -> Result<()> {
        // Cleanup task for old segments
        let config = self.config.clone();
        let active_streams = self.active_streams.clone();
        tokio::spawn(async move {
            Self::cleanup_task(config, active_streams).await;
        });

        // Playlist update task
        let active_streams = self.active_streams.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            Self::playlist_update_task(active_streams, config).await;
        });

        Ok(())
    }

    pub async fn start_stream(&self, stream_key: &str, multicast_address: Ipv4Addr, multicast_port: u16) -> Result<()> {
        let stream_dir = self.config.segment_directory.join(stream_key);
        fs::create_dir_all(&stream_dir).await?;

        let playlist_path = stream_dir.join("playlist.m3u8");
        let segments_dir = stream_dir.clone();

        let hls_stream = HlsStream {
            stream_key: stream_key.to_string(),
            multicast_address,
            multicast_port,
            started_at: chrono::Utc::now(),
            segments: Vec::new(),
            current_sequence: 0,
            target_duration: self.config.segment_duration as f32,
            playlist_path,
            segments_dir,
            viewer_count: 0,
            total_duration: 0.0,
        };

        // Add to active streams
        {
            let mut streams = self.active_streams.write().await;
            streams.insert(stream_key.to_string(), hls_stream);
        }

        // Create segment writer
        let segment_writer = Arc::new(Mutex::new(SegmentWriter::new(
            stream_key.to_string(),
            self.config.clone(),
            self.active_streams.clone(),
        )));

        {
            let mut writers = self.segment_writers.write().await;
            writers.insert(stream_key.to_string(), segment_writer);
        }

        // Start multicast receiver
        self.start_multicast_receiver(stream_key, multicast_address, multicast_port).await?;

        // Create initial playlist
        self.update_playlist(stream_key).await?;

        info!("Started HLS stream for key '{}' receiving from {}:{}", 
              stream_key, multicast_address, multicast_port);

        Ok(())
    }

    pub async fn stop_stream(&self, stream_key: &str) -> Result<()> {
        // Finalize current segment
        if let Some(writer) = {
            let writers = self.segment_writers.read().await;
            writers.get(stream_key).cloned()
        } {
            let mut writer = writer.lock().await;
            writer.finalize_current_segment().await?;
        }

        // Remove from active streams
        {
            let mut streams = self.active_streams.write().await;
            streams.remove(stream_key);
        }

        // Remove segment writer
        {
            let mut writers = self.segment_writers.write().await;
            writers.remove(stream_key);
        }

        // Update final playlist
        self.update_playlist(stream_key).await?;

        info!("Stopped HLS stream for key '{}'", stream_key);
        Ok(())
    }

    async fn start_multicast_receiver(&self, stream_key: &str, multicast_address: Ipv4Addr, multicast_port: u16) -> Result<()> {
        let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), multicast_port);
        let socket = UdpSocket::bind(socket_addr).await?;

        // Join multicast group
        socket.join_multicast_v4(multicast_address, Ipv4Addr::UNSPECIFIED)?;

        let stream_key = stream_key.to_string();
        let segment_writers = self.segment_writers.clone();

        tokio::spawn(async move {
            let mut buffer = [0u8; 1500];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((size, _)) => {
                        if size < 16 {
                            continue; // Skip packets that are too small
                        }

                        // Parse custom packet header
                        let _sequence_number = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                        let _timestamp = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
                        let payload_type = buffer[8];
                        
                        let payload = &buffer[16..size];

                        // Write to appropriate segment
                        if let Some(writer) = {
                            let writers = segment_writers.read().await;
                            writers.get(&stream_key).cloned()
                        } {
                            let mut writer = writer.lock().await;
                            if let Err(e) = writer.write_data(payload, payload_type).await {
                                error!("Failed to write HLS data: {}", e);
                            }
                        }
                    },
                    Err(e) => {
                        error!("Error receiving multicast data for HLS: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn get_playlist(&self, stream_key: &str) -> Result<String> {
        let streams = self.active_streams.read().await;
        if let Some(stream) = streams.get(stream_key) {
            if stream.playlist_path.exists() {
                Ok(fs::read_to_string(&stream.playlist_path).await?)
            } else {
                Err(anyhow::anyhow!("Playlist not found"))
            }
        } else {
            Err(anyhow::anyhow!("Stream not found"))
        }
    }

    pub async fn get_segment(&self, stream_key: &str, segment_name: &str) -> Result<Bytes> {
        let streams = self.active_streams.read().await;
        if let Some(stream) = streams.get(stream_key) {
            let segment_path = stream.segments_dir.join(segment_name);
            if segment_path.exists() {
                Ok(Bytes::from(fs::read(segment_path).await?))
            } else {
                Err(anyhow::anyhow!("Segment not found"))
            }
        } else {
            Err(anyhow::anyhow!("Stream not found"))
        }
    }

    pub async fn get_stream_url(&self, stream_key: &str) -> Result<String> {
        let streams = self.active_streams.read().await;
        if streams.contains_key(stream_key) {
            Ok(format!("/api/v1/hls/{}/playlist.m3u8", stream_key))
        } else {
            Err(anyhow::anyhow!("Stream not found"))
        }
    }

    async fn update_playlist(&self, stream_key: &str) -> Result<()> {
        let stream = {
            let streams = self.active_streams.read().await;
            streams.get(stream_key).cloned()
        };

        if let Some(stream) = stream {
            let mut playlist = MediaPlaylist::new();
            playlist.target_duration = stream.target_duration;
            playlist.version = Some(3);
            playlist.sequence = stream.current_sequence.saturating_sub(stream.segments.len() as u64);

            // Add segments to playlist
            for segment in &stream.segments {
                if segment.is_complete {
                    let mut media_segment = MediaSegment::new(&segment.filename);
                    media_segment.duration = segment.duration;
                    playlist.segments.push(media_segment);
                }
            }

            // Write playlist to file
            let playlist_content = playlist.to_string();
            fs::write(&stream.playlist_path, playlist_content).await?;

            debug!("Updated HLS playlist for stream '{}' with {} segments", 
                   stream_key, stream.segments.len());
        }

        Ok(())
    }

    async fn cleanup_task(config: HlsConfig, active_streams: Arc<RwLock<HashMap<String, HlsStream>>>) {
        let mut interval = interval(Duration::from_secs(config.cleanup_interval));

        loop {
            interval.tick().await;

            let mut streams = active_streams.write().await;
            for stream in streams.values_mut() {
                let now = chrono::Utc::now();
                let max_segments = config.playlist_length as usize;

                // Remove old segments
                if stream.segments.len() > max_segments {
                    let segments_to_remove = stream.segments.len() - max_segments;
                    let removed_segments: Vec<_> = stream.segments.drain(0..segments_to_remove).collect();

                    for segment in removed_segments {
                        if let Err(e) = fs::remove_file(&segment.file_path).await {
                            warn!("Failed to remove old segment {}: {}", segment.file_path.display(), e);
                        }
                    }
                }

                // Remove segments older than cleanup interval
                stream.segments.retain(|segment| {
                    let age = now.signed_duration_since(segment.created_at);
                    if age.num_seconds() > (config.cleanup_interval * 2) as i64 {
                        let _ = std::fs::remove_file(&segment.file_path);
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    async fn playlist_update_task(active_streams: Arc<RwLock<HashMap<String, HlsStream>>>, config: HlsConfig) {
        let mut interval = interval(Duration::from_secs(config.segment_duration as u64 / 4));

        loop {
            interval.tick().await;

            let stream_keys: Vec<String> = {
                let streams = active_streams.read().await;
                streams.keys().cloned().collect()
            };

            for stream_key in stream_keys {
                if let Err(e) = Self::update_playlist_for_stream(&active_streams, &stream_key).await {
                    error!("Failed to update playlist for stream '{}': {}", stream_key, e);
                }
            }
        }
    }

    async fn update_playlist_for_stream(
        active_streams: &Arc<RwLock<HashMap<String, HlsStream>>>,
        stream_key: &str
    ) -> Result<()> {
        let stream = {
            let streams = active_streams.read().await;
            streams.get(stream_key).cloned()
        };

        if let Some(stream) = stream {
            let mut playlist = MediaPlaylist::new();
            playlist.target_duration = stream.target_duration;
            playlist.version = Some(3);
            playlist.sequence = stream.current_sequence.saturating_sub(stream.segments.len() as u64);

            for segment in &stream.segments {
                if segment.is_complete {
                    let mut media_segment = MediaSegment::new(&segment.filename);
                    media_segment.duration = segment.duration;
                    playlist.segments.push(media_segment);
                }
            }

            let playlist_content = playlist.to_string();
            fs::write(&stream.playlist_path, playlist_content).await?;
        }

        Ok(())
    }

    pub async fn get_stream_stats(&self, stream_key: &str) -> Option<HlsStreamStats> {
        let streams = self.active_streams.read().await;
        if let Some(stream) = streams.get(stream_key) {
            Some(HlsStreamStats {
                stream_key: stream_key.to_string(),
                segment_count: stream.segments.len(),
                total_duration: stream.total_duration,
                current_sequence: stream.current_sequence,
                viewer_count: stream.viewer_count,
                uptime_seconds: chrono::Utc::now()
                    .signed_duration_since(stream.started_at)
                    .num_seconds() as u64,
            })
        } else {
            None
        }
    }

    pub async fn track_viewer(&self, stream_key: &str, session: HlsViewerSession) -> Result<()> {
        // Update viewer count for the stream
        let mut streams = self.active_streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            // This is simplified - in production, maintain a proper viewer registry
            stream.viewer_count += 1;
        }
        Ok(())
    }
}

impl SegmentWriter {
    fn new(
        stream_key: String,
        config: HlsConfig,
        active_streams: Arc<RwLock<HashMap<String, HlsStream>>>,
    ) -> Self {
        Self {
            stream_key,
            current_segment: None,
            segment_file: None,
            segment_start_time: None,
            bytes_written: 0,
            config,
            active_streams,
        }
    }

    async fn write_data(&mut self, data: &[u8], payload_type: u8) -> Result<()> {
        // Start new segment if needed
        if self.current_segment.is_none() {
            self.start_new_segment().await?;
        }

        // Check if segment duration exceeded
        if let Some(start_time) = self.segment_start_time {
            let elapsed = start_time.elapsed().unwrap_or_default();
            if elapsed >= Duration::from_secs(self.config.segment_duration as u64) {
                self.finalize_current_segment().await?;
                self.start_new_segment().await?;
            }
        }

        // Write data to current segment
        if let Some(ref mut file) = self.segment_file {
            // For simplicity, we'll write raw TS packets
            // In production, this should properly mux into MPEG-TS format
            file.write_all(data).await?;
            self.bytes_written += data.len() as u64;
        }

        Ok(())
    }

    async fn start_new_segment(&mut self) -> Result<()> {
        let sequence_number = {
            let streams = self.active_streams.read().await;
            streams.get(&self.stream_key)
                .map(|s| s.current_sequence)
                .unwrap_or(0)
        };

        let filename = format!("segment_{:06}.ts", sequence_number);
        let file_path = {
            let streams = self.active_streams.read().await;
            streams.get(&self.stream_key)
                .map(|s| s.segments_dir.join(&filename))
                .ok_or_else(|| anyhow::anyhow!("Stream not found"))?
        };

        let file = File::create(&file_path).await?;
        let buffered_file = BufWriter::new(file);

        let segment = HlsSegment {
            sequence_number,
            duration: 0.0,
            filename,
            file_path,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            is_complete: false,
        };

        self.current_segment = Some(segment);
        self.segment_file = Some(buffered_file);
        self.segment_start_time = Some(SystemTime::now());
        self.bytes_written = 0;

        debug!("Started new HLS segment {} for stream '{}'", sequence_number, self.stream_key);
        Ok(())
    }

    async fn finalize_current_segment(&mut self) -> Result<()> {
        if let Some(mut segment) = self.current_segment.take() {
            // Flush and close file
            if let Some(mut file) = self.segment_file.take() {
                file.flush().await?;
                drop(file);
            }

            // Calculate actual duration
            if let Some(start_time) = self.segment_start_time.take() {
                let elapsed = start_time.elapsed().unwrap_or_default();
                segment.duration = elapsed.as_secs_f32();
            }

            segment.size_bytes = self.bytes_written;
            segment.is_complete = true;

            // Add to stream segments
            {
                let mut streams = self.active_streams.write().await;
                if let Some(stream) = streams.get_mut(&self.stream_key) {
                    stream.segments.push(segment.clone());
                    stream.current_sequence += 1;
                    stream.total_duration += segment.duration;
                }
            }

            info!("Finalized HLS segment {} for stream '{}' (duration: {:.2}s, size: {} bytes)", 
                  segment.sequence_number, self.stream_key, segment.duration, segment.size_bytes);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HlsStreamStats {
    pub stream_key: String,
    pub segment_count: usize,
    pub total_duration: f32,
    pub current_sequence: u64,
    pub viewer_count: usize,
    pub uptime_seconds: u64,
}