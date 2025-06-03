use anyhow::Result;
use chrono::{DateTime, Utc};
use rocket_db_pools::{sqlx, Database};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Database)]
#[database("postgres")]
pub struct Db(PgPool);

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    pub is_verified: bool,
    pub stream_key: Option<String>,
    pub role: UserRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Streamer,
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Stream {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_live: bool,
    pub is_private: bool,
    pub stream_key: String,
    pub multicast_address: Option<String>,
    pub multicast_port: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub viewer_count: i32,
    pub max_viewers: i32,
    pub thumbnail_url: Option<String>,
    pub recording_enabled: bool,
    pub transcoding_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct StreamSession {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: Option<i32>,
    pub peak_viewers: i32,
    pub total_viewers: i32,
    pub bytes_transmitted: i64,
    pub quality_metrics: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Viewer {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub user_id: Option<Uuid>,
    pub ip_address: String,
    pub user_agent: Option<String>,
    pub joined_at: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
    pub watch_time_seconds: i32,
    pub quality_profile: String,
    pub connection_type: ConnectionType,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "connection_type", rename_all = "lowercase")]
pub enum ConnectionType {
    Multicast,
    Webrtc,
    Hls,
    Rtmp,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct StreamMetrics {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub viewer_count: i32,
    pub bitrate_kbps: i32,
    pub frame_rate: f32,
    pub dropped_frames: i32,
    pub network_throughput: i64,
    pub cpu_usage: f32,
    pub memory_usage: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateStreamRequest {
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_private: bool,
    pub recording_enabled: bool,
    pub transcoding_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateStreamRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub is_private: Option<bool>,
    pub recording_enabled: Option<bool>,
    pub transcoding_profiles: Option<Vec<String>>,
}

pub async fn setup_database(database_url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(database_url).await?;
    
    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;
    
    Ok(pool)
}

impl User {
    pub async fn create(pool: &PgPool, req: CreateUserRequest) -> Result<Self> {
        let id = Uuid::new_v4();
        let stream_key = Uuid::new_v4().to_string();
        let now = Utc::now();
        
        let user = sqlx::query_as::<_, User>(
            r#"
            INSERT INTO users (id, username, email, password_hash, created_at, updated_at, 
                             is_active, is_verified, stream_key, role)
            VALUES ($1, $2, $3, $4, $5, $6, true, false, $7, $8)
            RETURNING *
            "#
        )
        .bind(id)
        .bind(&req.username)
        .bind(&req.email)
        .bind(&req.password) // This should be hashed before calling
        .bind(now)
        .bind(now)
        .bind(stream_key)
        .bind(&req.role)
        .fetch_one(pool)
        .await?;
        
        Ok(user)
    }

    pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Self>> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        
        Ok(user)
    }

    pub async fn find_by_username(pool: &PgPool, username: &str) -> Result<Option<Self>> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1")
            .bind(username)
            .fetch_optional(pool)
            .await?;
        
        Ok(user)
    }

    pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<Self>> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(pool)
            .await?;
        
        Ok(user)
    }

    pub async fn find_by_stream_key(pool: &PgPool, stream_key: &str) -> Result<Option<Self>> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE stream_key = $1")
            .bind(stream_key)
            .fetch_optional(pool)
            .await?;
        
        Ok(user)
    }

    pub async fn update_stream_key(&mut self, pool: &PgPool) -> Result<()> {
        let new_key = Uuid::new_v4().to_string();
        
        sqlx::query("UPDATE users SET stream_key = $1, updated_at = $2 WHERE id = $3")
            .bind(&new_key)
            .bind(Utc::now())
            .bind(self.id)
            .execute(pool)
            .await?;
        
        self.stream_key = Some(new_key);
        Ok(())
    }
}

