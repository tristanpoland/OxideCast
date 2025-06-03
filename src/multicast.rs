use anyhow::{Result, Context};
use bytes::Bytes;
use futures::SinkExt;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, RwLock, Mutex};
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::MulticastConfig;

pub struct MulticastManager {
    config: MulticastConfig,
    groups: Arc<RwLock<HashMap<String, MulticastGroup>>>,
    sockets: Arc<RwLock<HashMap<Ipv4Addr, Arc<UdpSocket>>>>,
    address_allocator: Arc<Mutex<AddressAllocator>>,
    packet_sender: Arc<PacketSender>,
}

#[derive(Debug, Clone)]
pub struct MulticastGroup {
    pub id: Uuid,
    pub stream_key: String,
    pub multicast_address: Ipv4Addr,
    pub port: u16,
    pub created_at: Instant,
    pub last_activity: Instant,
    pub viewer_count: usize,
    pub packet_count: u64,
    pub bytes_sent: u64,
    pub viewers: HashMap<SocketAddr, ViewerInfo>,
}

#[derive(Debug, Clone)]
pub struct ViewerInfo {
    pub id: Uuid,
    pub joined_at: Instant,
    pub last_seen: Instant,
    pub packets_received: u64,
    pub quality_profile: String,
}

#[derive(Debug, Clone)]
pub struct MulticastPacket {
    pub stream_key: String,
    pub sequence_number: u32,
    pub timestamp: u32,
    pub payload_type: PayloadType,
    pub data: Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PayloadType {
    Video,
    Audio,
    Metadata,
    KeyFrame,
}

struct AddressAllocator {
    base_address: Ipv4Addr,
    port_range: (u16, u16),
    allocated_addresses: HashMap<Ipv4Addr, u16>,
    allocated_ports: HashMap<u16, bool>,
    next_address_offset: u32,
    next_port: u16,
}

struct PacketSender {
    sockets: Arc<RwLock<HashMap<Ipv4Addr, Arc<UdpSocket>>>>,
    config: MulticastConfig,
}

impl AddressAllocator {
    fn new(base_address: Ipv4Addr, port_range: (u16, u16)) -> Self {
        Self {
            base_address,
            port_range,
            allocated_addresses: HashMap::new(),
            allocated_ports: HashMap::new(),
            next_address_offset: 1,
            next_port: port_range.0,
        }
    }

    fn allocate_address(&mut self) -> Result<(Ipv4Addr, u16)> {
        // Allocate multicast address
        let base_octets = self.base_address.octets();
        let mut address = None;
        
        for _ in 0..1000 { // Prevent infinite loop
            let offset = self.next_address_offset;
            let new_octets = [
                base_octets[0],
                base_octets[1],
                ((base_octets[2] as u32 + (offset >> 8)) % 256) as u8,
                ((base_octets[3] as u32 + (offset & 0xFF)) % 256) as u8,
            ];
            let addr = Ipv4Addr::from(new_octets);
            
            if !self.allocated_addresses.contains_key(&addr) {
                address = Some(addr);
                self.next_address_offset += 1;
                break;
            }
            self.next_address_offset += 1;
        }

        let address = address.ok_or_else(|| anyhow::anyhow!("No available multicast addresses"))?;

        // Allocate port
        let mut port = None;
        for p in self.port_range.0..=self.port_range.1 {
            if !self.allocated_ports.contains_key(&p) {
                port = Some(p);
                break;
            }
        }

        let port = port.ok_or_else(|| anyhow::anyhow!("No available ports"))?;

        self.allocated_addresses.insert(address, port);
        self.allocated_ports.insert(port, true);

        Ok((address, port))
    }

    fn deallocate_address(&mut self, address: Ipv4Addr) {
        if let Some(port) = self.allocated_addresses.remove(&address) {
            self.allocated_ports.remove(&port);
        }
    }
}

impl PacketSender {
    fn new(sockets: Arc<RwLock<HashMap<Ipv4Addr, Arc<UdpSocket>>>>, config: MulticastConfig) -> Self {
        Self { sockets, config }
    }

