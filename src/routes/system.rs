//! # routes/system.rs
//!
//! The **System** and **Jobs** tabs of the Configuration area (administrators only; each can be
//! switched off in Settings > Features).
//!
//! * `GET  /api/admin/system`            version, storage and limits, counts, plugins, log level, metrics on/off
//! * `PUT  /api/admin/system/log-level`  `{ level: "default" | "verbose" | "trace" }`
//! * `GET  /api/admin/system/metrics`    Orthanc's Prometheus metrics, as text
//! * `GET  /api/admin/orthanc?path=/x`   read any address of Orthanc's REST API (GET only, when switched on)
//! * `GET  /api/admin/jobs`              every Orthanc job, newest first
//! * `POST /api/admin/jobs/{id}/{action}` cancel, pause, resume or resubmit a job

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

fn admin(user: &User) -> AppResult<()> {
    user.require(Permission::Admin)
}

fn system_on(state: &AppState) -> AppResult<()> {
    if state.settings.get().system_page {
        Ok(())
    } else {
        Err(AppError::Denied("The System page is switched off (Settings > Features)".into()))
    }
}

pub async fn overview(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    admin(&user)?;
    system_on(&state)?;
    let orthanc = &state.orthanc;
    let (system, statistics, plugins, log_level) = tokio::join!(
        orthanc.get_json("/system"),
        orthanc.get_json("/statistics"),
        orthanc.get_json("/plugins"),
        orthanc.get_text("/tools/log-level"),
    );
    let system = system?;
    let names: Vec<String> = plugins
        .ok()
        .and_then(|p| p.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    // Each plugin's version and description (in parallel, a few at a time).
    let plugins: Vec<Value> = stream::iter(names)
        .map(|name| async move {
            let info = orthanc.get_json(&format!("/plugins/{name}")).await.unwrap_or(Value::Null);
            json!({ "name": name, "version": info["Version"], "description": info["Description"] })
        })
        .buffered(6)
        .collect()
        .await;
    Ok(Json(json!({
        "system": {
            "name": system["Name"],
            "version": system["Version"],
            "api_version": system["ApiVersion"],
            "database_version": system["DatabaseVersion"],
            "database_backend": system["DatabaseBackendPlugin"],
            "storage_area": system["StorageAreaPlugin"],
            "dicom_aet": system["DicomAet"],
            "dicom_port": system["DicomPort"],
            "http_port": system["HttpPort"],
            "maximum_storage_size": system["MaximumStorageSize"],
            "maximum_patient_count": system["MaximumPatientCount"],
            "maximum_storage_mode": system["MaximumStorageMode"],
            "check_revisions": system["CheckRevisions"],
            "overwrite_instances": system["OverwriteInstances"],
            "ingest_transcoding": system["IngestTranscoding"],
        },
        "statistics": statistics.unwrap_or(Value::Null),
        "plugins": plugins,
        "log_level": log_level.ok().map(|l| l.trim().to_string()),
        "raw_api": state.settings.get().raw_api,
    })))
}

#[derive(Deserialize)]
pub struct LogLevel {
    level: String,
}

pub async fn set_log_level(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<LogLevel>,
) -> AppResult<Json<Value>> {
    admin(&user)?;
    system_on(&state)?;
    if !matches!(req.level.as_str(), "default" | "verbose" | "trace") {
        return Err(AppError::BadRequest("The log level is default, verbose or trace".into()));
    }
    state.orthanc.put_text("/tools/log-level", &req.level).await?;
    state.db.audit(&user.username, "orthanc_log_level", &req.level);
    Ok(Json(json!({ "level": req.level })))
}

pub async fn metrics(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Response> {
    admin(&user)?;
    system_on(&state)?;
    let text = state.orthanc.get_text("/tools/metrics-prometheus").await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

#[derive(Deserialize)]
pub struct RawQuery {
    path: String,
}

/// Is this a reasonable address of Orthanc's REST API to READ? (`/patients`, `/tools/labels?x=1`...)
pub fn is_readable_path(path: &str) -> bool {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    route.starts_with('/')
        && path.len() <= 400
        && !route.contains("//")
        && route.split('/').skip(1).all(|seg| seg != ".." && seg != ".")
        && route.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        && query.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | '&' | '-' | '_' | '.' | ','))
        // Whole files and archives are downloads, not something to read on a page.
        && !["/file", "/archive", "/media", "/pdf", "/content", "/frames", "/preview", "/image", "/rendered", "/attachments"]
            .iter()
            .any(|bad| route.contains(bad))
}

/// Reads an address of Orthanc's REST API (GET only), shown as JSON or text.
pub async fn raw(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<RawQuery>,
) -> AppResult<Response> {
    admin(&user)?;
    system_on(&state)?;
    if !state.settings.get().raw_api {
        return Err(AppError::Denied("Reading Orthanc's API is switched off (Settings > Features)".into()));
    }
    if !is_readable_path(&q.path) {
        return Err(AppError::BadRequest("Give an address such as /patients or /tools/labels (no files)".into()));
    }
    let text = state.orthanc.get_text(&q.path).await?;
    let text: String = text.chars().take(2_000_000).collect();
    state.db.audit(&user.username, "orthanc_api_read", &q.path);
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

// ------------------------------------------------------------------------------- jobs

fn jobs_on(state: &AppState) -> AppResult<()> {
    if state.settings.get().jobs_page {
        Ok(())
    } else {
        Err(AppError::Denied("The Jobs page is switched off (Settings > Features)".into()))
    }
}

pub async fn list_jobs(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    admin(&user)?;
    jobs_on(&state)?;
    let jobs = state.orthanc.get_json("/jobs?expand").await?;
    let mut items: Vec<Value> = jobs
        .as_array()
        .map(|list| {
            list.iter()
                .map(|j| {
                    json!({
                        "id": j["ID"], "type": j["Type"], "state": j["State"], "progress": j["Progress"],
                        "priority": j["Priority"], "created": j["CreationTime"], "completed": j["CompletionTime"],
                        "runtime": j["EffectiveRuntime"], "error": j["ErrorDescription"], "content": j["Content"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| b["created"].as_str().cmp(&a["created"].as_str()));
    Ok(Json(Value::Array(items)))
}

pub async fn job_action(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, action)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    admin(&user)?;
    jobs_on(&state)?;
    if !(id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')) {
        return Err(AppError::BadRequest("Invalid job id".into()));
    }
    if !matches!(action.as_str(), "cancel" | "pause" | "resume" | "resubmit") {
        return Err(AppError::BadRequest("The action is cancel, pause, resume or resubmit".into()));
    }
    state.orthanc.post_json(&format!("/jobs/{id}/{action}"), &json!({})).await?;
    state.db.audit(&user.username, &format!("job_{action}"), &id);
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_read_addresses() {
        assert!(is_readable_path("/patients"));
        assert!(is_readable_path("/tools/labels"));
        assert!(is_readable_path("/studies?expand=true&limit=5"));
        assert!(!is_readable_path("patients"));
        assert!(!is_readable_path("/studies/../system"));
        assert!(!is_readable_path("//evil.example.com/x"));
        assert!(!is_readable_path("/instances/abc/file"));
        assert!(!is_readable_path("/studies/abc/archive"));
        assert!(!is_readable_path("/x?a=<script>"));
        assert!(!is_readable_path("/x y"));
    }
}
