use anyhow::{Result, Context};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio::time::{timeout, Duration};
use tokio_util::codec::{Decoder, Encoder, Framed};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::RtmpConfig;
use crate::stream_manager::StreamManager;

pub struct RtmpServer {
    config: RtmpConfig,
    stream_manager: Arc<StreamManager>,
    connections: Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>,
    active_streams: Arc<RwLock<HashMap<String, ActiveStream>>>,
}

#[derive(Debug, Clone)]
pub struct RtmpConnection {
    pub id: Uuid,
    pub addr: SocketAddr,
    pub stream_key: Option<String>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub bytes_received: u64,
    pub is_publishing: bool,
}

#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub stream_key: String,
    pub connection_id: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub frame_count: u64,
    pub bytes_transmitted: u64,
}

#[derive(Debug, Clone)]
pub struct RtmpMessage {
    pub message_type: RtmpMessageType,
    pub timestamp: u32,
    pub stream_id: u32,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RtmpMessageType {
    SetChunkSize = 1,
    AbortMessage = 2,
    Acknowledgement = 3,
    UserControl = 4,
    WindowAckSize = 5,
    SetPeerBandwidth = 6,
    AudioData = 8,
    VideoData = 9,
    DataAmf3 = 15,
    SharedObjectAmf3 = 16,
    CommandAmf3 = 17,
    DataAmf0 = 18,
    SharedObjectAmf0 = 19,
    CommandAmf0 = 20,
    AggregateMessage = 22,
}

pub struct RtmpCodec {
    chunk_size: usize,
    window_ack_size: u32,
    peer_bandwidth: u32,
    sequence_number: u32,
    last_ack_sent: u32,
}

impl RtmpCodec {
    pub fn new() -> Self {
        Self {
            chunk_size: 128,
            window_ack_size: 2500000,
            peer_bandwidth: 2500000,
            sequence_number: 0,
            last_ack_sent: 0,
        }
    }
}

impl Decoder for RtmpCodec {
    type Item = RtmpMessage;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 12 {
            return Ok(None);
        }

        // Basic RTMP chunk parsing
        let basic_header = src[0];
        let chunk_stream_id = (basic_header & 0x3F) as u32;
        
        let fmt = (basic_header >> 6) & 0x03;
        let mut offset = 1;

        // Extended chunk stream ID
        let chunk_stream_id = if chunk_stream_id == 0 {
            if src.len() < offset + 1 {
                return Ok(None);
            }
            let id = src[offset] as u32 + 64;
            offset += 1;
            id
        } else if chunk_stream_id == 1 {
            if src.len() < offset + 2 {
                return Ok(None);
            }
            let id = (src[offset] as u32) + ((src[offset + 1] as u32) << 8) + 64;
            offset += 2;
            id
        } else {
            chunk_stream_id
        };

        // Message header based on format type
        let (timestamp, message_length, message_type_id, message_stream_id) = match fmt {
            0 => {
                // Type 0: 11 bytes
                if src.len() < offset + 11 {
                    return Ok(None);
                }
                let timestamp = ((src[offset] as u32) << 16) | 
                              ((src[offset + 1] as u32) << 8) | 
                              (src[offset + 2] as u32);
                let length = ((src[offset + 3] as u32) << 16) | 
                           ((src[offset + 4] as u32) << 8) | 
                           (src[offset + 5] as u32);
                let msg_type = src[offset + 6];
                let stream_id = src[offset + 7] as u32 | 
                              ((src[offset + 8] as u32) << 8) |
                              ((src[offset + 9] as u32) << 16) |
                              ((src[offset + 10] as u32) << 24);
                offset += 11;
                (timestamp, length, msg_type, stream_id)
            },
            1 => {
                // Type 1: 7 bytes
                if src.len() < offset + 7 {
                    return Ok(None);
                }
                let timestamp_delta = ((src[offset] as u32) << 16) | 
                                    ((src[offset + 1] as u32) << 8) | 
                                    (src[offset + 2] as u32);
                let length = ((src[offset + 3] as u32) << 16) | 
                           ((src[offset + 4] as u32) << 8) | 
                           (src[offset + 5] as u32);
                let msg_type = src[offset + 6];
                offset += 7;
                (timestamp_delta, length, msg_type, 0) // Use previous stream ID
            },
            2 => {
                // Type 2: 3 bytes
                if src.len() < offset + 3 {
                    return Ok(None);
                }
                let timestamp_delta = ((src[offset] as u32) << 16) | 
                                    ((src[offset + 1] as u32) << 8) | 
                                    (src[offset + 2] as u32);
                offset += 3;
                (timestamp_delta, 0, 0, 0) // Use previous length, type, and stream ID
            },
            3 => {
                // Type 3: no header
                (0, 0, 0, 0) // Use all previous values
            },
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid chunk format")),
        };

