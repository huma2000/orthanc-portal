//! # error.rs
//!
//! One error type for the whole application. Every handler returns
//! `Result<_, AppError>`; axum turns the error into an HTTP response through the
//! [`IntoResponse`] implementation below. The body is always JSON:
//! `{ "error": "human readable message" }`, which the web UI displays as-is.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Shortcut used by handlers: `AppResult<Json<Foo>>`.
pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// No valid session / token -> HTTP 401.
    #[error("Authentication required")]
    Unauthorized,

    /// Logged in, but the account lacks a permission -> HTTP 403.
    /// The payload is the permission name (e.g. "download").
    #[error("Your account does not have the '{0}' permission")]
    Forbidden(&'static str),

    /// A rule of the portal forbids this for this kind of connection (for example Weasis
    /// over the internet) -> HTTP 403. The payload is the explanation shown to the user.
    #[error("{0}")]
    Denied(String),

    /// The request itself is invalid (bad parameter, bad JSON...) -> HTTP 400.
    #[error("{0}")]
    BadRequest(String),

    /// The requested study/series/user does not exist -> HTTP 404.
    #[error("Not found")]
    NotFound,

    /// The action conflicts with existing data (duplicate username...) -> HTTP 409.
    #[error("{0}")]
    Conflict(String),

    /// Too many failed logins -> HTTP 429.
    #[error("{0}")]
    TooManyRequests(String),

    /// Orthanc is unreachable or answered with an error -> HTTP 502.
    #[error("The PACS returned an error: {0}")]
    Upstream(String),

    /// Anything unexpected -> HTTP 500. The detail is only written to the log,
    /// never sent to the browser (it could leak internals).
    #[error("Internal server error")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) | AppError::Denied(_) => StatusCode::FORBIDDEN,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        // Log server-side problems with their detail; client mistakes are not worth a log line.
        match &self {
            AppError::Internal(detail) => tracing::error!("internal error: {detail}"),
            AppError::Upstream(detail) => tracing::warn!("orthanc error: {detail}"),
            _ => {}
        }

        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Network-level failures while calling Orthanc (connection refused, timeout...).
impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        tracing::warn!("request to Orthanc failed: {e}");
        if e.is_connect() || e.is_timeout() {
            AppError::Upstream("the Orthanc server is unreachable".into())
        } else {
            AppError::Upstream("the request to the Orthanc server failed".into())
        }
    }
}

/// Database failures (SQLite). Never shown to the user.
impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Internal(format!("database error: {e}"))
    }
}
