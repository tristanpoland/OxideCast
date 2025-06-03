use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub rtmp: RtmpConfig,
    pub multicast: MulticastConfig,
    pub transcoding: TranscodingConfig,
    pub webrtc: WebRtcConfig,
    pub hls: HlsConfig,
    pub storage: StorageConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub workers: Option<usize>,
    pub max_connections: usize,
    pub keep_alive: u64,
    pub secret_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout: u64,
    pub idle_timeout: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub jwt_expiry: u64,
    pub refresh_expiry: u64,
    pub bcrypt_cost: u32,
    pub require_email_verification: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RtmpConfig {
    pub bind_address: SocketAddr,
    pub chunk_size: usize,
    pub max_connections: usize,
    pub connection_timeout: u64,
    pub stream_timeout: u64,
    pub buffer_size: usize,
    pub enable_auth: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MulticastConfig {
    pub interface: IpAddr,
    pub base_address: Ipv4Addr,
    pub port_range: (u16, u16),
    pub ttl: u32,
    pub buffer_size: usize,
    pub packet_size: usize,
    pub group_timeout: u64,
    pub enable_igmp: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscodingConfig {
    pub ffmpeg_path: PathBuf,
    pub max_concurrent: usize,
    pub temp_dir: PathBuf,
    pub hardware_acceleration: HardwareAcceleration,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
    pub profiles: Vec<EncodingProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HardwareAcceleration {
    pub enabled: bool,
    pub device: Option<String>,
    pub codec: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VideoCodec {
    pub name: String,
    pub encoder: String,
    pub hardware: bool,
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudioCodec {
    pub name: String,
    pub encoder: String,
    pub bitrate: u32,
    pub channels: u8,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EncodingProfile {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub framerate: f32,
    pub keyframe_interval: u32,
    pub audio_bitrate: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebRtcConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<TurnServer>,
    pub ice_timeout: u64,
    pub dtls_timeout: u64,
    pub max_bitrate: u32,
    pub min_bitrate: u32,
    pub enable_simulcast: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnServer {
    pub url: String,
    pub username: String,
    pub credential: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HlsConfig {
    pub segment_duration: u32,
    pub playlist_length: u32,
    pub segment_directory: PathBuf,
    pub cleanup_interval: u64,
    pub enable_low_latency: bool,
    pub partial_segment_duration: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    pub base_path: PathBuf,
    pub recordings_path: PathBuf,
    pub thumbnails_path: PathBuf,
    pub max_storage_size: u64,
    pub cleanup_interval: u64,
    pub enable_s3: bool,
    pub s3_config: Option<S3Config>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    pub level: String,
    pub output: LogOutput,
    pub rotation: LogRotation,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum LogOutput {
    Stdout,
    File { path: PathBuf },
    Both { path: PathBuf },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogRotation {
    pub enabled: bool,
    pub max_size: u64,
    pub max_age: u64,
    pub max_files: u32,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.auth.jwt_secret.len() < 32 {
            anyhow::bail!("JWT secret must be at least 32 characters long");
        }

        if self.multicast.port_range.0 >= self.multicast.port_range.1 {
            anyhow::bail!("Invalid multicast port range");
        }

        if self.transcoding.max_concurrent == 0 {
            anyhow::bail!("Max concurrent transcoding jobs must be > 0");
        }

        if !self.transcoding.ffmpeg_path.exists() {
            anyhow::bail!("FFmpeg path does not exist: {:?}", self.transcoding.ffmpeg_path);
        }

        std::fs::create_dir_all(&self.transcoding.temp_dir)?;
        std::fs::create_dir_all(&self.storage.base_path)?;
        std::fs::create_dir_all(&self.storage.recordings_path)?;
        std::fs::create_dir_all(&self.storage.thumbnails_path)?;
        std::fs::create_dir_all(&self.hls.segment_directory)?;

        Ok(())
    }

    pub fn default_config() -> Self {
        Self {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                port: 8080,
                workers: None,
                max_connections: 10000,
                keep_alive: 30,
                secret_key: "your-secret-key-change-this-in-production".to_string(),
            },
            database: DatabaseConfig {
                url: "postgresql://user:password@localhost:5432/streaming".to_string(),
                max_connections: 100,
                min_connections: 5,
                connect_timeout: 30,
                idle_timeout: 600,
            },
            auth: AuthConfig {
                jwt_secret: "your-jwt-secret-change-this-in-production-minimum-32-chars".to_string(),
                jwt_expiry: 3600,
                refresh_expiry: 86400 * 7,
                bcrypt_cost: 12,
                require_email_verification: false,
            },
            rtmp: RtmpConfig {
                bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1935),
                chunk_size: 4096,
                max_connections: 1000,
                connection_timeout: 30,
                stream_timeout: 300,
                buffer_size: 1024 * 1024,
                enable_auth: true,
            },
            multicast: MulticastConfig {
                interface: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                base_address: Ipv4Addr::new(239, 255, 0, 1),
                port_range: (30000, 40000),
                ttl: 64,
                buffer_size: 1024 * 1024,
                packet_size: 1400,
                group_timeout: 300,
                enable_igmp: true,
            },
            transcoding: TranscodingConfig {
                ffmpeg_path: PathBuf::from("/usr/bin/ffmpeg"),
                max_concurrent: 10,
                temp_dir: PathBuf::from("/tmp/streaming"),
                hardware_acceleration: HardwareAcceleration {
                    enabled: false,
                    device: None,
                    codec: None,
                },
                video_codecs: vec![
                    VideoCodec {
                        name: "h264".to_string(),
                        encoder: "libx264".to_string(),
                        hardware: false,
                        options: vec![
                            ("preset".to_string(), "medium".to_string()),
                            ("tune".to_string(), "zerolatency".to_string()),
                        ],
                    },
                ],
                audio_codecs: vec![
                    AudioCodec {
                        name: "aac".to_string(),
                        encoder: "aac".to_string(),
                        bitrate: 128000,
                        channels: 2,
                        sample_rate: 48000,
                    },
                ],
                profiles: vec![
                    EncodingProfile {
                        name: "720p".to_string(),
                        width: 1280,
                        height: 720,
                        bitrate: 2500000,
                        framerate: 30.0,
                        keyframe_interval: 60,
                        audio_bitrate: 128000,
                    },
                    EncodingProfile {
                        name: "480p".to_string(),
                        width: 854,
                        height: 480,
                        bitrate: 1000000,
                        framerate: 30.0,
                        keyframe_interval: 60,
                        audio_bitrate: 96000,
                    },
                ],
            },
            webrtc: WebRtcConfig {
                stun_servers: vec!["stun:stun.l.google.com:19302".to_string()],
                turn_servers: vec![],
                ice_timeout: 30,
                dtls_timeout: 30,
                max_bitrate: 5000000,
                min_bitrate: 100000,
                enable_simulcast: true,
            },
            hls: HlsConfig {
                segment_duration: 6,
                playlist_length: 10,
                segment_directory: PathBuf::from("/tmp/streaming/hls"),
                cleanup_interval: 300,
                enable_low_latency: false,
                partial_segment_duration: 0.2,
            },
            storage: StorageConfig {
                base_path: PathBuf::from("/data/streaming"),
                recordings_path: PathBuf::from("/data/streaming/recordings"),
                thumbnails_path: PathBuf::from("/data/streaming/thumbnails"),
                max_storage_size: 1024 * 1024 * 1024 * 100, // 100GB
                cleanup_interval: 3600,
                enable_s3: false,
                s3_config: None,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                output: LogOutput::Stdout,
                rotation: LogRotation {
                    enabled: false,
                    max_size: 1024 * 1024 * 100, // 100MB
                    max_age: 86400 * 7, // 7 days
                    max_files: 10,
                },
            },
        }
    }
}