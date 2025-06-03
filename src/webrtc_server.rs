use anyhow::{Result, Context};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock, Mutex};
use tokio::time::{interval, timeout};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RtpCodecCapability};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::{RTCRtpTransceiver, RTCRtpTransceiverDirection};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;
use webrtc::util::Unmarshal;

use crate::config::WebRtcConfig;

pub struct WebRtcServer {
    config: WebRtcConfig,
    api: webrtc::api::API,
    active_streams: Arc<RwLock<HashMap<String, WebRtcStream>>>,
    peer_connections: Arc<RwLock<HashMap<Uuid, WebRtcPeerConnection>>>,
    ice_servers: Vec<RTCIceServer>,
}

#[derive(Debug, Clone)]
pub struct WebRtcStream {
    pub stream_key: String,
    pub multicast_address: Ipv4Addr,
    pub multicast_port: u16,
    pub video_track: Option<Arc<TrackLocalStaticRTP>>,
    pub audio_track: Option<Arc<TrackLocalStaticRTP>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub viewer_count: usize,
    pub packet_count: u64,
    pub bytes_transmitted: u64,
}

#[derive(Debug)]
pub struct WebRtcPeerConnection {
    pub id: Uuid,
    pub stream_key: String,
    pub peer_connection: Arc<RTCPeerConnection>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub connection_state: RTCPeerConnectionState,
    pub viewer_info: ViewerInfo,
    pub stats: ConnectionStats,
}