        // Extended timestamp
        let timestamp = if timestamp >= 0xFFFFFF {
            if src.len() < offset + 4 {
                return Ok(None);
            }
            let ext_timestamp = ((src[offset] as u32) << 24) |
                              ((src[offset + 1] as u32) << 16) |
                              ((src[offset + 2] as u32) << 8) |
                              (src[offset + 3] as u32);
            offset += 4;
            ext_timestamp
        } else {
            timestamp
        };

        let payload_size = std::cmp::min(message_length as usize, self.chunk_size);
        
        if src.len() < offset + payload_size {
            return Ok(None);
        }

        let payload = src.split_to(offset + payload_size).split_off(offset).freeze();

        let message_type = match message_type_id {
            1 => RtmpMessageType::SetChunkSize,
            2 => RtmpMessageType::AbortMessage,
            3 => RtmpMessageType::Acknowledgement,
            4 => RtmpMessageType::UserControl,
            5 => RtmpMessageType::WindowAckSize,
            6 => RtmpMessageType::SetPeerBandwidth,
            8 => RtmpMessageType::AudioData,
            9 => RtmpMessageType::VideoData,
            15 => RtmpMessageType::DataAmf3,
            16 => RtmpMessageType::SharedObjectAmf3,
            17 => RtmpMessageType::CommandAmf3,
            18 => RtmpMessageType::DataAmf0,
            19 => RtmpMessageType::SharedObjectAmf0,
            20 => RtmpMessageType::CommandAmf0,
            22 => RtmpMessageType::AggregateMessage,
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Unknown message type")),
        };

        Ok(Some(RtmpMessage {
            message_type,
            timestamp,
            stream_id: message_stream_id,
            payload,
        }))
    }
}

impl Encoder<RtmpMessage> for RtmpCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: RtmpMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // Simplified RTMP message encoding
        dst.reserve(12 + item.payload.len());
        
        // Basic header (format 0, chunk stream ID 2)
        dst.extend_from_slice(&[0x02]);
        
        // Message header (type 0)
        dst.extend_from_slice(&[
            (item.timestamp >> 16) as u8,
            (item.timestamp >> 8) as u8,
            item.timestamp as u8,
            (item.payload.len() >> 16) as u8,
            (item.payload.len() >> 8) as u8,
            item.payload.len() as u8,
            item.message_type as u8,
            item.stream_id as u8,
            (item.stream_id >> 8) as u8,
            (item.stream_id >> 16) as u8,
            (item.stream_id >> 24) as u8,
        ]);
        
        // Payload
        dst.extend_from_slice(&item.payload);
        
        Ok(())
    }
}

