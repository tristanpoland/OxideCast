-- Add migration script here
-- Initial database schema for multicast streaming server

-- Create custom types
CREATE TYPE user_role AS ENUM ('admin', 'streamer', 'viewer');
CREATE TYPE connection_type AS ENUM ('multicast', 'webrtc', 'hls', 'rtmp');

-- Users table
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username VARCHAR(50) UNIQUE NOT NULL,
    email VARCHAR(255) UNIQUE NOT NULL,
    password_hash VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    is_verified BOOLEAN NOT NULL DEFAULT FALSE,
    stream_key VARCHAR(255) UNIQUE,
    role user_role NOT NULL DEFAULT 'viewer'
);

-- Create indexes for users
CREATE INDEX idx_users_username ON users(username);
CREATE INDEX idx_users_email ON users(email);
CREATE INDEX idx_users_stream_key ON users(stream_key);
CREATE INDEX idx_users_role ON users(role);

-- Streams table
CREATE TABLE streams (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title VARCHAR(255) NOT NULL,
    description TEXT,
    category VARCHAR(100),
    is_live BOOLEAN NOT NULL DEFAULT FALSE,
    is_private BOOLEAN NOT NULL DEFAULT FALSE,
    stream_key VARCHAR(255) UNIQUE NOT NULL,
    multicast_address VARCHAR(15), -- IPv4 address
    multicast_port INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    ended_at TIMESTAMPTZ,
    viewer_count INTEGER NOT NULL DEFAULT 0,
    max_viewers INTEGER NOT NULL DEFAULT 0,
    thumbnail_url VARCHAR(500),
    recording_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    transcoding_profiles TEXT[] NOT NULL DEFAULT ARRAY['720p']
);

-- Create indexes for streams
CREATE INDEX idx_streams_user_id ON streams(user_id);
CREATE INDEX idx_streams_stream_key ON streams(stream_key);
CREATE INDEX idx_streams_is_live ON streams(is_live);
CREATE INDEX idx_streams_is_private ON streams(is_private);
CREATE INDEX idx_streams_category ON streams(category);
CREATE INDEX idx_streams_created_at ON streams(created_at);
CREATE INDEX idx_streams_viewer_count ON streams(viewer_count);

-- Stream sessions table (for analytics)
CREATE TABLE stream_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id UUID NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    duration_seconds INTEGER,
    peak_viewers INTEGER NOT NULL DEFAULT 0,
    total_viewers INTEGER NOT NULL DEFAULT 0,
    bytes_transmitted BIGINT NOT NULL DEFAULT 0,
    quality_metrics JSONB
);

-- Create indexes for stream sessions
CREATE INDEX idx_stream_sessions_stream_id ON stream_sessions(stream_id);
CREATE INDEX idx_stream_sessions_started_at ON stream_sessions(started_at);

-- Viewers table
CREATE TABLE viewers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id UUID NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    ip_address INET NOT NULL,
    user_agent TEXT,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    left_at TIMESTAMPTZ,
    watch_time_seconds INTEGER NOT NULL DEFAULT 0,
    quality_profile VARCHAR(50) NOT NULL DEFAULT '720p',
    connection_type connection_type NOT NULL
);

-- Create indexes for viewers
CREATE INDEX idx_viewers_stream_id ON viewers(stream_id);
CREATE INDEX idx_viewers_user_id ON viewers(user_id);
CREATE INDEX idx_viewers_joined_at ON viewers(joined_at);
CREATE INDEX idx_viewers_connection_type ON viewers(connection_type);

-- Stream metrics table (for monitoring)
CREATE TABLE stream_metrics (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    stream_id UUID NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    viewer_count INTEGER NOT NULL,
    bitrate_kbps INTEGER NOT NULL,
    frame_rate REAL NOT NULL,
    dropped_frames INTEGER NOT NULL DEFAULT 0,
    network_throughput BIGINT NOT NULL DEFAULT 0,
    cpu_usage REAL NOT NULL DEFAULT 0.0,
    memory_usage BIGINT NOT NULL DEFAULT 0
);

-- Create indexes for stream metrics
CREATE INDEX idx_stream_metrics_stream_id ON stream_metrics(stream_id);
CREATE INDEX idx_stream_metrics_timestamp ON stream_metrics(timestamp);

-- Partitioning for stream_metrics by timestamp (monthly partitions)
-- This helps with performance as metrics data grows
CREATE TABLE stream_metrics_template (LIKE stream_metrics INCLUDING ALL);

-- Function to create monthly partitions
CREATE OR REPLACE FUNCTION create_monthly_partition(table_name text, start_date date)
RETURNS void AS $$
DECLARE
    partition_name text;
    end_date date;
