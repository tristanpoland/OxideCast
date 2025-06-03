# 🎥 Oxide Streaming Server

A production-ready, high-performance media streaming server built in Rust with multicast distribution capabilities. This server efficiently handles thousands of concurrent viewers by sending each packet only once via multicast, dramatically reducing bandwidth usage compared to traditional unicast streaming.

## ✨ Features

### 🌐 Core Streaming Capabilities
- **Multicast Distribution**: Send each packet once to thousands of viewers
- **RTMP Ingestion**: Accept streams from OBS, FFmpeg, and other encoders
- **Multiple Output Formats**: HLS, WebRTC, and direct multicast UDP
- **Real-time Transcoding**: Hardware-accelerated transcoding with FFmpeg
- **Adaptive Bitrate**: Multiple quality profiles with automatic switching

### ⚡ Performance & Scalability
- **High Concurrency**: Handle 10,000+ concurrent viewers
- **Low Latency**: Sub-second latency with WebRTC and optimized multicast
- **Resource Efficient**: Built in Rust for maximum performance
- **Horizontal Scaling**: Stateless design with external data storage

### 🔐 Security & Authentication
- **JWT Authentication**: Secure API access with role-based permissions
- **Stream Key Management**: Unique keys for each stream with rotation
- **Rate Limiting**: Protection against abuse and DDoS attacks
- **CORS Support**: Secure cross-origin access for web players

### 📊 Monitoring & Analytics
- **Real-time Metrics**: Stream quality, viewer counts, and system health
- **Prometheus Integration**: Comprehensive metrics collection
- **Grafana Dashboards**: Beautiful visualization and alerting
- **Database Analytics**: Historical data and viewer behavior tracking

## 🚀 Quick Start

### Prerequisites

- Docker and Docker Compose
- 8GB+ RAM recommended
- Linux host with multicast networking support
- FFmpeg installed (for transcoding)

### 1. Clone and Setup

```bash
git clone https://github.com/tristanpoland/OxideCast.git
cd multicast-streaming-server

# Copy and customize configuration
cp config.toml.example config.toml
cp docker-compose.yml.example docker-compose.yml

# Edit configuration files
nano config.toml
```

### 2. Start the Stack

```bash
# Start all services
docker-compose up -d

# Check service health
docker-compose ps

# View logs
docker-compose logs -f streaming-server
```

### 3. Verify Installation

```bash
# Check API health
curl http://localhost:8080/health

# Check web interface
open http://localhost:8080
```

## 📋 Configuration

### Core Configuration (`config.toml`)

```toml
[server]
host = "0.0.0.0"
port = 8080
secret_key = "your-secure-secret-key"

[database]
url = "postgresql://user:pass@localhost:5432/streaming"

[multicast]
interface = "0.0.0.0"
base_address = "239.255.0.1"
port_range = [30000, 40000]

[transcoding]
ffmpeg_path = "/usr/bin/ffmpeg"
max_concurrent = 10

# Hardware acceleration (optional)
[transcoding.hardware_acceleration]
enabled = true
device = "cuda"  # or vaapi, qsv, videotoolbox
```

### Quality Profiles

Define multiple encoding profiles for adaptive streaming:

```toml
[[transcoding.profiles]]
name = "1080p"
width = 1920
height = 1080
bitrate = 5000000
framerate = 30.0

[[transcoding.profiles]]
name = "720p"
width = 1280
height = 720
bitrate = 2500000
framerate = 30.0
```

## 🎬 Usage

### Streaming with OBS

1. **Get your stream key**:
   ```bash
   curl -X POST http://localhost:8080/api/v1/auth/login \
     -H "Content-Type: application/json" \
     -d '{"username": "your_username", "password": "your_password"}'
   ```

2. **Configure OBS**:
   - Server: `rtmp://your-server-ip:1935/live`
   - Stream Key: `your-stream-key-from-api`

3. **Start streaming** in OBS

### Viewing Streams

#### Web Player
Open `http://localhost:8080/player` in your browser to see available streams.

#### Direct URLs

**HLS (HTTP Live Streaming)**:
```
http://localhost:8080/api/v1/hls/{stream_key}/playlist.m3u8
```

**Multicast UDP**:
```
udp://{multicast_address}:{port}
```

Use with VLC, FFplay, or any compatible player:
```bash
# VLC
vlc udp://@239.255.0.1:30000

# FFplay
ffplay udp://239.255.0.1:30000

# With HLS
vlc http://localhost:8080/api/v1/hls/stream123/playlist.m3u8
```

## 🔧 API Reference

### Authentication

**Login**:
```bash
POST /api/v1/auth/login
{
  "username": "user",
  "password": "pass"
}
```

**Create Stream**:
```bash
POST /api/v1/streams
Authorization: Bearer <token>
{
  "title": "My Stream",
  "description": "Stream description",
  "transcoding_profiles": ["720p", "480p"]
}
```

### Stream Management

**List Streams**:
```bash
GET /api/v1/streams?page=1&limit=20
```

**Get Stream Details**:
```bash
GET /api/v1/streams/{stream_id}
```

**Start/Stop Stream**:
```bash
POST /api/v1/streams/{stream_id}/start
POST /api/v1/streams/{stream_id}/stop
```

### Viewer Management

**Join Stream**:
```bash
POST /api/v1/streams/{stream_id}/join
{
  "connection_type": "hls",
  "quality_profile": "720p"
}
```

**Get Stream URL**:
```bash
GET /api/v1/streams/{stream_id}/url?connection_type=hls
```

### Statistics

**Server Stats**:
```bash
GET /api/v1/stats
Authorization: Bearer <admin_token>
```

**Stream Stats**:
```bash
GET /api/v1/streams/{stream_id}/stats
Authorization: Bearer <token>
```

