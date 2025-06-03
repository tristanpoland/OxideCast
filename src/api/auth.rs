use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::State;
use rocket_db_pools::{Connection, Database};
use std::sync::Arc;
use tracing::{error, info};

use crate::api::ApiResponse;
use crate::auth::{AuthManager, LoginRequest, RegisterRequest, RefreshRequest, Claims, TokenType};
use crate::database::{Db, User, CreateUserRequest, UserRole};
use crate::{AppState};

#[rocket::post("/auth/login", data = "<request>")]
pub async fn login(
    mut db: Connection<Db>,
    state: &State<AppState>,
    request: Json<LoginRequest>,
) -> Result<Json<ApiResponse<crate::auth::LoginResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    // Find user by username
    let user = match User::find_by_username(&mut **db, &request.username).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            return Err(status::Custom(
                Status::Unauthorized,
                Json(ApiResponse::error("Invalid credentials".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error during login: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // Verify password
    let password_valid = match state.auth_manager.verify_password(&request.password, &user.password_hash) {
        Ok(valid) => valid,
        Err(e) => {
            error!("Password verification error: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    if !password_valid {
        return Err(status::Custom(
            Status::Unauthorized,
            Json(ApiResponse::error("Invalid credentials".to_string())),
        ));
    }

    // Check if user is active
    if !user.is_active {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("Account is disabled".to_string())),
        ));
    }

    // Generate tokens
    let (access_token, refresh_token) = match state.auth_manager.generate_tokens(&user) {
        Ok(tokens) => tokens,
        Err(e) => {
            error!("Token generation error: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = state.auth_manager.create_login_response(&user, access_token, refresh_token);

    info!("User '{}' logged in successfully", user.username);
    Ok(Json(ApiResponse::success(response)))
}

#[rocket::post("/auth/register", data = "<request>")]
pub async fn register(
    mut db: Connection<Db>,
    state: &State<AppState>,
    request: Json<RegisterRequest>,
) -> Result<Json<ApiResponse<crate::auth::LoginResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    // Validate input
    if request.username.len() < 3 || request.username.len() > 50 {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Username must be between 3 and 50 characters".to_string())),
        ));
    }

    if request.password.len() < 8 {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Password must be at least 8 characters".to_string())),
        ));
    }

    if !request.email.contains('@') {
        return Err(status::Custom(
            Status::BadRequest,
            Json(ApiResponse::error("Invalid email address".to_string())),
        ));
    }

    // Check if username exists
    match User::find_by_username(&mut **db, &request.username).await {
        Ok(Some(_)) => {
            return Err(status::Custom(
                Status::Conflict,
                Json(ApiResponse::error("Username already exists".to_string())),
            ));
        }
        Ok(None) => {}
        Err(e) => {
            error!("Database error checking username: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    }

    // Check if email exists
    match User::find_by_email(&mut **db, &request.email).await {
        Ok(Some(_)) => {
            return Err(status::Custom(
                Status::Conflict,
                Json(ApiResponse::error("Email already exists".to_string())),
            ));
        }
        Ok(None) => {}
        Err(e) => {
            error!("Database error checking email: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    }

    // Hash password
    let password_hash = match state.auth_manager.hash_password(&request.password) {
        Ok(hash) => hash,
        Err(e) => {
            error!("Password hashing error: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // Create user
    let create_request = CreateUserRequest {
        username: request.username.clone(),
        email: request.email.clone(),
        password: password_hash,
        role: UserRole::Streamer, // Default role
    };

    let user = match User::create(&mut **db, create_request).await {
        Ok(user) => user,
        Err(e) => {
            error!("Database error creating user: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // Generate tokens
    let (access_token, refresh_token) = match state.auth_manager.generate_tokens(&user) {
        Ok(tokens) => tokens,
        Err(e) => {
            error!("Token generation error: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = state.auth_manager.create_login_response(&user, access_token, refresh_token);

    info!("User '{}' registered successfully", user.username);
    Ok(Json(ApiResponse::success(response)))
}

#[rocket::post("/auth/refresh", data = "<request>")]
pub async fn refresh_token(
    mut db: Connection<Db>,
    state: &State<AppState>,
    request: Json<RefreshRequest>,
) -> Result<Json<ApiResponse<crate::auth::LoginResponse>>, status::Custom<Json<ApiResponse<()>>>> {
    let request = request.into_inner();

    // Verify refresh token
    let claims = match state.auth_manager.verify_token(&request.refresh_token) {
        Ok(claims) => claims,
        Err(_) => {
            return Err(status::Custom(
                Status::Unauthorized,
                Json(ApiResponse::error("Invalid refresh token".to_string())),
            ));
        }
    };

    // Check if this is a refresh token
    if !matches!(claims.token_type, TokenType::Refresh) {
        return Err(status::Custom(
            Status::Unauthorized,
            Json(ApiResponse::error("Invalid token type".to_string())),
        ));
    }

    // Get user from database
    let user_id = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Err(status::Custom(
                Status::Unauthorized,
                Json(ApiResponse::error("Invalid token".to_string())),
            ));
        }
    };

    let user = match User::find_by_id(&mut **db, user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            return Err(status::Custom(
                Status::Unauthorized,
                Json(ApiResponse::error("User not found".to_string())),
            ));
        }
        Err(e) => {
            error!("Database error during token refresh: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    // Check if user is still active
    if !user.is_active {
        return Err(status::Custom(
            Status::Forbidden,
            Json(ApiResponse::error("Account is disabled".to_string())),
        ));
    }

    // Generate new tokens
    let (access_token, refresh_token) = match state.auth_manager.generate_tokens(&user) {
        Ok(tokens) => tokens,
        Err(e) => {
            error!("Token generation error during refresh: {}", e);
            return Err(status::Custom(
                Status::InternalServerError,
                Json(ApiResponse::error("Internal server error".to_string())),
            ));
        }
    };

    let response = state.auth_manager.create_login_response(&user, access_token, refresh_token);

    info!("Refreshed tokens for user '{}'", user.username);
    Ok(Json(ApiResponse::success(response)))
}