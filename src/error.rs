//! Application-wide error type.
//!
//! [`AppError`] is the canonical error returned by Axum handlers. It
//! implements [`IntoResponse`] so handlers can use `Result<T, AppError>`
//! and return appropriate HTTP status codes automatically.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;
use tracing::error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("template render error: {0}")]
    Template(#[from] askama::Error),

    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("not found")]
    NotFound,

    #[error("forbidden")]
    Forbidden,

    #[error("unauthorized")]
    Unauthorized,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        // Log internal errors at error level; user errors at debug.
        if status.is_server_error() {
            error!(error = %self, "request failed");
        }

        (status, self.to_string()).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