    async fn send_packet(&self, group: &MulticastGroup, packet: &MulticastPacket) -> Result<()> {
        let sockets = self.sockets.read().await;
        let socket = sockets.get(&group.multicast_address)
            .ok_or_else(|| anyhow::anyhow!("No socket for multicast address {}", group.multicast_address))?;

        let dest_addr = SocketAddr::new(IpAddr::V4(group.multicast_address), group.port);
        
        // Create UDP packet with custom header
        let mut packet_data = Vec::new();
        
        // Custom packet header (16 bytes)
        packet_data.extend_from_slice(&packet.sequence_number.to_be_bytes());  // 4 bytes
        packet_data.extend_from_slice(&packet.timestamp.to_be_bytes());       // 4 bytes
        packet_data.push(packet.payload_type as u8);                          // 1 byte
        packet_data.extend_from_slice(&[0u8; 7]);                            // 7 bytes padding
        
        // Payload
        packet_data.extend_from_slice(&packet.data);

        socket.send_to(&packet_data, dest_addr).await
            .context("Failed to send multicast packet")?;

        debug!("Sent multicast packet to {} (seq: {}, size: {})", 
               dest_addr, packet.sequence_number, packet_data.len());

        Ok(())
    }
}

impl MulticastManager {
    pub async fn new(config: MulticastConfig) -> Result<Self> {
        let address_allocator = Arc::new(Mutex::new(AddressAllocator::new(
            config.base_address,
            config.port_range,
        )));

        let sockets = Arc::new(RwLock::new(HashMap::new()));
        let packet_sender = Arc::new(PacketSender::new(sockets.clone(), config.clone()));

        Ok(Self {
            config,
            groups: Arc::new(RwLock::new(HashMap::new())),
            sockets,
            address_allocator,
            packet_sender,
        })
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting multicast manager");

        // Start cleanup task
        let groups = self.groups.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            Self::cleanup_task(groups, config).await;
        });

        // Start IGMP management if enabled
        if self.config.enable_igmp {
            let groups = self.groups.clone();
            tokio::spawn(async move {
                Self::igmp_management_task(groups).await;
            });
        }