BEGIN
    partition_name := table_name || '_' || to_char(start_date, 'YYYY_MM');
    end_date := start_date + interval '1 month';
    
    EXECUTE format('
        CREATE TABLE IF NOT EXISTS %I (
            CHECK (timestamp >= %L AND timestamp < %L)
        ) INHERITS (%I)',
        partition_name, start_date, end_date, table_name);
        
    EXECUTE format('
        CREATE INDEX IF NOT EXISTS %I ON %I (stream_id, timestamp)',
        partition_name || '_stream_timestamp', partition_name);
END;
$$ LANGUAGE plpgsql;

-- Create partition for current month
SELECT create_monthly_partition('stream_metrics', date_trunc('month', CURRENT_DATE));

-- Function to automatically update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Create triggers for updated_at
CREATE TRIGGER update_users_updated_at 
    BEFORE UPDATE ON users 
    FOR EACH ROW 
    EXECUTE FUNCTION update_updated_at_column();

CREATE TRIGGER update_streams_updated_at 
    BEFORE UPDATE ON streams 
    FOR EACH ROW 
    EXECUTE FUNCTION update_updated_at_column();

-- Function to clean up old metrics (keep last 3 months)
CREATE OR REPLACE FUNCTION cleanup_old_metrics()
RETURNS void AS $$
BEGIN
    DELETE FROM stream_metrics 
    WHERE timestamp < NOW() - INTERVAL '3 months';
END;
$$ LANGUAGE plpgsql;

-- Function to automatically end viewer sessions when stream ends
CREATE OR REPLACE FUNCTION end_viewer_sessions_on_stream_end()
RETURNS TRIGGER AS $$
BEGIN
    IF OLD.is_live = TRUE AND NEW.is_live = FALSE THEN
        UPDATE viewers 
        SET left_at = NOW(),
            watch_time_seconds = EXTRACT(EPOCH FROM (NOW() - joined_at))::INTEGER
        WHERE stream_id = NEW.id AND left_at IS NULL;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Create trigger for ending viewer sessions
CREATE TRIGGER end_viewer_sessions_trigger
    AFTER UPDATE ON streams
    FOR EACH ROW
    EXECUTE FUNCTION end_viewer_sessions_on_stream_end();

-- Create some helpful views
CREATE VIEW active_streams AS
SELECT 
    s.*,
    u.username as streamer_username,
    EXTRACT(EPOCH FROM (NOW() - s.started_at))::INTEGER as duration_seconds
FROM streams s
JOIN users u ON s.user_id = u.id
WHERE s.is_live = TRUE;

CREATE VIEW stream_analytics AS
SELECT 
    s.id,
    s.title,
    s.user_id,
    u.username as streamer_username,
    s.created_at,
    s.is_live,
    s.viewer_count,
    s.max_viewers,
    COALESCE(ss.peak_viewers, 0) as session_peak_viewers,
    COALESCE(ss.total_viewers, 0) as session_total_viewers,
    COALESCE(ss.duration_seconds, 0) as session_duration_seconds,
    COALESCE(ss.bytes_transmitted, 0) as session_bytes_transmitted
FROM streams s
JOIN users u ON s.user_id = u.id
LEFT JOIN stream_sessions ss ON s.id = ss.stream_id;

-- Insert default admin user (password: 'admin123' hashed with bcrypt cost 12)
INSERT INTO users (username, email, password_hash, role, is_active, is_verified)
VALUES (
    'admin',
    'admin@example.com',
    '$2b$12$LQv3c1yqBWVHxkd0LHAkCOYz6TtxMQJqhN8/LeQ9keFjXdFfwHZAS',
    'admin',
    TRUE,
    TRUE
);

-- Create indexes for performance
CREATE INDEX CONCURRENTLY idx_streams_live_public ON streams(is_live, is_private, viewer_count DESC) 
WHERE is_live = TRUE AND is_private = FALSE;

CREATE INDEX CONCURRENTLY idx_viewers_active ON viewers(stream_id, joined_at) 
WHERE left_at IS NULL;

CREATE INDEX CONCURRENTLY idx_stream_metrics_recent ON stream_metrics(stream_id, timestamp DESC) 
WHERE timestamp > NOW() - INTERVAL '24 hours';

-- Add constraints
ALTER TABLE streams ADD CONSTRAINT check_viewer_count_positive CHECK (viewer_count >= 0);
ALTER TABLE streams ADD CONSTRAINT check_max_viewers_positive CHECK (max_viewers >= 0);
ALTER TABLE streams ADD CONSTRAINT check_multicast_port_range CHECK (multicast_port IS NULL OR (multicast_port >= 1024 AND multicast_port <= 65535));
ALTER TABLE viewers ADD CONSTRAINT check_watch_time_positive CHECK (watch_time_seconds >= 0);
ALTER TABLE stream_metrics ADD CONSTRAINT check_metrics_positive CHECK (
    viewer_count >= 0 AND 
    bitrate_kbps >= 0 AND 
    frame_rate >= 0 AND 
    dropped_frames >= 0 AND 
    network_throughput >= 0 AND 
    cpu_usage >= 0 AND 
    memory_usage >= 0
);