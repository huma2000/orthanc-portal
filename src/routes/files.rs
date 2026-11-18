//! # routes/files.rs
//!
//! The **image level** and the other ways of getting files out. Intranet listener only; each part
//! can be switched off in Settings > Features.
//!
//! * `GET /api/series/{id}/instances`   the images of a series: number, SOP class, whether it is a PDF (read)
//! * `GET /api/instances/{id}/tags`     every DICOM tag of an image, as Orthanc's `/tags` (read)
//! * `GET /api/instances/{id}/preview`  a PNG / JPEG picture of one image (visualize)
//! * `GET /api/instances/{id}/pdf`      the PDF report encapsulated in an image (visualize)
//! * `GET /api/instances/{id}/download` that one DICOM file (download, "single file download")
//! * `GET /api/series/{id}/nifti`       the series as NIfTI, through Orthanc's neuro plugin (download)
//! * `GET /api/download?studies=a,b&dicomdir=` several studies in ONE ZIP (download, "bulk actions")
//!
//! Everything is audited except looking at tags and pictures, like viewing a study.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::routes::download::zip_response;
use crate::state::AppState;
use crate::util::is_valid_orthanc_id;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// SOP Class UID of "Encapsulated PDF Storage".
pub const PDF_SOP_CLASS: &str = "1.2.840.10008.5.1.4.1.1.104.1";

fn check_id(id: &str) -> AppResult<()> {
    if is_valid_orthanc_id(id) {
        Ok(())
    } else {
        Err(AppError::BadRequest("Invalid id".into()))
    }
}

fn browser_on(state: &AppState) -> AppResult<()> {
    if state.settings.get().instance_browser {
        Ok(())
    } else {
        Err(AppError::Denied("Images and DICOM tags are switched off (Settings > Features)".into()))
    }
}

/// Streams an Orthanc answer to the browser with our own type and name.
fn file_response(upstream: reqwest::Response, content_type: &str, disposition: &str) -> AppResult<Response> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| AppError::Internal(format!("cannot build the answer: {e}")))
}

/// The images of a series, in order.
pub async fn series_instances(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    browser_on(&state)?;
    check_id(&id)?;
    let list = state.orthanc.get_json(&format!("/series/{id}/instances?requested-tags=SOPClassUID")).await?;
    let mut items: Vec<Value> = list
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    let tags = &i["MainDicomTags"];
                    let sop_class = i["RequestedTags"]["SOPClassUID"].as_str().unwrap_or("");
                    json!({
                        "id": i["ID"].as_str().unwrap_or(""),
                        "number": tags["InstanceNumber"].as_str().unwrap_or(""),
                        "sop_instance_uid": tags["SOPInstanceUID"].as_str().unwrap_or(""),
                        "sop_class_uid": sop_class,
                        "is_pdf": sop_class == PDF_SOP_CLASS,
                        "frames": tags["NumberOfFrames"].as_str().unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by_key(|i| i["number"].as_str().and_then(|n| n.trim().parse::<i64>().ok()).unwrap_or(i64::MAX));
    Ok(Json(Value::Array(items)))
}

/// Every DICOM tag of one image (Orthanc's `/tags`, without the pixel data).
pub async fn instance_tags(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    browser_on(&state)?;
    check_id(&id)?;
    Ok(Json(state.orthanc.get_json(&format!("/instances/{id}/tags?ignore-length=0")).await?))
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    width: Option<u32>,
}

/// A picture of one image.
pub async fn instance_preview(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> AppResult<Response> {
    user.require(Permission::Visualize)?;
    browser_on(&state)?;
    check_id(&id)?;
    let width = q.width.unwrap_or(1024).clamp(64, 2048);
    let Some((bytes, content_type)) = state.orthanc.rendered_image(&id, width).await? else {
        return Err(AppError::BadRequest("This image cannot be shown as a picture".into()));
    };
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// The PDF report inside an "Encapsulated PDF" image, shown by the browser.
pub async fn instance_pdf(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    user.require(Permission::Visualize)?;
    browser_on(&state)?;
    check_id(&id)?;
    let upstream = state.orthanc.get_stream(&format!("/instances/{id}/pdf")).await?;
    state.db.audit(&user.username, "open_pdf", &id);
    file_response(upstream, "application/pdf", "inline; filename=\"report.pdf\"")
}

/// One DICOM file.
pub async fn instance_download(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    if !state.settings.get().single_file_download {
        return Err(AppError::Denied("Downloading single files is switched off (Settings > Features)".into()));
    }
    check_id(&id)?;
    let upstream = state.orthanc.get_stream(&format!("/instances/{id}/file")).await?;
    let name = format!("{}.dcm", id.chars().take(17).collect::<String>());
    state.db.audit(&user.username, "download_instance", &id);
    file_response(upstream, "application/dicom", &format!("attachment; filename=\"{name}\""))
}

/// The series as a NIfTI file (Orthanc's neuro plugin), compressed.
pub async fn series_nifti(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    if !state.settings.get().nifti_export {
        return Err(AppError::Denied("The NIfTI export is switched off (Settings > Features)".into()));
    }
    check_id(&id)?;
    let upstream = state.orthanc.get_stream(&format!("/series/{id}/nifti?compress")).await?;
    let name = format!("series_{}.nii.gz", id.chars().take(8).collect::<String>());
    state.db.audit(&user.username, "download_nifti", &id);
    file_response(upstream, "application/gzip", &format!("attachment; filename=\"{name}\""))
}

#[derive(Deserialize)]
pub struct BulkDownload {
    /// Orthanc ids separated by commas.
    studies: String,
    dicomdir: Option<bool>,
}

/// Several studies in one ZIP (`POST /tools/create-archive` or `/tools/create-media`).
pub async fn download_many(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<BulkDownload>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    if !state.settings.get().bulk_actions {
        return Err(AppError::Denied("Actions on several studies are switched off (Settings > Features)".into()));
    }
    let ids: Vec<&str> = q.studies.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if ids.is_empty() || ids.len() > 100 {
        return Err(AppError::BadRequest("Choose 1 to 100 studies".into()));
    }
    for id in &ids {
        check_id(id)?;
    }
    let tool = if q.dicomdir.unwrap_or(false) { "/tools/create-media" } else { "/tools/create-archive" };
    let upstream = state.orthanc.post_stream(tool, &json!({ "Resources": ids, "Synchronous": true })).await?;
    let file_name = format!("{}_studies.zip", ids.len());
    state.db.audit(&user.username, "download_studies", &format!("{} studies: {}", ids.len(), ids.join(",")));
    zip_response(upstream, &file_name)
}

/// The ZIP name of a study, from the template of Settings > Features.
pub fn study_file_name(template: &str, study: &StudySummary, id: &str) -> String {
    let short: String = id.chars().take(8).collect();
    let modality = study.modalities.join("-");
    let values = [
        ("patient_id", study.patient_id.as_str()),
        ("patient_name", study.patient_name.as_str()),
        ("study_date", study.study_date.as_str()),
        ("description", study.study_description.as_str()),
        ("accession", study.accession_number.as_str()),
        ("modality", modality.as_str()),
        ("id", short.as_str()),
    ];
    format!("{}.zip", crate::settings::download_name(template, &values))
}
