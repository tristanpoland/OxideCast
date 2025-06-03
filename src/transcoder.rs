use anyhow::{Result, Context};
use bytes::Bytes;
use futures::stream::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{TranscodingConfig, EncodingProfile, VideoCodec, AudioCodec};

pub struct TranscoderManager {
    config: TranscodingConfig,
    active_jobs: Arc<RwLock<HashMap<Uuid, TranscodingJob>>>,
    job_semaphore: Arc<Semaphore>,
    profiles_map: HashMap<String, EncodingProfile>,
    video_codecs_map: HashMap<String, VideoCodec>,
    audio_codecs_map: HashMap<String, AudioCodec>,
}

#[derive(Debug, Clone)]
pub struct TranscodingJob {
    pub id: Uuid,
    pub stream_key: String,
    pub input_source: InputSource,
    pub output_profiles: Vec<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub status: JobStatus,
    pub progress: TranscodingProgress,
    pub child_process: Option<u32>, // PID for process management
    pub output_paths: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone)]
pub enum InputSource {
    Rtmp { stream_key: String },
    File { path: PathBuf },
    Network { url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Pending,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TranscodingProgress {
    pub frames_processed: u64,
    pub duration_processed: Duration,
    pub bitrate_kbps: u32,
    pub fps: f32,
    pub quality_score: f32,
    pub cpu_usage: f32,
    pub memory_usage: u64,
}

#[derive(Debug, Clone)]
pub struct TranscodingRequest {
    pub stream_key: String,
    pub input_source: InputSource,
    pub profiles: Vec<String>,
    pub output_format: OutputFormat,
    pub realtime: bool,
    pub hardware_acceleration: bool,
}

#[derive(Debug, Clone)]
pub enum OutputFormat {
    Hls { segment_duration: u32 },
    Dash { segment_duration: u32 },
    Rtmp { target_url: String },
    Multicast { address: String, port: u16 },
    File { container: String },
}

pub struct TranscodingOutput {
    pub profile: String,
    pub format: OutputFormat,
    pub data_sender: mpsc::Sender<Bytes>,
}

impl TranscoderManager {
    pub async fn new(config: TranscodingConfig) -> Result<Self> {
        // Validate FFmpeg installation
        Self::validate_ffmpeg(&config.ffmpeg_path).await?;

        // Create temp directory
        fs::create_dir_all(&config.temp_dir).await
            .context("Failed to create temp directory")?;

        // Build lookup maps for quick access
        let profiles_map = config.profiles.iter()
            .map(|p| (p.name.clone(), p.clone()))
            .collect();

        let video_codecs_map = config.video_codecs.iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();

        let audio_codecs_map = config.audio_codecs.iter()
            .map(|c| (c.name.clone(), c.clone()))
            .collect();

        Ok(Self {
            config,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            job_semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            profiles_map,
            video_codecs_map,
            audio_codecs_map,
        })
    }

    pub async fn start_transcoding(&self, request: TranscodingRequest) -> Result<Uuid> {
        let job_id = Uuid::new_v4();
        
        // Validate profiles
        for profile_name in &request.profiles {
            if !self.profiles_map.contains_key(profile_name) {
                return Err(anyhow::anyhow!("Unknown profile: {}", profile_name));
            }
        }

        // Create job
        let job = TranscodingJob {
            id: job_id,
            stream_key: request.stream_key.clone(),
            input_source: request.input_source.clone(),
            output_profiles: request.profiles.clone(),
            started_at: chrono::Utc::now(),
            status: JobStatus::Pending,
            progress: TranscodingProgress {
                frames_processed: 0,
                duration_processed: Duration::from_secs(0),
                bitrate_kbps: 0,
                fps: 0.0,
                quality_score: 0.0,
                cpu_usage: 0.0,
                memory_usage: 0,
            },
            child_process: None,
            output_paths: HashMap::new(),
        };

        // Add to active jobs
        {
            let mut jobs = self.active_jobs.write().await;
            jobs.insert(job_id, job);
        }

        // Start transcoding task
        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(e) = manager.run_transcoding_job(job_id, request).await {
                error!("Transcoding job {} failed: {}", job_id, e);
                manager.update_job_status(job_id, JobStatus::Failed).await;
            }
        });

        info!("Started transcoding job {} for stream '{}'", job_id, request.stream_key);
        Ok(job_id)
    }

    pub async fn stop_transcoding(&self, job_id: Uuid) -> Result<()> {
        let pid = {
            let mut jobs = self.active_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = JobStatus::Cancelled;
                job.child_process
            } else {
                return Err(anyhow::anyhow!("Job not found: {}", job_id));
            }
        };

