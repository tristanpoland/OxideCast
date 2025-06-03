use rocket::http::Status;
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::{catch, Request};
use tracing::error;

use crate::api::ApiResponse;

#[catch(404)]
pub fn not_found(req: &Request) -> Json<ApiResponse<()>> {
    Json(ApiResponse::error(format!("Not found: {}", req.uri())))
}

#[catch(400)]
pub fn bad_request(req: &Request) -> Json<ApiResponse<()>> {
    Json(ApiResponse::error("Bad request".to_string()))
}

#[catch(401)]
pub fn unauthorized(req: &Request) -> Json<ApiResponse<()>> {
    Json(ApiResponse::error("Unauthorized".to_string()))
}

#[catch(403)]
pub fn forbidden(req: &Request) -> Json<ApiResponse<()>> {
    Json(ApiResponse::error("Forbidden".to_string()))
}

#[catch(422)]
pub fn unprocessable_entity(req: &Request) -> Json<ApiResponse<()>> {
    Json(ApiResponse::error("Unprocessable entity".to_string()))
}

#[catch(500)]
pub fn internal_error(req: &Request) -> Json<ApiResponse<()>> {
    error!("Internal server error for request: {}", req.uri());
    Json(ApiResponse::error("Internal server error".to_string()))
}

#[catch(default)]
pub fn default_catcher(status: Status, req: &Request) -> Json<ApiResponse<()>> {
    error!("Unhandled error {} for request: {}", status.code, req.uri());
    Json(ApiResponse::error(format!("Error {}: {}", status.code, status.reason())))
}