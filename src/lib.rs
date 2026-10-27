mod diff;
pub mod local;
mod storage;
pub mod workspace;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct AppError(pub StatusCode, pub String);

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.1)
    }
}

impl std::error::Error for AppError {}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