        if let Some(pid) = pid {
            // Kill the FFmpeg process
            if let Err(e) = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM
            ) {
                warn!("Failed to terminate transcoding process {}: {}", pid, e);
            }
        }

        info!("Stopped transcoding job {}", job_id);
        Ok(())
    }

    async fn run_transcoding_job(&self, job_id: Uuid, request: TranscodingRequest) -> Result<()> {
        // Acquire semaphore permit
        let _permit = self.job_semaphore.acquire().await?;

        self.update_job_status(job_id, JobStatus::Starting).await;

        // Generate FFmpeg command
        let ffmpeg_args = self.build_ffmpeg_command(&request).await?;
        
        // Start FFmpeg process
        let mut child = Command::new(&self.config.ffmpeg_path)
            .args(&ffmpeg_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start FFmpeg process")?;

        let pid = child.id().ok_or_else(|| anyhow::anyhow!("Failed to get process ID"))?;

        // Update job with process ID
        {
            let mut jobs = self.active_jobs.write().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.child_process = Some(pid);
                job.status = JobStatus::Running;
            }
        }

        info!("Started FFmpeg process {} for job {}", pid, job_id);

        // Monitor transcoding progress
        let progress_monitor = self.monitor_transcoding_progress(job_id, &mut child);
        let input_feeder = self.feed_input_data(job_id, &request, &mut child);

        // Wait for completion
        tokio::select! {
            result = child.wait() => {
                match result {
                    Ok(status) => {
                        if status.success() {
                            self.update_job_status(job_id, JobStatus::Completed).await;
                            info!("Transcoding job {} completed successfully", job_id);
                        } else {
                            self.update_job_status(job_id, JobStatus::Failed).await;
                            error!("Transcoding job {} failed with exit code: {:?}", job_id, status.code());
                        }
                    },
                    Err(e) => {
                        self.update_job_status(job_id, JobStatus::Failed).await;
                        error!("Transcoding job {} failed: {}", job_id, e);
                    }
                }
            },
            _ = progress_monitor => {
                debug!("Progress monitor ended for job {}", job_id);
            },
            _ = input_feeder => {
                debug!("Input feeder ended for job {}", job_id);
            }
        }

        // Cleanup
        self.cleanup_job(job_id).await?;

        Ok(())
    }

    async fn build_ffmpeg_command(&self, request: &TranscodingRequest) -> Result<Vec<String>> {
        let mut args = Vec::new();

        // Global options
        args.extend_from_slice(&["-hide_banner".to_string(), "-loglevel".to_string(), "info".to_string()]);

        // Hardware acceleration
        if request.hardware_acceleration && self.config.hardware_acceleration.enabled {
            if let Some(ref device) = self.config.hardware_acceleration.device {
                args.extend_from_slice(&["-hwaccel".to_string(), device.clone()]);
            }
            if let Some(ref codec) = self.config.hardware_acceleration.codec {
                args.extend_from_slice(&["-hwaccel_device".to_string(), codec.clone()]);
            }
        }

        // Input configuration
        match &request.input_source {
            InputSource::Rtmp { stream_key } => {
                args.extend_from_slice(&[
                    "-f".to_string(), "flv".to_string(),
                    "-i".to_string(), format!("rtmp://localhost:1935/live/{}", stream_key)
                ]);
            },
            InputSource::File { path } => {
                args.extend_from_slice(&["-i".to_string(), path.to_string_lossy().to_string()]);
            },
            InputSource::Network { url } => {
                args.extend_from_slice(&["-i".to_string(), url.clone()]);
            }
        }

        // Realtime processing
        if request.realtime {
            args.extend_from_slice(&["-re".to_string()]);
        }

        // Output configurations for each profile
        for profile_name in &request.profiles {
            if let Some(profile) = self.profiles_map.get(profile_name) {
                self.add_profile_args(&mut args, profile, &request.output_format).await?;
            }
        }

        debug!("FFmpeg command: {} {}", self.config.ffmpeg_path.display(), args.join(" "));
        Ok(args)
    }

    async fn add_profile_args(&self, args: &mut Vec<String>, profile: &EncodingProfile, output_format: &OutputFormat) -> Result<()> {
        // Video encoding
        if let Some(video_codec) = self.video_codecs_map.get("h264") {
            args.extend_from_slice(&[
                "-c:v".to_string(), video_codec.encoder.clone(),
                "-b:v".to_string(), format!("{}k", profile.bitrate / 1000),
                "-s".to_string(), format!("{}x{}", profile.width, profile.height),
                "-r".to_string(), profile.framerate.to_string(),
                "-g".to_string(), profile.keyframe_interval.to_string(),
            ]);

            // Add codec-specific options
            for (key, value) in &video_codec.options {
                args.extend_from_slice(&[format!("-{}", key), value.clone()]);
            }
        }

        // Audio encoding
        if let Some(audio_codec) = self.audio_codecs_map.get("aac") {
            args.extend_from_slice(&[
                "-c:a".to_string(), audio_codec.encoder.clone(),
                "-b:a".to_string(), format!("{}k", profile.audio_bitrate / 1000),
                "-ar".to_string(), audio_codec.sample_rate.to_string(),
                "-ac".to_string(), audio_codec.channels.to_string(),
            ]);
        }

        // Output format specific options
        match output_format {
            OutputFormat::Hls { segment_duration } => {
                let output_dir = self.config.temp_dir.join(format!("hls_{}", profile.name));
                fs::create_dir_all(&output_dir).await?;
                
                args.extend_from_slice(&[
                    "-f".to_string(), "hls".to_string(),
                    "-hls_time".to_string(), segment_duration.to_string(),
                    "-hls_playlist_type".to_string(), "event".to_string(),
                    "-hls_flags".to_string(), "delete_segments".to_string(),
                    output_dir.join("playlist.m3u8").to_string_lossy().to_string(),
                ]);
            },
            OutputFormat::Dash { segment_duration } => {
                let output_dir = self.config.temp_dir.join(format!("dash_{}", profile.name));
                fs::create_dir_all(&output_dir).await?;
                
                args.extend_from_slice(&[
                    "-f".to_string(), "dash".to_string(),
                    "-seg_duration".to_string(), segment_duration.to_string(),
                    output_dir.join("manifest.mpd").to_string_lossy().to_string(),
                ]);
            },
            OutputFormat::Rtmp { target_url } => {
                args.extend_from_slice(&[
                    "-f".to_string(), "flv".to_string(),
                    target_url.clone(),
                ]);
            },
            OutputFormat::Multicast { address, port } => {
                args.extend_from_slice(&[
                    "-f".to_string(), "mpegts".to_string(),
                    format!("udp://{}:{}", address, port),
                ]);
            },
            OutputFormat::File { container } => {
                let output_path = self.config.temp_dir.join(format!("{}_{}.{}", 
                    profile.name, chrono::Utc::now().timestamp(), container));
                args.push(output_path.to_string_lossy().to_string());
            }
        }

        Ok(())
    }

    async fn monitor_transcoding_progress(&self, job_id: Uuid, child: &mut Child) -> Result<()> {
        if let Some(stderr) = child.stderr.take() {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buffer = String::new();

            loop {
                buffer.clear();
                match reader.read_line(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // Parse FFmpeg progress from stderr
                        if let Some(progress) = self.parse_ffmpeg_progress(&buffer) {
                            self.update_job_progress(job_id, progress).await;
                        }
                    },
                    Err(e) => {
                        warn!("Error reading FFmpeg stderr: {}", e);
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn parse_ffmpeg_progress(&self, line: &str) -> Option<TranscodingProgress> {
        // Parse FFmpeg progress output
        // Example: frame= 1234 fps= 30.0 q=28.0 size=    1024kB time=00:00:41.33 bitrate= 203.4kbits/s speed=1.0x
        
        if !line.contains("frame=") || !line.contains("fps=") {
            return None;
        }

        let mut frames = 0u64;
        let mut fps = 0.0f32;
        let mut bitrate = 0u32;
        let mut duration = Duration::from_secs(0);

        // Simple parsing - in production, use regex for better parsing
        for part in line.split_whitespace() {
            if let Some(value) = part.strip_prefix("frame=") {
                frames = value.parse().unwrap_or(0);
            } else if let Some(value) = part.strip_prefix("fps=") {
                fps = value.parse().unwrap_or(0.0);
            } else if let Some(value) = part.strip_prefix("bitrate=") {
                if let Some(bitrate_str) = value.strip_suffix("kbits/s") {
                    bitrate = bitrate_str.parse::<f32>().unwrap_or(0.0) as u32;
                }
            } else if let Some(value) = part.strip_prefix("time=") {
                // Parse time format HH:MM:SS.ss
                if let Ok(parsed_time) = Self::parse_duration(value) {
                    duration = parsed_time;
                }
            }
        }

        Some(TranscodingProgress {
            frames_processed: frames,
            duration_processed: duration,
            bitrate_kbps: bitrate,
            fps,
            quality_score: 0.0, // Would need additional analysis
            cpu_usage: 0.0,     // Would need system monitoring
            memory_usage: 0,    // Would need system monitoring
        })
    }

    fn parse_duration(time_str: &str) -> Result<Duration> {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() != 3 {
            return Err(anyhow::anyhow!("Invalid time format"));
        }

        let hours: u64 = parts[0].parse()?;
        let minutes: u64 = parts[1].parse()?;
        let seconds: f64 = parts[2].parse()?;

        let total_seconds = hours * 3600 + minutes * 60 + seconds as u64;
        let nanoseconds = ((seconds - seconds.floor()) * 1_000_000_000.0) as u32;

        Ok(Duration::new(total_seconds, nanoseconds))
    }

    async fn feed_input_data(&self, job_id: Uuid, request: &TranscodingRequest, child: &mut Child) -> Result<()> {
        // For RTMP input, this would connect to the RTMP stream and pipe data
        // For now, we'll just let FFmpeg handle the input directly
        
        match &request.input_source {
            InputSource::Rtmp { .. } => {
                // FFmpeg will handle RTMP input directly
                debug!("Using FFmpeg's built-in RTMP input for job {}", job_id);
            },
            InputSource::File { .. } => {
                // FFmpeg will handle file input directly
                debug!("Using FFmpeg's built-in file input for job {}", job_id);
            },
            InputSource::Network { .. } => {
                // FFmpeg will handle network input directly
                debug!("Using FFmpeg's built-in network input for job {}", job_id);
            }
        }

        Ok(())
    }

    async fn update_job_status(&self, job_id: Uuid, status: JobStatus) {
        let mut jobs = self.active_jobs.write().await;
        if let Some(job) = jobs.get_mut(&job_id) {
            job.status = status;
        }
    }

    async fn update_job_progress(&self, job_id: Uuid, progress: TranscodingProgress) {
        let mut jobs = self.active_jobs.write().await;
        if let Some(job) = jobs.get_mut(&job_id) {
            job.progress = progress;
        }
    }

    async fn cleanup_job(&self, job_id: Uuid) -> Result<()> {
        let job = {
            let mut jobs = self.active_jobs.write().await;
            jobs.remove(&job_id)
        };

        if let Some(job) = job {
            // Clean up temporary files
            for (_profile, path) in job.output_paths {
                if path.exists() {
                    if let Err(e) = fs::remove_file(&path).await {
                        warn!("Failed to cleanup file {}: {}", path.display(), e);
                    }
                }
            }
        }

        Ok(())
    }

    async fn validate_ffmpeg(ffmpeg_path: &Path) -> Result<()> {
        let output = Command::new(ffmpeg_path)
            .arg("-version")
            .output()
            .await
            .context("Failed to execute FFmpeg")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("FFmpeg is not working properly"));
        }

        let version_info = String::from_utf8_lossy(&output.stdout);
        info!("FFmpeg validation successful: {}", version_info.lines().next().unwrap_or("Unknown version"));

        Ok(())
    }

    pub async fn get_job_status(&self, job_id: Uuid) -> Option<TranscodingJob> {
        let jobs = self.active_jobs.read().await;
        jobs.get(&job_id).cloned()
    }

    pub async fn list_active_jobs(&self) -> Vec<TranscodingJob> {
        let jobs = self.active_jobs.read().await;
        jobs.values().cloned().collect()
    }

    pub async fn get_stats(&self) -> TranscodingStats {
        let jobs = self.active_jobs.read().await;
        
        let total_jobs = jobs.len();
        let running_jobs = jobs.values().filter(|j| j.status == JobStatus::Running).count();
        let failed_jobs = jobs.values().filter(|j| j.status == JobStatus::Failed).count();
        let completed_jobs = jobs.values().filter(|j| j.status == JobStatus::Completed).count();

        let total_frames = jobs.values().map(|j| j.progress.frames_processed).sum();
        let avg_fps = if running_jobs > 0 {
            jobs.values()
                .filter(|j| j.status == JobStatus::Running)
                .map(|j| j.progress.fps)
                .sum::<f32>() / running_jobs as f32
        } else {
            0.0
        };

        TranscodingStats {
            total_jobs,
            running_jobs,
            completed_jobs,
            failed_jobs,
            available_slots: self.job_semaphore.available_permits(),
            total_frames_processed: total_frames,
            average_fps: avg_fps,
        }
    }
}

impl Clone for TranscoderManager {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            active_jobs: self.active_jobs.clone(),
            job_semaphore: self.job_semaphore.clone(),
            profiles_map: self.profiles_map.clone(),
            video_codecs_map: self.video_codecs_map.clone(),
            audio_codecs_map: self.audio_codecs_map.clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscodingStats {
    pub total_jobs: usize,
    pub running_jobs: usize,
    pub completed_jobs: usize,
    pub failed_jobs: usize,
    pub available_slots: usize,
    pub total_frames_processed: u64,
    pub average_fps: f32,
}