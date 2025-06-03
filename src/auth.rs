use anyhow::{Result, Context};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::State;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::config::AuthConfig;
use crate::database::{User, UserRole};

#[derive(Debug, Clone)]
pub struct AuthManager {
    config: AuthConfig,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String, // user_id
    pub username: String,
    pub role: UserRole,
    pub exp: i64,
    pub iat: i64,
    pub token_type: TokenType,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum TokenType {
    Access,
    Refresh,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub user: UserInfo,
    pub expires_in: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub role: UserRole,
    pub stream_key: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
}

pub struct AuthenticatedUser {
    pub id: Uuid,
    pub username: String,
    pub role: UserRole,
}

impl AuthManager {
    pub fn new(config: AuthConfig) -> Self {
        let encoding_key = EncodingKey::from_secret(config.jwt_secret.as_ref());
        let decoding_key = DecodingKey::from_secret(config.jwt_secret.as_ref());

        Self {
            config,
            encoding_key,
            decoding_key,
        }
    }

    pub fn generate_tokens(&self, user: &User) -> Result<(String, String)> {
        let now = Utc::now();
        
        // Access token
        let access_claims = Claims {
            sub: user.id.to_string(),
            username: user.username.clone(),
            role: user.role.clone(),
            exp: (now + Duration::seconds(self.config.jwt_expiry as i64)).timestamp(),
            iat: now.timestamp(),
            token_type: TokenType::Access,
        };

        let access_token = encode(&Header::default(), &access_claims, &self.encoding_key)
            .context("Failed to encode access token")?;

        // Refresh token
        let refresh_claims = Claims {
            sub: user.id.to_string(),
            username: user.username.clone(),
            role: user.role.clone(),
            exp: (now + Duration::seconds(self.config.refresh_expiry as i64)).timestamp(),
            iat: now.timestamp(),
            token_type: TokenType::Refresh,
        };

        let refresh_token = encode(&Header::default(), &refresh_claims, &self.encoding_key)
            .context("Failed to encode refresh token")?;

        Ok((access_token, refresh_token))
    }

    pub fn verify_token(&self, token: &str) -> Result<Claims> {
        let validation = Validation::new(Algorithm::HS256);
        
        let token_data = decode::<Claims>(token, &self.decoding_key, &validation)
            .context("Failed to decode token")?;

        // Check if token is expired
        let now = Utc::now().timestamp();
        if token_data.claims.exp < now {
            anyhow::bail!("Token has expired");
        }

        Ok(token_data.claims)
    }

    pub fn hash_password(&self, password: &str) -> Result<String> {
        bcrypt::hash(password, self.config.bcrypt_cost)
            .context("Failed to hash password")
    }

    pub fn verify_password(&self, password: &str, hash: &str) -> Result<bool> {
        bcrypt::verify(password, hash)
            .context("Failed to verify password")
    }

    pub fn create_login_response(&self, user: &User, access_token: String, refresh_token: String) -> LoginResponse {
        LoginResponse {
            access_token,
            refresh_token,
            user: UserInfo {
                id: user.id,
                username: user.username.clone(),
                email: user.email.clone(),
                role: user.role.clone(),
                stream_key: user.stream_key.clone(),
                created_at: user.created_at,
            },
            expires_in: self.config.jwt_expiry,
        }
    }

