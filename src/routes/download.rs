//! # routes/download.rs
//!
//! ZIP downloads - both require the `download` permission.
//!
//! * `GET /api/studies/{id}/download[?dicomdir=true]`
//! * `GET /api/series/{id}/download[?dicomdir=true]`
//!
//! Orthanc builds the ZIP and the portal streams it to the browser chunk by
//! chunk, so even multi-gigabyte studies use almost no memory here. The web UI
//! uses plain links (not JavaScript `fetch`), so the browser's own download
//! manager handles progress, pause and resume.
//!
//! * default (`/archive`): ZIP of the .dcm files organised by patient/study/series;
//! * `dicomdir=true` (`/media`): same plus a DICOMDIR index, like a CD/DVD.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{SeriesSummary, StudySummary};
use crate::orthanc::ArchiveKind;
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::{is_valid_orthanc_id, sanitize_filename};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct DownloadParams {
    /// `true` -> include a DICOMDIR index in the ZIP.
    dicomdir: Option<bool>,
}

/// Wraps Orthanc's ZIP stream in a response that makes the browser save a file.
fn zip_response(upstream: reqwest::Response, file_name: &str) -> AppResult<Response> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file_name}\""))
        // Medical data: never let browsers/proxies cache it.
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| AppError::Internal(format!("cannot build download response: {e}")))
}

/// Short, readable suffix taken from the Orthanc id, to keep file names unique.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub async fn download_study(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(params): Query<DownloadParams>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid study id".into()));
    }

    // Read the study first: this both checks it exists and gives us a nice file name.
    let study = StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{id}")).await?);
    let who = if study.patient_id.is_empty() { &study.patient_name } else { &study.patient_id };
    let file_name =
        format!("{}_{}_{}.zip", sanitize_filename(who), sanitize_filename(&study.study_date), short_id(&id));

    let upstream = state.orthanc.archive(ArchiveKind::Study, &id, params.dicomdir.unwrap_or(false)).await?;
    state.db.audit(&user.username, "download_study", &format!("{id} ({file_name})"));
    zip_response(upstream, &file_name)
}

pub async fn download_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(params): Query<DownloadParams>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid series id".into()));
    }

    let series = SeriesSummary::from_orthanc(&state.orthanc.get_json(&format!("/series/{id}")).await?);
    let file_name = format!(
        "series_{}_{}_{}.zip",
        sanitize_filename(&series.modality),
        sanitize_filename(&series.series_number),
        short_id(&id)
    );

    let upstream = state.orthanc.archive(ArchiveKind::Series, &id, params.dicomdir.unwrap_or(false)).await?;
    state.db.audit(&user.username, "download_series", &format!("{id} ({file_name})"));
    zip_response(upstream, &file_name)
}
