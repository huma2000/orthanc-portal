//! # routes/system.rs
//!
//! The **System** and **Jobs** tabs of the Configuration area (administrators only; each can be
//! switched off in Settings > Features).
//!
//! * `GET  /api/admin/system`            version, storage and limits, counts, plugins, log level, metrics on/off
//! * `PUT  /api/admin/system/log-level`  `{ level: "default" | "verbose" | "trace" }`
//! * `GET  /api/admin/system/metrics`    Orthanc's Prometheus metrics, as text
//! * `GET  /api/admin/system/log?file=&lines=`  the end of one of Orthanc's log files (Settings > Orthanc
//!   gives their folder: Orthanc's REST API can change the log level but cannot read the log)
//! * `GET  /api/admin/system/log/download?file=`  that whole file
//! * `GET  /api/admin/orthanc?path=/x`   read any address of Orthanc's REST API (GET only, when switched on)
//! * `GET  /api/admin/jobs`              every Orthanc job, newest first
//! * `POST /api/admin/jobs/{id}/{action}` cancel, pause, resume or resubmit a job

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::feature;
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
    feature(state.settings.get().system_page, "The System page is switched off")
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
    feature(state.settings.get().raw_api, "Reading Orthanc's API is switched off")?;
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
    feature(state.settings.get().jobs_page, "The Jobs page is switched off")
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

// ------------------------------------------------------------------------ Orthanc's log

/// At most this much of the end of a log file is read for the page (the newest lines).
const LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// At most this many lines are sent to the page.
const LOG_MAX_LINES: usize = 5000;

/// One log file of the folder.
struct LogFile {
    name: String,
    path: std::path::PathBuf,
    size: u64,
    modified: i64,
}

/// Is this the name of a log file (Orthanc's `Orthanc.log.20261118-101500.123`, or `*.log`)?
/// Only plain names: never a path.
pub fn is_log_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && (name.to_ascii_lowercase().contains(".log") || name.to_ascii_lowercase().starts_with("orthanc"))
}

/// The log files of the folder, newest first.
fn log_files(dir: &str) -> AppResult<Vec<LogFile>> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AppError::BadRequest(format!("Cannot read the folder of Orthanc's log files ({dir}): {e}")))?;
    let mut files: Vec<LogFile> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let meta = entry.metadata().ok()?;
            if !meta.is_file() || !is_log_name(&name) {
                return None;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(LogFile { name, path: entry.path(), size: meta.len(), modified })
        })
        .collect();
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
    Ok(files)
}

/// The file asked for (one of the listed names, nothing else), or the newest.
fn pick_log<'a>(files: &'a [LogFile], wanted: Option<&str>) -> AppResult<Option<&'a LogFile>> {
    match wanted.filter(|w| !w.is_empty()) {
        Some(name) => files.iter().find(|f| f.name == name).map(Some).ok_or(AppError::NotFound),
        None => Ok(files.first()),
    }
}

/// The last `lines` lines of a file, reading at most [`LOG_TAIL_BYTES`] from its end.
fn tail(path: &std::path::Path, size: u64, lines: usize) -> std::io::Result<(Vec<String>, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let start = size.saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        all.remove(0); // cut in the middle of a line
    }
    let truncated = start > 0 || all.len() > lines;
    let from = all.len().saturating_sub(lines);
    Ok((all[from..].iter().map(|l| l.to_string()).collect(), truncated))
}

#[derive(Deserialize)]
pub struct LogQuery {
    file: Option<String>,
    lines: Option<usize>,
}

/// The end of one of Orthanc's log files, for the System page.
pub async fn orthanc_log(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<LogQuery>,
) -> AppResult<Json<Value>> {
    admin(&user)?;
    system_on(&state)?;
    let dir = state.settings.get().orthanc_log_dir.clone();
    if dir.is_empty() {
        return Ok(Json(json!({ "configured": false })));
    }
    let lines = q.lines.unwrap_or(500).clamp(10, LOG_MAX_LINES);
    let wanted = q.file.clone();
    let answer = tokio::task::spawn_blocking(move || -> AppResult<Value> {
        let files = log_files(&dir)?;
        let listed: Vec<Value> =
            files.iter().map(|f| json!({ "name": f.name, "size": f.size, "modified": f.modified })).collect();
        let Some(file) = pick_log(&files, wanted.as_deref())? else {
            return Ok(json!({ "configured": true, "folder": dir, "files": listed, "file": null, "lines": [] }));
        };
        let (text, truncated) = tail(&file.path, file.size, lines)
            .map_err(|e| AppError::BadRequest(format!("Cannot read {}: {e}", file.name)))?;
        Ok(json!({ "configured": true, "folder": dir, "files": listed, "file": file.name, "size": file.size,
                   "modified": file.modified, "lines": text, "truncated": truncated }))
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;
    state.db.audit(&user.username, "orthanc_log_read", answer["file"].as_str().unwrap_or("(no file)"));
    Ok(Json(answer))
}

/// One whole log file, downloaded.
pub async fn orthanc_log_download(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<LogQuery>,
) -> AppResult<Response> {
    admin(&user)?;
    system_on(&state)?;
    let dir = state.settings.get().orthanc_log_dir.clone();
    if dir.is_empty() {
        return Err(AppError::NotFound);
    }
    let wanted = q.file.clone();
    let (name, bytes) = tokio::task::spawn_blocking(move || -> AppResult<(String, Vec<u8>)> {
        let files = log_files(&dir)?;
        let file = pick_log(&files, wanted.as_deref())?.ok_or(AppError::NotFound)?;
        let bytes =
            std::fs::read(&file.path).map_err(|e| AppError::BadRequest(format!("Cannot read {}: {e}", file.name)))?;
        Ok((file.name.clone(), bytes))
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;
    state.db.audit(&user.username, "orthanc_log_download", &name);
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.txt\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
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

    #[test]
    fn only_plain_log_names() {
        assert!(is_log_name("Orthanc.log.20261118-101500.123"));
        assert!(is_log_name("orthanc.log"));
        assert!(is_log_name("Orthanc.INFO"));
        assert!(!is_log_name("../etc/passwd"));
        assert!(!is_log_name("passwd"));
        assert!(!is_log_name(".hidden.log"));
        assert!(!is_log_name("a/b.log"));
        assert!(!is_log_name("a\\b.log"));
    }

    #[test]
    fn the_end_of_a_log_file() {
        let dir = std::env::temp_dir().join(format!("portal-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Orthanc.log.1");
        let text: String = (1..=50).map(|n| format!("I line {n}\n")).collect();
        std::fs::write(&path, &text).unwrap();
        let (lines, truncated) = tail(&path, text.len() as u64, 10).unwrap();
        assert_eq!(lines.first().map(String::as_str), Some("I line 41"));
        assert_eq!(lines.last().map(String::as_str), Some("I line 50"));
        assert!(truncated);
        let files = log_files(dir.to_str().unwrap()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(pick_log(&files, Some("../x")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