    pub fn extract_token_from_header(&self, authorization: &str) -> Option<&str> {
        if authorization.starts_with("Bearer ") {
            Some(&authorization[7..])
        } else {
            None
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthenticatedUser {
    type Error = AuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let auth_manager = match req.guard::<&State<Arc<crate::AppState>>>().await {
            Outcome::Success(state) => &state.auth_manager,
            Outcome::Error(e) => return Outcome::Error(e),
            Outcome::Forward(f) => return Outcome::Forward(f),
        };

        let authorization = match req.headers().get_one("Authorization") {
            Some(auth) => auth,
            None => return Outcome::Error((Status::Unauthorized, AuthError::MissingToken)),
        };

        let token = match auth_manager.extract_token_from_header(authorization) {
            Some(token) => token,
            None => return Outcome::Error((Status::Unauthorized, AuthError::InvalidFormat)),
        };

        let claims = match auth_manager.verify_token(token) {
            Ok(claims) => claims,
            Err(_) => return Outcome::Error((Status::Unauthorized, AuthError::InvalidToken)),
        };

        // Ensure this is an access token
        match claims.token_type {
            TokenType::Access => {},
            TokenType::Refresh => return Outcome::Error((Status::Unauthorized, AuthError::WrongTokenType)),
        }

        let user_id = match Uuid::parse_str(&claims.sub) {
            Ok(id) => id,
            Err(_) => return Outcome::Error((Status::Unauthorized, AuthError::InvalidToken)),
        };

        Outcome::Success(AuthenticatedUser {
            id: user_id,
            username: claims.username,
            role: claims.role,
        })
    }
}

#[derive(Debug)]
pub enum AuthError {
    MissingToken,
    InvalidToken,
    InvalidFormat,
    WrongTokenType,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::MissingToken => write!(f, "Missing authentication token"),
            AuthError::InvalidToken => write!(f, "Invalid authentication token"),
            AuthError::InvalidFormat => write!(f, "Invalid token format"),
            AuthError::WrongTokenType => write!(f, "Wrong token type"),
        }
    }
}

impl std::error::Error for AuthError {}

pub struct AdminUser(pub AuthenticatedUser);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AdminUser {
    type Error = AuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let user = match AuthenticatedUser::from_request(req).await {
            Outcome::Success(user) => user,
            Outcome::Error(e) => return Outcome::Error(e),
            Outcome::Forward(f) => return Outcome::Forward(f),
        };

        match user.role {
            UserRole::Admin => Outcome::Success(AdminUser(user)),
            _ => Outcome::Error((Status::Forbidden, AuthError::InvalidToken)),
        }
    }
}

pub struct StreamerUser(pub AuthenticatedUser);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for StreamerUser {
    type Error = AuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let user = match AuthenticatedUser::from_request(req).await {
            Outcome::Success(user) => user,
            Outcome::Error(e) => return Outcome::Error(e),
            Outcome::Forward(f) => return Outcome::Forward(f),
        };

        match user.role {
            UserRole::Admin | UserRole::Streamer => Outcome::Success(StreamerUser(user)),
            _ => Outcome::Error((Status::Forbidden, AuthError::InvalidToken)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;

    fn test_auth_config() -> AuthConfig {
        AuthConfig {
            jwt_secret: "test-secret-key-minimum-32-characters-long".to_string(),
            jwt_expiry: 3600,
            refresh_expiry: 86400,
            bcrypt_cost: 4, // Lower cost for tests
            require_email_verification: false,
        }
    }

    fn test_user() -> User {
        User {
            id: Uuid::new_v4(),
            username: "testuser".to_string(),
            email: "test@example.com".to_string(),
            password_hash: "hashed_password".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            is_active: true,
            is_verified: true,
            stream_key: Some("test_stream_key".to_string()),
            role: UserRole::Streamer,
        }
    }

    #[test]
    fn test_password_hashing() {
        let auth_manager = AuthManager::new(test_auth_config());
        let password = "test_password";
        
        let hash = auth_manager.hash_password(password).unwrap();
        assert!(auth_manager.verify_password(password, &hash).unwrap());
        assert!(!auth_manager.verify_password("wrong_password", &hash).unwrap());
    }

    #[test]
    fn test_token_generation_and_verification() {
        let auth_manager = AuthManager::new(test_auth_config());
        let user = test_user();
        
        let (access_token, refresh_token) = auth_manager.generate_tokens(&user).unwrap();
        
        // Verify access token
        let access_claims = auth_manager.verify_token(&access_token).unwrap();
        assert_eq!(access_claims.username, user.username);
        assert!(matches!(access_claims.token_type, TokenType::Access));
        
        // Verify refresh token
        let refresh_claims = auth_manager.verify_token(&refresh_token).unwrap();
        assert_eq!(refresh_claims.username, user.username);
        assert!(matches!(refresh_claims.token_type, TokenType::Refresh));
    }

    #[test]
    fn test_token_extraction() {
        let auth_manager = AuthManager::new(test_auth_config());
        
        let header = "Bearer abc123";
        assert_eq!(auth_manager.extract_token_from_header(header), Some("abc123"));
        
        let invalid_header = "InvalidFormat abc123";
        assert_eq!(auth_manager.extract_token_from_header(invalid_header), None);
    }
}