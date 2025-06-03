# Multi-stage build for Rust application with FFmpeg
FROM rust:1.75-bullseye AS builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libpq-dev \
    cmake \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Build dependencies (this will be cached unless Cargo.toml changes)
RUN cargo build --release

# Remove dummy source
RUN rm -rf src

# Copy source code
COPY src ./src
COPY migrations ./migrations
COPY static ./static

# Build the application
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bullseye-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl1.1 \
    libpq5 \
    ffmpeg \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd -m -u 1000 streaming && \
    mkdir -p /app /data/streaming /tmp/streaming && \
    chown -R streaming:streaming /app /data/streaming /tmp/streaming

# Copy the binary from builder stage
COPY --from=builder /app/target/release/multicast-streaming-server /app/
COPY --from=builder /app/migrations /app/migrations
COPY --from=builder /app/static /app/static

# Copy configuration files
COPY config.toml /app/
COPY Rocket.toml /app/

# Set proper permissions
RUN chown -R streaming:streaming /app

# Switch to app user
USER streaming

# Set working directory
WORKDIR /app

# Expose ports
EXPOSE 8080 1935 30000-40000/udp

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Set environment variables
ENV RUST_LOG=info
ENV ROCKET_PROFILE=production

# Run the application
CMD ["./multicast-streaming-server", "--config", "config.toml"]