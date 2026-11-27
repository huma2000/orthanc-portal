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
use crate::util::sanitize_filename;
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
pub(crate) fn zip_response(upstream: reqwest::Response, file_name: &str) -> AppResult<Response> {
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
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();

    // Read the study (for a nice file name) while Orthanc already starts building the ZIP:
    // the two requests run at the same time. A study that does not exist fails both (404).
    let dicomdir = params.dicomdir.unwrap_or(false);
    let study_path = format!("/studies/{id}");
    let (study, upstream) =
        tokio::try_join!(orthanc.get_json(&study_path), orthanc.archive(ArchiveKind::Study, &id, dicomdir))?;
    let study = StudySummary::from_orthanc(&study);
    let file_name = crate::routes::files::study_file_name(&state.settings.get().download_name_template, &study, &id);

    state.db.audit(&user.username, "download_study", &format!("{resource} ({file_name})"));
    zip_response(upstream, &file_name)
}

pub async fn download_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(params): Query<DownloadParams>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();

    // As for a study: the details (file name) and the ZIP are requested at the same time.
    let dicomdir = params.dicomdir.unwrap_or(false);
    let series_path = format!("/series/{id}");
    let (series, upstream) =
        tokio::try_join!(orthanc.get_json(&series_path), orthanc.archive(ArchiveKind::Series, &id, dicomdir))?;
    let series = SeriesSummary::from_orthanc(&series);
    let file_name = format!(
        "series_{}_{}_{}.zip",
        sanitize_filename(&series.modality),
        sanitize_filename(&series.series_number),
        short_id(&id)
    );

    state.db.audit(&user.username, "download_series", &format!("{resource} ({file_name})"));
    zip_response(upstream, &file_name)
}