impl Stream {
    pub async fn create(pool: &PgPool, user_id: Uuid, req: CreateStreamRequest) -> Result<Self> {
        let id = Uuid::new_v4();
        let stream_key = Uuid::new_v4().to_string();
        let now = Utc::now();
        
        let stream = sqlx::query_as::<_, Stream>(
            r#"
            INSERT INTO streams (id, user_id, title, description, category, is_live, is_private,
                               stream_key, created_at, updated_at, viewer_count, max_viewers,
                               recording_enabled, transcoding_profiles)
            VALUES ($1, $2, $3, $4, $5, false, $6, $7, $8, $9, 0, 0, $10, $11)
            RETURNING *
            "#
        )
        .bind(id)
        .bind(user_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(&req.category)
        .bind(req.is_private)
        .bind(stream_key)
        .bind(now)
        .bind(now)
        .bind(req.recording_enabled)
        .bind(&req.transcoding_profiles)
        .fetch_one(pool)
        .await?;
        
        Ok(stream)
    }

    pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Self>> {
        let stream = sqlx::query_as::<_, Stream>("SELECT * FROM streams WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        
        Ok(stream)
    }

    pub async fn find_by_stream_key(pool: &PgPool, stream_key: &str) -> Result<Option<Self>> {
        let stream = sqlx::query_as::<_, Stream>("SELECT * FROM streams WHERE stream_key = $1")
            .bind(stream_key)
            .fetch_optional(pool)
            .await?;
        
        Ok(stream)
    }

    pub async fn find_by_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<Self>> {
        let streams = sqlx::query_as::<_, Stream>(
            "SELECT * FROM streams WHERE user_id = $1 ORDER BY created_at DESC"
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?;
        
        Ok(streams)
    }

    pub async fn find_live_streams(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<Self>> {
        let streams = sqlx::query_as::<_, Stream>(
            r#"
            SELECT * FROM streams 
            WHERE is_live = true AND is_private = false 
            ORDER BY viewer_count DESC, started_at DESC
            LIMIT $1 OFFSET $2
            "#
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        
        Ok(streams)
    }

    pub async fn update(&mut self, pool: &PgPool, req: UpdateStreamRequest) -> Result<()> {
        if let Some(title) = req.title {
            self.title = title;
        }
        if let Some(description) = req.description {
            self.description = Some(description);
        }
        if let Some(category) = req.category {
            self.category = Some(category);
        }
        if let Some(is_private) = req.is_private {
            self.is_private = is_private;
        }
        if let Some(recording_enabled) = req.recording_enabled {
            self.recording_enabled = recording_enabled;
        }
        if let Some(transcoding_profiles) = req.transcoding_profiles {
            self.transcoding_profiles = transcoding_profiles;
        }
        
        self.updated_at = Utc::now();
        
        sqlx::query(
            r#"
            UPDATE streams 
            SET title = $1, description = $2, category = $3, is_private = $4, 
                recording_enabled = $5, transcoding_profiles = $6, updated_at = $7
            WHERE id = $8
            "#
        )
        .bind(&self.title)
        .bind(&self.description)
        .bind(&self.category)
        .bind(self.is_private)
        .bind(self.recording_enabled)
        .bind(&self.transcoding_profiles)
        .bind(self.updated_at)
        .bind(self.id)
        .execute(pool)
        .await?;
        
        Ok(())
    }

    pub async fn start_streaming(&mut self, pool: &PgPool, multicast_addr: &str, multicast_port: i32) -> Result<()> {
        self.is_live = true;
        self.started_at = Some(Utc::now());
        self.multicast_address = Some(multicast_addr.to_string());
        self.multicast_port = Some(multicast_port);
        
        sqlx::query(
            r#"
            UPDATE streams 
            SET is_live = true, started_at = $1, multicast_address = $2, multicast_port = $3
            WHERE id = $4
            "#
        )
        .bind(self.started_at)
        .bind(&self.multicast_address)
        .bind(self.multicast_port)
        .bind(self.id)
        .execute(pool)
        .await?;
        
        Ok(())
    }

    pub async fn stop_streaming(&mut self, pool: &PgPool) -> Result<()> {
        self.is_live = false;
        self.ended_at = Some(Utc::now());
        
        sqlx::query(
            "UPDATE streams SET is_live = false, ended_at = $1 WHERE id = $2"
        )
        .bind(self.ended_at)
        .bind(self.id)
        .execute(pool)
        .await?;
        
        Ok(())
    }

    pub async fn update_viewer_count(&mut self, pool: &PgPool, count: i32) -> Result<()> {
        self.viewer_count = count;
        if count > self.max_viewers {
            self.max_viewers = count;
        }
        
        sqlx::query(
            "UPDATE streams SET viewer_count = $1, max_viewers = $2 WHERE id = $3"
        )
        .bind(self.viewer_count)
        .bind(self.max_viewers)
        .bind(self.id)
        .execute(pool)
        .await?;
        
        Ok(())
    }
}

impl StreamMetrics {
    pub async fn record(pool: &PgPool, stream_id: Uuid, 
                       viewer_count: i32, bitrate_kbps: i32, frame_rate: f32,
                       dropped_frames: i32, throughput: i64, cpu_usage: f32, 
                       memory_usage: i64) -> Result<()> {
        let id = Uuid::new_v4();
        
        sqlx::query(
            r#"
            INSERT INTO stream_metrics (id, stream_id, timestamp, viewer_count, bitrate_kbps,
                                      frame_rate, dropped_frames, network_throughput, 
                                      cpu_usage, memory_usage)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#
        )
        .bind(id)
        .bind(stream_id)
        .bind(Utc::now())
        .bind(viewer_count)
        .bind(bitrate_kbps)
        .bind(frame_rate)
        .bind(dropped_frames)
        .bind(throughput)
        .bind(cpu_usage)
        .bind(memory_usage)
        .execute(pool)
        .await?;
        
        Ok(())
    }
}