## 🏗️ Architecture

### System Components

```
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   RTMP Ingest   │────│  Stream Manager  │────│   Multicast     │
│   (Port 1935)   │    │                  │    │  Distribution   │
└─────────────────┘    └──────────────────┘    └─────────────────┘
                              │                          │
                              ▼                          ▼
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   Transcoder    │    │    Database      │    │    Viewers      │
│   (FFmpeg)      │    │  (PostgreSQL)    │    │  (Multicast)    │
└─────────────────┘    └──────────────────┘    └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   HLS Server    │    │   Web Server     │    │   WebRTC        │
│                 │    │   (Rocket)       │    │   Server        │
└─────────────────┘    └──────────────────┘    └─────────────────┘
```

### Data Flow

1. **Ingest**: RTMP streams received on port 1935
2. **Transcode**: FFmpeg converts to multiple quality profiles
3. **Distribute**: Stream data sent via multicast UDP
4. **Serve**: HLS segments and WebRTC streams served via HTTP API
5. **Monitor**: Metrics collected and stored for analytics

## 🔧 Development

### Building from Source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install system dependencies (Ubuntu/Debian)
sudo apt update && sudo apt install -y \
  pkg-config libssl-dev libpq-dev cmake \
  build-essential ffmpeg

# Clone and build
git clone https://github.com/your-repo/multicast-streaming-server.git
cd multicast-streaming-server
cargo build --release

# Run migrations
export DATABASE_URL="postgresql://user:pass@localhost/streaming"
cargo install sqlx-cli
sqlx migrate run

# Start the server
./target/release/multicast-streaming-server --config config.toml
```

### Running Tests

```bash
# Unit tests
cargo test

# Integration tests
cargo test --test integration

# Load tests (requires additional setup)
cargo test --test load_test --release
```

### Development with Docker

```bash
# Development environment
docker-compose -f docker-compose.dev.yml up

# Hot reload (requires cargo-watch)
cargo install cargo-watch
cargo watch -x run
```

## 📊 Monitoring

### Prometheus Metrics

The server exposes metrics at `/metrics`:

- `streaming_active_streams`: Number of active streams
- `streaming_total_viewers`: Total viewer count across all streams
- `streaming_multicast_packets_sent`: Multicast packets transmitted
- `streaming_transcoding_jobs_active`: Active transcoding jobs
- `streaming_http_requests_total`: HTTP API request count

### Grafana Dashboards

Pre-configured dashboards available at `http://localhost:3000` (admin/admin):

- **Server Overview**: System health and performance
- **Stream Analytics**: Per-stream metrics and viewer data
- **Network Performance**: Multicast and transcoding performance
- **Viewer Insights**: Geographic and device analytics

### Log Analysis

```bash
# View real-time logs
docker-compose logs -f streaming-server

# Search for errors
docker-compose logs streaming-server | grep ERROR

# Export logs for analysis
docker-compose logs --no-color streaming-server > streaming.log
```

## 🚀 Production Deployment

### Security Checklist

- [ ] Change all default passwords and secrets
- [ ] Enable HTTPS with valid certificates
- [ ] Configure firewall rules (allow 80, 443, 1935, multicast range)
- [ ] Set up regular database backups
- [ ] Configure log rotation
- [ ] Enable monitoring and alerting
- [ ] Restrict API access with authentication
- [ ] Configure rate limiting
- [ ] Set up intrusion detection

### Performance Tuning

**System Configuration**:
```bash
# Increase file descriptor limits
echo "fs.file-max = 1000000" >> /etc/sysctl.conf

# Optimize network buffer sizes
echo "net.core.rmem_max = 268435456" >> /etc/sysctl.conf
echo "net.core.wmem_max = 268435456" >> /etc/sysctl.conf

# Enable multicast
echo "net.ipv4.ip_forward = 1" >> /etc/sysctl.conf

# Apply changes
sysctl -p
```

**Application Configuration**:
```toml
[server]
workers = 8  # Match CPU cores
max_connections = 50000

[multicast]
buffer_size = 2097152  # 2MB

[transcoding]
max_concurrent = 20  # Based on CPU/GPU capacity
```

### Scaling

**Horizontal Scaling**:
- Deploy multiple server instances behind a load balancer
- Use shared PostgreSQL and Redis instances
- Implement session affinity for WebRTC connections
- Consider database read replicas for analytics

**Vertical Scaling**:
- Increase CPU cores for transcoding
- Add GPU acceleration for hardware transcoding
- Increase RAM for concurrent streams
- Use faster storage (NVMe SSD) for segment caching

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch: `git checkout -b feature/amazing-feature`
3. Commit changes: `git commit -m 'Add amazing feature'`
4. Push to branch: `git push origin feature/amazing-feature`
5. Open a Pull Request

### Development Guidelines

- Follow Rust best practices and idioms
- Add tests for new functionality
- Update documentation for API changes
- Ensure Docker builds successfully
- Run `cargo clippy` and `cargo fmt` before committing

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## 🙏 Acknowledgments

- [Rocket](https://rocket.rs/) - Web framework
- [SQLx](https://github.com/launchbadge/sqlx) - Database toolkit
- [Tokio](https://tokio.rs/) - Async runtime
- [FFmpeg](https://ffmpeg.org/) - Media processing
- [WebRTC](https://webrtc.org/) - Real-time communication

## 📞 Support

- 📚 [Documentation](https://docs.your-domain.com)
- 🐛 [Issue Tracker](https://github.com/your-repo/multicast-streaming-server/issues)
- 💬 [Discord Community](https://discord.gg/your-invite)
- 📧 [Email Support](mailto:support@your-domain.com)

---

**Made with ❤️ and 🦀 Rust**