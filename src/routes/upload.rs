//! # routes/upload.rs
//!
//! `POST /api/upload` - requires the `upload` permission.
//!
//! The browser sends ONE file per request, as the raw request body (no
//! multipart). This design has three advantages:
//!   * the file is *streamed* to Orthanc, never held fully in memory;
//!   * the web UI can show per-file progress and retry single failures;
//!   * every request stays small enough for Cloudflare's per-request upload
//!     limit (100 MB on Free/Pro plans), because a study is many small files.
//!
//! The optional header `X-File-Name` (URL-encoded) is only used for the audit log.
//! Orthanc accepts a DICOM file or a ZIP archive of DICOM files.

use crate::auth::AuthUser;
use crate::error::AppResult;
use crate::permissions::Permission;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::Json;
use percent_encoding::percent_decode_str;
use serde_json::Value;

pub async fn upload(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Json<Value>> {
    user.require(Permission::Upload)?;

    // Forward the size announced by the browser so Orthanc receives a normal
    // (non-chunked) request.
    let content_length =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());

    // Convert axum's request body into a stream reqwest can send as it arrives.
    let stream = body.into_data_stream();
    let result = state.orthanc.upload_instance(reqwest::Body::wrap_stream(stream), content_length).await?;

    // Audit: file name (cleaned), outcome and the study it landed in.
    let file_name = headers
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(|v| percent_decode_str(v).decode_utf8_lossy().to_string())
        .unwrap_or_default();
    let file_name: String = file_name.chars().filter(|c| !c.is_control()).take(120).collect();
    let status = result["Status"].as_str().unwrap_or("?");
    let study = result["ParentStudy"].as_str().unwrap_or("?");
    state.db.audit(&user.username, "upload", &format!("{file_name} -> study {study} ({status})"));

    // Orthanc's answer ({ID, Status, ParentStudy, ...}) is passed through to the UI.
    Ok(Json(result))
}