impl RtmpServer {
    pub async fn new(config: RtmpConfig, stream_manager: Arc<StreamManager>) -> Result<Self> {
        Ok(Self {
            config,
            stream_manager,
            connections: Arc::new(RwLock::new(HashMap::new())),
            active_streams: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.config.bind_address).await
            .context("Failed to bind RTMP server")?;

        info!("RTMP server listening on {}", self.config.bind_address);

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    debug!("New RTMP connection from {}", addr);
                    
                    let connections = self.connections.clone();
                    let active_streams = self.active_streams.clone();
                    let stream_manager = self.stream_manager.clone();
                    let config = self.config.clone();

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(
                            stream, addr, connections, active_streams, stream_manager, config
                        ).await {
                            error!("RTMP connection error for {}: {}", addr, e);
                        }
                    });
                },
                Err(e) => {
                    error!("Failed to accept RTMP connection: {}", e);
                }
            }
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        addr: SocketAddr,
        connections: Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>,
        active_streams: Arc<RwLock<HashMap<String, ActiveStream>>>,
        stream_manager: Arc<StreamManager>,
        config: RtmpConfig,
    ) -> Result<()> {
        let connection_id = Uuid::new_v4();
        let connected_at = chrono::Utc::now();

        // Add connection to tracking
        {
            let mut conns = connections.write().await;
            conns.insert(addr, RtmpConnection {
                id: connection_id,
                addr,
                stream_key: None,
                connected_at,
                bytes_received: 0,
                is_publishing: false,
            });
        }

        let codec = RtmpCodec::new();
        let mut framed = Framed::new(stream, codec);

        // RTMP handshake
        Self::perform_handshake(&mut framed).await?;

        // Connection timeout
        let connection_future = Self::handle_rtmp_messages(
            &mut framed, 
            addr, 
            connections.clone(), 
            active_streams.clone(), 
            stream_manager.clone()
        );

        match timeout(Duration::from_secs(config.connection_timeout), connection_future).await {
            Ok(result) => result,
            Err(_) => {
                warn!("RTMP connection {} timed out", addr);
                Err(anyhow::anyhow!("Connection timeout"))
            }
        }?;

        // Clean up connection
        connections.write().await.remove(&addr);
        info!("RTMP connection {} closed", addr);

        Ok(())
    }

    async fn perform_handshake(framed: &mut Framed<TcpStream, RtmpCodec>) -> Result<()> {
        // Simplified RTMP handshake
        // In production, this should implement the full RTMP handshake protocol
        
        let mut c0c1 = BytesMut::new();
        c0c1.resize(1537, 0);
        
        // Wait for C0+C1
        if let Some(data) = framed.next().await {
            let _message = data?;
            // Process handshake data
        }

        // Send S0+S1+S2
        let mut s0s1s2 = BytesMut::new();
        s0s1s2.resize(3073, 0);
        s0s1s2[0] = 0x03; // RTMP version
        
        // Send handshake response
        // This is simplified - production should implement proper handshake
        
        info!("RTMP handshake completed");
        Ok(())
    }

    async fn handle_rtmp_messages(
        framed: &mut Framed<TcpStream, RtmpCodec>,
        addr: SocketAddr,
        connections: Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>,
        active_streams: Arc<RwLock<HashMap<String, ActiveStream>>>,
        stream_manager: Arc<StreamManager>,
    ) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<Bytes>(1000);

        while let Some(message_result) = framed.next().await {
            let message = message_result?;
            
            match message.message_type {
                RtmpMessageType::CommandAmf0 => {
                    Self::handle_command(
                        &message, 
                        addr, 
                        &connections, 
                        &active_streams, 
                        &stream_manager,
                        framed
                    ).await?;
                },
                RtmpMessageType::VideoData | RtmpMessageType::AudioData => {
                    // Forward media data to stream manager
                    if let Some(stream_key) = Self::get_stream_key(&connections, addr).await {
                        if let Err(e) = tx.send(message.payload).await {
                            warn!("Failed to send media data: {}", e);
                            break;
                        }
                        
                        // Update connection stats
                        Self::update_connection_stats(&connections, addr, message.payload.len() as u64).await;
                    }
                },
                RtmpMessageType::SetChunkSize => {
                    // Handle chunk size changes
                    if message.payload.len() >= 4 {
                        let chunk_size = u32::from_be_bytes([
                            message.payload[0], message.payload[1], 
                            message.payload[2], message.payload[3]
                        ]);
                        debug!("Chunk size set to: {}", chunk_size);
                    }
                },
                _ => {
                    debug!("Received RTMP message type: {:?}", message.message_type);
                }
            }
        }

        Ok(())
    }

    async fn handle_command(
        message: &RtmpMessage,
        addr: SocketAddr,
        connections: &Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>,
        active_streams: &Arc<RwLock<HashMap<String, ActiveStream>>>,
        stream_manager: &Arc<StreamManager>,
        framed: &mut Framed<TcpStream, RtmpCodec>,
    ) -> Result<()> {
        // Parse AMF0 command (simplified)
        let payload = &message.payload;
        if payload.len() < 2 {
            return Ok(());
        }

        // This is a simplified AMF0 parser - production should use a proper AMF library
        let command_name = Self::parse_amf0_string(&payload[1..])?;
        
        match command_name.as_str() {
            "connect" => {
                debug!("RTMP connect command from {}", addr);
                Self::send_connect_response(framed).await?;
            },
            "publish" => {
                let stream_key = Self::parse_stream_key_from_publish(&payload[1..])?;
                info!("RTMP publish command from {} with key: {}", addr, stream_key);
                
                // Authenticate stream key
                if let Some(stream_id) = stream_manager.authenticate_stream_key(&stream_key).await? {
                    // Start streaming
                    {
                        let mut conns = connections.write().await;
                        if let Some(conn) = conns.get_mut(&addr) {
                            conn.stream_key = Some(stream_key.clone());
                            conn.is_publishing = true;
                        }
                    }

                    {
                        let mut streams = active_streams.write().await;
                        streams.insert(stream_key.clone(), ActiveStream {
                            stream_key: stream_key.clone(),
                            connection_id: connections.read().await.get(&addr).unwrap().id,
                            started_at: chrono::Utc::now(),
                            frame_count: 0,
                            bytes_transmitted: 0,
                        });
                    }

                    stream_manager.start_stream(stream_id).await?;
                    Self::send_publish_response(framed, true).await?;
                } else {
                    warn!("Invalid stream key: {}", stream_key);
                    Self::send_publish_response(framed, false).await?;
                }
            },
            "play" => {
                debug!("RTMP play command from {}", addr);
                // Handle play requests if needed
            },
            _ => {
                debug!("Unknown RTMP command: {}", command_name);
            }
        }

        Ok(())
    }

    async fn send_connect_response(framed: &mut Framed<TcpStream, RtmpCodec>) -> Result<()> {
        // Send _result response for connect
        let response = RtmpMessage {
            message_type: RtmpMessageType::CommandAmf0,
            timestamp: 0,
            stream_id: 0,
            payload: Self::create_connect_response_payload(),
        };
        
        framed.send(response).await?;
        Ok(())
    }

    async fn send_publish_response(framed: &mut Framed<TcpStream, RtmpCodec>, success: bool) -> Result<()> {
        let response = RtmpMessage {
            message_type: RtmpMessageType::CommandAmf0,
            timestamp: 0,
            stream_id: 1,
            payload: Self::create_publish_response_payload(success),
        };
        
        framed.send(response).await?;
        Ok(())
    }

    fn create_connect_response_payload() -> Bytes {
        // Simplified AMF0 response - production should use proper AMF encoding
        let mut payload = BytesMut::new();
        payload.extend_from_slice(b"\x02\x00\x07_result"); // AMF0 string "_result"
        payload.extend_from_slice(b"\x00\x3f\xf0\x00\x00\x00\x00\x00\x00"); // Number 1.0
        // Add more AMF0 encoded response data...
        payload.freeze()
    }

    fn create_publish_response_payload(success: bool) -> Bytes {
        let mut payload = BytesMut::new();
        if success {
            payload.extend_from_slice(b"\x02\x00\x0fonPublishStart");
        } else {
            payload.extend_from_slice(b"\x02\x00\x0donPublishError");
        }
        payload.freeze()
    }

    fn parse_amf0_string(data: &[u8]) -> Result<String> {
        if data.len() < 3 {
            return Err(anyhow::anyhow!("Invalid AMF0 string"));
        }
        
        if data[0] != 0x02 {
            return Err(anyhow::anyhow!("Not an AMF0 string"));
        }
        
        let length = ((data[1] as usize) << 8) | (data[2] as usize);
        if data.len() < 3 + length {
            return Err(anyhow::anyhow!("AMF0 string length mismatch"));
        }
        
        Ok(String::from_utf8_lossy(&data[3..3+length]).to_string())
    }

    fn parse_stream_key_from_publish(data: &[u8]) -> Result<String> {
        // This is simplified - should properly parse AMF0 publish command
        // Skip to the stream key parameter
        if let Ok(first_param) = Self::parse_amf0_string(data) {
            if first_param == "publish" {
                // Look for the next string parameter (stream key)
                if let Some(start) = data.iter().position(|&b| b == 0x02) {
                    if start + 1 < data.len() {
                        return Self::parse_amf0_string(&data[start..]);
                    }
                }
            }
        }
        Err(anyhow::anyhow!("Could not parse stream key"))
    }

    async fn get_stream_key(
        connections: &Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>, 
        addr: SocketAddr
    ) -> Option<String> {
        let conns = connections.read().await;
        conns.get(&addr)?.stream_key.clone()
    }

    async fn update_connection_stats(
        connections: &Arc<RwLock<HashMap<SocketAddr, RtmpConnection>>>,
        addr: SocketAddr,
        bytes: u64,
    ) {
        let mut conns = connections.write().await;
        if let Some(conn) = conns.get_mut(&addr) {
            conn.bytes_received += bytes;
        }
    }

    pub async fn get_active_streams(&self) -> HashMap<String, ActiveStream> {
        self.active_streams.read().await.clone()
    }

    pub async fn get_connections(&self) -> HashMap<SocketAddr, RtmpConnection> {
        self.connections.read().await.clone()
    }

    pub async fn disconnect_stream(&self, stream_key: &str) -> Result<()> {
        let mut streams = self.active_streams.write().await;
        streams.remove(stream_key);
        info!("Disconnected stream: {}", stream_key);
        Ok(())
    }
}