#[derive(Debug, Clone)]
pub struct ViewerInfo {
    pub ip_address: String,
    pub user_agent: Option<String>,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub round_trip_time_ms: f64,
    pub packet_loss_rate: f64,
    pub jitter_ms: f64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct WebRtcOffer {
    pub sdp: String,
    pub stream_key: String,
    pub session_id: String,
    pub viewer_info: ViewerInfo,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct WebRtcAnswer {
    pub sdp: String,
    pub session_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
    pub session_id: String,
}

impl WebRtcServer {
    pub async fn new(config: WebRtcConfig) -> Result<Self> {
        // Setup media engine
        let mut media_engine = MediaEngine::default();
        
        // Register video codecs
        media_engine.register_codec(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f".to_owned(),
                rtcp_feedback: vec![],
            },
            webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
        )?;

        // Register audio codecs
        media_engine.register_codec(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Audio,
        )?;

        // Setup interceptor registry
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        // Build API
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        // Configure ICE servers
        let mut ice_servers = Vec::new();
        
        // Add STUN servers
        for stun_url in &config.stun_servers {
            ice_servers.push(RTCIceServer {
                urls: vec![stun_url.clone()],
                ..Default::default()
            });
        }

        // Add TURN servers
        for turn_server in &config.turn_servers {
            ice_servers.push(RTCIceServer {
                urls: vec![turn_server.url.clone()],
                username: turn_server.username.clone(),
                credential: turn_server.credential.clone(),
                credential_type: webrtc::ice_transport::ice_credential_type::RTCIceCredentialType::Password,
            });
        }

        Ok(Self {
            config,
            api,
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            peer_connections: Arc::new(RwLock::new(HashMap::new())),
            ice_servers,
        })
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting WebRTC server");

        // Start background tasks
        self.start_background_tasks().await?;

        info!("WebRTC server started successfully");
        Ok(())
    }

    async fn start_background_tasks(&self) -> Result<()> {
        // Connection cleanup task
        let peer_connections = self.peer_connections.clone();
        tokio::spawn(async move {
            Self::connection_cleanup_task(peer_connections).await;
        });

        // Stats collection task
        let peer_connections = self.peer_connections.clone();
        tokio::spawn(async move {
            Self::stats_collection_task(peer_connections).await;
        });

        Ok(())
    }

    pub async fn start_stream(&self, stream_key: &str, multicast_address: Ipv4Addr, multicast_port: u16) -> Result<()> {
        // Create video track
        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f".to_owned(),
                rtcp_feedback: vec![],
            },
            format!("video_{}", stream_key),
            format!("webrtc_video_{}", stream_key),
        ));

        // Create audio track
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            format!("audio_{}", stream_key),
            format!("webrtc_audio_{}", stream_key),
        ));

        let webrtc_stream = WebRtcStream {
            stream_key: stream_key.to_string(),
            multicast_address,
            multicast_port,
            video_track: Some(video_track.clone()),
            audio_track: Some(audio_track.clone()),
            started_at: chrono::Utc::now(),
            viewer_count: 0,
            packet_count: 0,
            bytes_transmitted: 0,
        };

        // Add to active streams
        {
            let mut streams = self.active_streams.write().await;
            streams.insert(stream_key.to_string(), webrtc_stream);
        }

        // Start multicast receiver
        self.start_multicast_receiver(stream_key, multicast_address, multicast_port, video_track, audio_track).await?;

        info!("Started WebRTC stream for key '{}' receiving from {}:{}", 
              stream_key, multicast_address, multicast_port);

        Ok(())
    }

    pub async fn stop_stream(&self, stream_key: &str) -> Result<()> {
        // Remove from active streams
        {
            let mut streams = self.active_streams.write().await;
            streams.remove(stream_key);
        }

        // Close all peer connections for this stream
        let connection_ids: Vec<Uuid> = {
            let connections = self.peer_connections.read().await;
            connections.values()
                .filter(|conn| conn.stream_key == stream_key)
                .map(|conn| conn.id)
                .collect()
        };

        for connection_id in connection_ids {
            self.close_peer_connection(connection_id).await?;
        }

        info!("Stopped WebRTC stream for key '{}'", stream_key);
        Ok(())
    }

    async fn start_multicast_receiver(
        &self,
        stream_key: &str,
        multicast_address: Ipv4Addr,
        multicast_port: u16,
        video_track: Arc<TrackLocalStaticRTP>,
        audio_track: Arc<TrackLocalStaticRTP>,
    ) -> Result<()> {
        let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), multicast_port);
        let socket = UdpSocket::bind(socket_addr).await?;

        // Join multicast group
        socket.join_multicast_v4(multicast_address, Ipv4Addr::UNSPECIFIED)?;

        let stream_key = stream_key.to_string();
        let active_streams = self.active_streams.clone();

        tokio::spawn(async move {
            let mut buffer = [0u8; 1500];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((size, _)) => {
                        if size < 16 {
                            continue; // Skip packets that are too small
                        }

                        // Parse custom packet header
                        let sequence_number = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                        let timestamp = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
                        let payload_type = buffer[8];
                        
                        let payload = &buffer[16..size];

                        // Update stream stats
                        {
                            let mut streams = active_streams.write().await;
                            if let Some(stream) = streams.get_mut(&stream_key) {
                                stream.packet_count += 1;
                                stream.bytes_transmitted += size as u64;
                            }
                        }

                        // Forward to appropriate track
                        let track = match payload_type {
                            0 => Some(video_track.clone()), // Video
                            1 => Some(audio_track.clone()), // Audio
                            _ => None,
                        };

                        if let Some(track) = track {
                            if let Err(e) = track.write_rtp(&webrtc::rtp::packet::Packet {
                                header: webrtc::rtp::header::Header {
                                    version: 2,
                                    padding: false,
                                    extension: false,
                                    marker: payload_type == 0, // Mark video packets
                                    payload_type: if payload_type == 0 { 96 } else { 97 },
                                    sequence_number: sequence_number as u16,
                                    timestamp,
                                    ssrc: 1,
                                    csrc: vec![],
                                    extension_profile: 0,
                                    extensions: vec![],
                                    extensions_padding: 0,
                                },
                                payload: Bytes::copy_from_slice(payload),
                            }).await {
                                debug!("Failed to write RTP packet: {}", e);
                            }
                        }
                    },
                    Err(e) => {
                        error!("Error receiving multicast data: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn create_peer_connection(&self, offer: WebRtcOffer) -> Result<WebRtcAnswer> {
        // Validate stream exists
        let stream_exists = {
            let streams = self.active_streams.read().await;
            streams.contains_key(&offer.stream_key)
        };

        if !stream_exists {
            return Err(anyhow::anyhow!("Stream not found: {}", offer.stream_key));
        }

        // Create peer connection configuration
        let config = RTCConfiguration {
            ice_servers: self.ice_servers.clone(),
            ..Default::default()
        };

        // Create peer connection
        let peer_connection = Arc::new(self.api.new_peer_connection(config).await?);
        let connection_id = Uuid::new_v4();

        // Add tracks from the stream
        {
            let streams = self.active_streams.read().await;
            if let Some(stream) = streams.get(&offer.stream_key) {
                if let Some(video_track) = &stream.video_track {
                    peer_connection.add_track(video_track.clone()).await?;
                }
                if let Some(audio_track) = &stream.audio_track {
                    peer_connection.add_track(audio_track.clone()).await?;
                }
            }
        }

        // Set up connection state handler
        let pc_clone = peer_connection.clone();
        let connection_id_clone = connection_id;
        let peer_connections = self.peer_connections.clone();
        
        peer_connection.on_peer_connection_state_change(Box::new(move |state| {
            let pc = pc_clone.clone();
            let conn_id = connection_id_clone;
            let connections = peer_connections.clone();
            
            Box::pin(async move {
                info!("Peer connection {} state changed to: {:?}", conn_id, state);
                
                // Update connection state
                {
                    let mut conns = connections.write().await;
                    if let Some(conn) = conns.get_mut(&conn_id) {
                        conn.connection_state = state;
                        conn.last_activity = chrono::Utc::now();
                    }
                }

                // Handle disconnections
                match state {
                    RTCPeerConnectionState::Disconnected | 
                    RTCPeerConnectionState::Failed | 
                    RTCPeerConnectionState::Closed => {
                        let _ = pc.close().await;
                        let mut conns = connections.write().await;
                        conns.remove(&conn_id);
                    },
                    _ => {}
                }
            })
        }));

        // Set up ICE candidate handler
        let pc_clone = peer_connection.clone();
        peer_connection.on_ice_candidate(Box::new(move |candidate| {
            let pc = pc_clone.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    debug!("ICE candidate: {}", candidate.to_string());
                    // In a real application, send this candidate to the client
                }
            })
        }));

        // Set remote description
        let offer_sdp = RTCSessionDescription::offer(offer.sdp)?;
        peer_connection.set_remote_description(offer_sdp).await?;

        // Create answer
        let answer = peer_connection.create_answer(None).await?;
        peer_connection.set_local_description(answer.clone()).await?;

        // Store peer connection
        let webrtc_peer = WebRtcPeerConnection {
            id: connection_id,
            stream_key: offer.stream_key.clone(),
            peer_connection: peer_connection.clone(),
            created_at: chrono::Utc::now(),
            last_activity: chrono::Utc::now(),
            connection_state: RTCPeerConnectionState::New,
            viewer_info: offer.viewer_info,
            stats: ConnectionStats {
                packets_sent: 0,
                packets_received: 0,
                bytes_sent: 0,
                bytes_received: 0,
                round_trip_time_ms: 0.0,
                packet_loss_rate: 0.0,
                jitter_ms: 0.0,
            },
        };

        {
            let mut connections = self.peer_connections.write().await;
            connections.insert(connection_id, webrtc_peer);
        }

        // Update viewer count
        self.update_viewer_count(&offer.stream_key).await;

        info!("Created peer connection {} for stream '{}'", connection_id, offer.stream_key);

        Ok(WebRtcAnswer {
            sdp: answer.sdp,
            session_id: offer.session_id,
        })
    }

    pub async fn handle_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        // Find peer connection by session ID
        let peer_connection = {
            let connections = self.peer_connections.read().await;
            connections.values()
                .find(|conn| conn.viewer_info.session_id == candidate.session_id)
                .map(|conn| conn.peer_connection.clone())
        };

        if let Some(pc) = peer_connection {
            let ice_candidate = webrtc::ice_transport::ice_candidate::RTCIceCandidate::from(&candidate.candidate)?;
            pc.add_ice_candidate(ice_candidate).await?;
            debug!("Added ICE candidate for session {}", candidate.session_id);
        } else {
            warn!("Peer connection not found for session: {}", candidate.session_id);
        }

        Ok(())
    }

    async fn close_peer_connection(&self, connection_id: Uuid) -> Result<()> {
        let peer_connection = {
            let mut connections = self.peer_connections.write().await;
            connections.remove(&connection_id)
        };

        if let Some(conn) = peer_connection {
            conn.peer_connection.close().await?;
            self.update_viewer_count(&conn.stream_key).await;
            info!("Closed peer connection {}", connection_id);
        }

        Ok(())
    }

    async fn update_viewer_count(&self, stream_key: &str) {
        let viewer_count = {
            let connections = self.peer_connections.read().await;
            connections.values()
                .filter(|conn| conn.stream_key == stream_key && 
                       conn.connection_state == RTCPeerConnectionState::Connected)
                .count()
        };

        let mut streams = self.active_streams.write().await;
        if let Some(stream) = streams.get_mut(stream_key) {
            stream.viewer_count = viewer_count;
        }
    }

    pub async fn get_stream_url(&self, stream_key: &str) -> Result<String> {
        let streams = self.active_streams.read().await;
        if streams.contains_key(stream_key) {
            Ok(format!("/api/v1/webrtc/offer/{}", stream_key))
        } else {
            Err(anyhow::anyhow!("Stream not found"))
        }
    }

    pub async fn get_stream_stats(&self, stream_key: &str) -> Option<WebRtcStreamStats> {
        let streams = self.active_streams.read().await;
        let connections = self.peer_connections.read().await;

        if let Some(stream) = streams.get(stream_key) {
            let connected_viewers = connections.values()
                .filter(|conn| conn.stream_key == stream_key && 
                       conn.connection_state == RTCPeerConnectionState::Connected)
                .count();

            Some(WebRtcStreamStats {
                stream_key: stream_key.to_string(),
                viewer_count: connected_viewers,
                total_connections: connections.values()
                    .filter(|conn| conn.stream_key == stream_key)
                    .count(),
                packets_sent: stream.packet_count,
                bytes_transmitted: stream.bytes_transmitted,
                uptime_seconds: chrono::Utc::now()
                    .signed_duration_since(stream.started_at)
                    .num_seconds() as u64,
            })
        } else {
            None
        }
    }

    async fn connection_cleanup_task(peer_connections: Arc<RwLock<HashMap<Uuid, WebRtcPeerConnection>>>) {
        let mut interval = interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            let now = chrono::Utc::now();
            let mut connections_to_remove = Vec::new();

            {
                let connections = peer_connections.read().await;
                for (id, conn) in connections.iter() {
                    // Remove connections that have been inactive for too long
                    let inactive_duration = now.signed_duration_since(conn.last_activity);
                    if inactive_duration.num_seconds() > 300 || // 5 minutes
                       conn.connection_state == RTCPeerConnectionState::Failed ||
                       conn.connection_state == RTCPeerConnectionState::Disconnected {
                        connections_to_remove.push(*id);
                    }
                }
            }

            if !connections_to_remove.is_empty() {
                let mut connections = peer_connections.write().await;
                for id in connections_to_remove {
                    if let Some(conn) = connections.remove(&id) {
                        let _ = conn.peer_connection.close().await;
                        debug!("Cleaned up inactive peer connection: {}", id);
                    }
                }
            }
        }
    }

    async fn stats_collection_task(peer_connections: Arc<RwLock<HashMap<Uuid, WebRtcPeerConnection>>>) {
        let mut interval = interval(Duration::from_secs(10));

        loop {
            interval.tick().await;

            let connections = peer_connections.read().await;
            for conn in connections.values() {
                if conn.connection_state == RTCPeerConnectionState::Connected {
                    // Collect WebRTC stats
                    if let Ok(stats) = conn.peer_connection.get_stats().await {
                        debug!("Collected stats for connection {}: {:?}", conn.id, stats.keys().collect::<Vec<_>>());
                        // Process and store stats
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WebRtcStreamStats {
    pub stream_key: String,
    pub viewer_count: usize,
    pub total_connections: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
    pub uptime_seconds: u64,
}