        info!("Multicast manager started successfully");
        Ok(())
    }

    pub async fn create_group(&self, stream_key: String) -> Result<(Ipv4Addr, u16)> {
        let (address, port) = {
            let mut allocator = self.address_allocator.lock().await;
            allocator.allocate_address()?
        };

        // Create UDP socket for this multicast group
        let socket = self.create_multicast_socket(address, port).await?;

        {
            let mut sockets = self.sockets.write().await;
            sockets.insert(address, Arc::new(socket));
        }

        let group = MulticastGroup {
            id: Uuid::new_v4(),
            stream_key: stream_key.clone(),
            multicast_address: address,
            port,
            created_at: Instant::now(),
            last_activity: Instant::now(),
            viewer_count: 0,
            packet_count: 0,
            bytes_sent: 0,
            viewers: HashMap::new(),
        };

        {
            let mut groups = self.groups.write().await;
            groups.insert(stream_key.clone(), group);
        }

        info!("Created multicast group for stream '{}' at {}:{}", 
              stream_key, address, port);

        Ok((address, port))
    }

    pub async fn remove_group(&self, stream_key: &str) -> Result<()> {
        let (address, _) = {
            let mut groups = self.groups.write().await;
            if let Some(group) = groups.remove(stream_key) {
                (group.multicast_address, group.port)
            } else {
                return Ok(()); // Group doesn't exist
            }
        };

        // Remove socket
        {
            let mut sockets = self.sockets.write().await;
            sockets.remove(&address);
        }

        // Deallocate address
        {
            let mut allocator = self.address_allocator.lock().await;
            allocator.deallocate_address(address);
        }

        info!("Removed multicast group for stream '{}'", stream_key);
        Ok(())
    }

    pub async fn send_packet(&self, packet: MulticastPacket) -> Result<()> {
        let group = {
            let mut groups = self.groups.write().await;
            if let Some(group) = groups.get_mut(&packet.stream_key) {
                group.last_activity = Instant::now();
                group.packet_count += 1;
                group.bytes_sent += packet.data.len() as u64;
                group.clone()
            } else {
                return Err(anyhow::anyhow!("Multicast group not found for stream: {}", packet.stream_key));
            }
        };

        self.packet_sender.send_packet(&group, &packet).await?;
        Ok(())
    }

    pub async fn add_viewer(&self, stream_key: &str, viewer_addr: SocketAddr, quality_profile: String) -> Result<Uuid> {
        let viewer_id = Uuid::new_v4();
        let viewer_info = ViewerInfo {
            id: viewer_id,
            joined_at: Instant::now(),
            last_seen: Instant::now(),
            packets_received: 0,
            quality_profile,
        };

        {
            let mut groups = self.groups.write().await;
            if let Some(group) = groups.get_mut(stream_key) {
                group.viewers.insert(viewer_addr, viewer_info);
                group.viewer_count = group.viewers.len();
                info!("Added viewer {} to multicast group '{}'", viewer_addr, stream_key);
            } else {
                return Err(anyhow::anyhow!("Multicast group not found for stream: {}", stream_key));
            }
        }

        Ok(viewer_id)
    }

    pub async fn remove_viewer(&self, stream_key: &str, viewer_addr: SocketAddr) -> Result<()> {
        {
            let mut groups = self.groups.write().await;
            if let Some(group) = groups.get_mut(stream_key) {
                group.viewers.remove(&viewer_addr);
                group.viewer_count = group.viewers.len();
                info!("Removed viewer {} from multicast group '{}'", viewer_addr, stream_key);
            }
        }
        Ok(())
    }

    pub async fn get_group_info(&self, stream_key: &str) -> Option<MulticastGroup> {
        let groups = self.groups.read().await;
        groups.get(stream_key).cloned()
    }

    pub async fn get_all_groups(&self) -> HashMap<String, MulticastGroup> {
        let groups = self.groups.read().await;
        groups.clone()
    }

    pub async fn update_viewer_last_seen(&self, stream_key: &str, viewer_addr: SocketAddr) -> Result<()> {
        let mut groups = self.groups.write().await;
        if let Some(group) = groups.get_mut(stream_key) {
            if let Some(viewer) = group.viewers.get_mut(&viewer_addr) {
                viewer.last_seen = Instant::now();
                viewer.packets_received += 1;
            }
        }
        Ok(())
    }

    async fn create_multicast_socket(&self, address: Ipv4Addr, port: u16) -> Result<UdpSocket> {
        let bind_addr = SocketAddr::new(self.config.interface, 0);
        let socket = UdpSocket::bind(bind_addr).await
            .context("Failed to bind multicast socket")?;

        // Set multicast options
        let socket_ref = socket2::Socket::from(std::os::unix::io::AsRawFd::as_raw_fd(&socket));
        
        // Set multicast TTL
        socket_ref.set_multicast_ttl_v4(self.config.ttl)
            .context("Failed to set multicast TTL")?;

        // Set multicast interface
        if let IpAddr::V4(interface) = self.config.interface {
            socket_ref.set_multicast_if_v4(&interface)
                .context("Failed to set multicast interface")?;
        }

        // Enable broadcast
        socket_ref.set_broadcast(true)
            .context("Failed to enable broadcast")?;

        // Set send buffer size
        socket_ref.set_send_buffer_size(self.config.buffer_size)
            .context("Failed to set send buffer size")?;

        // Join multicast group
        if let IpAddr::V4(interface) = self.config.interface {
            socket_ref.join_multicast_v4(&address, &interface)
                .context("Failed to join multicast group")?;
        }

        info!("Created multicast socket for {}:{} on interface {}", 
              address, port, self.config.interface);

        Ok(socket)
    }

    async fn cleanup_task(
        groups: Arc<RwLock<HashMap<String, MulticastGroup>>>,
        config: MulticastConfig,
    ) {
        let mut cleanup_interval = interval(Duration::from_secs(config.group_timeout));

        loop {
            cleanup_interval.tick().await;

            let now = Instant::now();
            let timeout_duration = Duration::from_secs(config.group_timeout);

            let mut groups_to_remove = Vec::new();
            let mut viewers_to_remove = Vec::new();

            {
                let mut groups_map = groups.write().await;
                
                for (stream_key, group) in groups_map.iter_mut() {
                    // Remove inactive viewers
                    let mut inactive_viewers = Vec::new();
                    for (addr, viewer) in &group.viewers {
                        if now.duration_since(viewer.last_seen) > timeout_duration {
                            inactive_viewers.push(*addr);
                        }
                    }

                    for addr in inactive_viewers {
                        group.viewers.remove(&addr);
                        viewers_to_remove.push((stream_key.clone(), addr));
                    }

                    group.viewer_count = group.viewers.len();

                    // Mark groups for removal if inactive and no viewers
                    if group.viewers.is_empty() && 
                       now.duration_since(group.last_activity) > timeout_duration {
                        groups_to_remove.push(stream_key.clone());
                    }
                }

                // Remove inactive groups
                for stream_key in &groups_to_remove {
                    groups_map.remove(stream_key);
                }
            }

            if !groups_to_remove.is_empty() {
                info!("Cleaned up {} inactive multicast groups", groups_to_remove.len());
            }

            if !viewers_to_remove.is_empty() {
                info!("Removed {} inactive viewers", viewers_to_remove.len());
            }
        }
    }

    async fn igmp_management_task(groups: Arc<RwLock<HashMap<String, MulticastGroup>>>) {
        let mut igmp_interval = interval(Duration::from_secs(30));

        loop {
            igmp_interval.tick().await;

            let groups_map = groups.read().await;
            for group in groups_map.values() {
                // Send IGMP membership reports to maintain group membership
                debug!("Maintaining IGMP membership for group {}", group.multicast_address);
                // Implementation would send IGMP packets here
            }
        }
    }

    pub async fn get_stats(&self) -> MulticastStats {
        let groups = self.groups.read().await;
        
        let total_groups = groups.len();
        let total_viewers = groups.values().map(|g| g.viewer_count).sum();
        let total_packets = groups.values().map(|g| g.packet_count).sum();
        let total_bytes = groups.values().map(|g| g.bytes_sent).sum();

        MulticastStats {
            active_groups: total_groups,
            total_viewers,
            packets_sent: total_packets,
            bytes_transmitted: total_bytes,
            uptime: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MulticastStats {
    pub active_groups: usize,
    pub total_viewers: usize,
    pub packets_sent: u64,
    pub bytes_transmitted: u64,
    pub uptime: u64,
}