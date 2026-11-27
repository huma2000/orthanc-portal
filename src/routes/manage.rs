//! # routes/manage.rs
//!
//! **Changing the archive**: delete, modify, anonymize, add a series. Intranet listener only. Each
//! is OFF until an administrator switches it on (Settings > Features), needs its permission
//! (`delete`, or `edit` for the others; administrators may always) and goes in the audit log.
//!
//! * `DELETE /api/studies/{id}`, `/api/series/{id}`, `/api/instances/{id}`        delete (delete)
//! * `POST   /api/studies/{id}/modify`  `{ replace: {Tag: value}, keep_uids }`    change tags (edit)
//! * `POST   /api/series/{id}/modify`   same, at series level
//! * `POST   /api/studies/{id}/move`    `{ patient_from_study: <id> }`             move to that study's patient
//! * `POST   /api/series/{id}/move`     `{ study: <id> | "new", description? }`    move to another / a new study
//! * `POST   /api/studies/{id}/anonymize`, `/api/series/{id}/anonymize` `{ patient_name?, patient_id?, keep_source }`
//! * `POST   /api/studies/{id}/add-series` `{ description, files: [ "data:application/pdf;base64,..." ] }`
//! * `GET    /api/jobs/{id}`            follow the job one of these started (the UI waits for it)
//!
//! Modify, move and anonymize run as Orthanc jobs (`"Asynchronous": true`): the answer is the job
//! id and the UI follows it. Only a fixed list of tags can be changed, and the values are checked.

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::ResourceRef;
use crate::state::AppState;
use crate::util::feature;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The tags that can be changed, at study level (patient + study) and at series level.
pub const STUDY_TAGS: [&str; 13] = [
    "PatientName",
    "PatientID",
    "PatientBirthDate",
    "PatientSex",
    "OtherPatientIDs",
    "StudyDescription",
    "StudyDate",
    "StudyTime",
    "AccessionNumber",
    "ReferringPhysicianName",
    "RequestingPhysician",
    "InstitutionName",
    "StudyID",
];
pub const SERIES_TAGS: [&str; 5] =
    ["SeriesDescription", "SeriesNumber", "BodyPartExamined", "ProtocolName", "SeriesDate"];
/// The patient tags copied when a study moves to another patient.
const PATIENT_TAGS: [&str; 4] = ["PatientName", "PatientID", "PatientBirthDate", "PatientSex"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Study,
    Series,
    Instance,
}

impl Level {
    fn path(self) -> &'static str {
        match self {
            Level::Study => "studies",
            Level::Series => "series",
            Level::Instance => "instances",
        }
    }
}

/// A value a person may type into a DICOM tag: no control characters, at most 64 characters
/// (dates as YYYYMMDD, times as HHMMSS, sex as M / F / O).
fn check_value(tag: &str, value: &str) -> AppResult<()> {
    let bad = |why: &str| Err(AppError::BadRequest(format!("{tag}: {why}")));
    if value.chars().count() > 64 || value.chars().any(|c| c.is_control()) {
        return bad("at most 64 characters, on one line");
    }
    if tag.ends_with("Date") && !value.is_empty() && !(value.len() == 8 && value.chars().all(|c| c.is_ascii_digit())) {
        return bad("a date is written YYYYMMDD");
    }
    if tag.ends_with("Time") && !value.is_empty() && !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return bad("a time is written HHMMSS");
    }
    if tag == "PatientSex" && !matches!(value, "" | "M" | "F" | "O") {
        return bad("M, F, O or empty");
    }
    if tag == "PatientID" && value.trim().is_empty() {
        return bad("the patient ID cannot be empty");
    }
    Ok(())
}

/// The `Replace` object of a modification, checked against the tags allowed at `level`.
fn replacements(level: Level, replace: &BTreeMap<String, String>) -> AppResult<Map<String, Value>> {
    let allowed: Vec<&str> = match level {
        Level::Study => STUDY_TAGS.to_vec(),
        Level::Series => SERIES_TAGS.to_vec(),
        Level::Instance => Vec::new(),
    };
    let mut out = Map::new();
    for (tag, value) in replace {
        if !allowed.contains(&tag.as_str()) {
            return Err(AppError::BadRequest(format!("The tag {tag} cannot be changed here")));
        }
        let value = value.trim();
        check_value(tag, value)?;
        out.insert(tag.clone(), json!(value));
    }
    if out.is_empty() {
        return Err(AppError::BadRequest("Nothing to change".into()));
    }
    Ok(out)
}

/// Starts a modify / anonymize job and records it.
async fn run_job(
    state: &AppState,
    on: (&OrthancClient, &ResourceRef),
    user: &User,
    path: &str,
    body: Value,
    audit: (&str, String),
) -> AppResult<Json<Value>> {
    let (orthanc, resource) = on;
    let job = resource.sibling(&orthanc.start_job(path, &body).await?).compound();
    state.db.audit(&user.username, audit.0, &format!("{} (job {job})", audit.1));
    Ok(Json(json!({ "job_id": job })))
}

/// A short description of a study for the audit log.
async fn describe_study(orthanc: &OrthancClient, id: &str) -> String {
    match orthanc.get_json(&format!("/studies/{id}")).await {
        Ok(v) => {
            let s = StudySummary::from_orthanc(&v);
            format!("{id} ({} | {} | {})", s.patient_name.replace('^', " "), s.study_date, s.study_description)
        }
        Err(_) => id.to_string(),
    }
}

/// The Orthanc ID of another study named in the request, which must be on the same server.
fn same_server(resource: &ResourceRef, raw: &str) -> AppResult<String> {
    let other = ResourceRef::parse(raw)?;
    if other.server != resource.server {
        return Err(AppError::BadRequest("Both studies must be on the same Orthanc server".into()));
    }
    Ok(other.id)
}

// --------------------------------------------------------------------------- delete

async fn delete(state: AppState, user: User, level: Level, id: String) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Delete)?;
    feature(state.settings.get().allow_delete, "Deleting is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    let what = if level == Level::Study { describe_study(&orthanc, &id).await } else { id.clone() };
    orthanc.delete(&format!("/{}/{id}", level.path())).await?;
    state.db.audit(&user.username, &format!("delete_{}", level.path()), &what);
    Ok(Json(json!({ "deleted": resource.compound() })))
}

pub async fn delete_study(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    delete(s, u, Level::Study, id).await
}
pub async fn delete_series(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    delete(s, u, Level::Series, id).await
}
pub async fn delete_instance(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    delete(s, u, Level::Instance, id).await
}

// --------------------------------------------------------------------------- modify

#[derive(Deserialize)]
pub struct ModifyRequest {
    #[serde(default)]
    replace: BTreeMap<String, String>,
    /// Keep the DICOM UIDs (the study stays "the same study" for other systems). Otherwise Orthanc
    /// gives the modified copy new UIDs. Either way the original is replaced.
    #[serde(default)]
    keep_uids: bool,
}

fn modify_body(replace: Map<String, Value>, keep_uids: bool, level: Level) -> Value {
    let keep: Vec<&str> = if keep_uids {
        match level {
            Level::Study => vec!["StudyInstanceUID", "SeriesInstanceUID", "SOPInstanceUID"],
            _ => vec!["SeriesInstanceUID", "SOPInstanceUID"],
        }
    } else {
        Vec::new()
    };
    json!({ "Replace": replace, "Keep": keep, "KeepSource": false, "Force": true, "Asynchronous": true })
}

async fn modify(state: AppState, user: User, level: Level, id: String, req: ModifyRequest) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Edit)?;
    feature(state.settings.get().allow_modify, "Modifying is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    let replace = replacements(level, &req.replace)?;
    let changes =
        replace.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let path = format!("/{}/{id}/modify", level.path());
    let audit = (if level == Level::Study { "modify_study" } else { "modify_series" }, format!("{id}: {changes}"));
    run_job(&state, (&orthanc, &resource), &user, &path, modify_body(replace, req.keep_uids, level), audit).await
}

pub async fn modify_study(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<ModifyRequest>,
) -> AppResult<Json<Value>> {
    modify(s, u, Level::Study, id, req).await
}
pub async fn modify_series(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<ModifyRequest>,
) -> AppResult<Json<Value>> {
    modify(s, u, Level::Series, id, req).await
}

#[derive(Deserialize)]
pub struct MoveStudy {
    /// A study of the patient the study goes to.
    patient_from_study: String,
    #[serde(default)]
    keep_uids: bool,
}

/// Moves a study to another patient: the patient tags of that patient's study are copied.
pub async fn move_study(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<MoveStudy>,
) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Edit)?;
    feature(state.settings.get().allow_modify, "Modifying is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    let other = same_server(&resource, &req.patient_from_study)?;
    if other == id {
        return Err(AppError::BadRequest("Choose a study of another patient".into()));
    }
    let target = orthanc.get_json(&format!("/studies/{other}")).await?;
    let mut replace = Map::new();
    for tag in PATIENT_TAGS {
        replace.insert(tag.into(), json!(target["PatientMainDicomTags"][tag].as_str().unwrap_or("")));
    }
    let who = replace["PatientID"].as_str().unwrap_or("").to_string();
    let path = format!("/studies/{id}/modify");
    run_job(
        &state,
        (&orthanc, &resource),
        &user,
        &path,
        modify_body(replace, req.keep_uids, Level::Study),
        ("move_study", format!("{id} -> patient {who}")),
    )
    .await
}

#[derive(Deserialize)]
pub struct MoveSeries {
    /// The Orthanc id of the study the series goes to, or `"new"` for a new study of the same patient.
    study: String,
    /// Description of the new study.
    #[serde(default)]
    description: String,
}

/// A new, unique StudyInstanceUID (the "2.25" root: a UUID written as a number).
fn new_uid() -> String {
    let bytes: [u8; 16] = rand::random();
    format!("2.25.{}", u128::from_be_bytes(bytes))
}

/// Moves a series to another study (it takes that study's patient and study tags) or to a new study.
pub async fn move_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<MoveSeries>,
) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Edit)?;
    feature(state.settings.get().allow_modify, "Modifying is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    let mut replace = Map::new();
    let target_text = if req.study == "new" {
        let description = req.description.trim();
        check_value("StudyDescription", description)?;
        replace.insert("StudyInstanceUID".into(), json!(new_uid()));
        replace.insert("StudyDescription".into(), json!(description));
        "a new study".to_string()
    } else {
        let other = same_server(&resource, &req.study)?;
        let target = orthanc.get_json(&format!("/studies/{other}")).await?;
        for tag in PATIENT_TAGS {
            replace.insert(tag.into(), json!(target["PatientMainDicomTags"][tag].as_str().unwrap_or("")));
        }
        for tag in ["StudyInstanceUID", "StudyDescription", "StudyDate", "StudyTime", "AccessionNumber", "StudyID"] {
            replace.insert(tag.into(), json!(target["MainDicomTags"][tag].as_str().unwrap_or("")));
        }
        format!("study {}", req.study)
    };
    let body = json!({ "Replace": replace, "Keep": ["SeriesInstanceUID", "SOPInstanceUID"], "KeepSource": false,
                       "Force": true, "Asynchronous": true });
    run_job(
        &state,
        (&orthanc, &resource),
        &user,
        &format!("/series/{id}/modify"),
        body,
        ("move_series", format!("{id} -> {target_text}")),
    )
    .await
}

// ------------------------------------------------------------------------ anonymize

#[derive(Deserialize)]
pub struct AnonymizeRequest {
    #[serde(default)]
    patient_name: String,
    #[serde(default)]
    patient_id: String,
    /// Keep the original next to the anonymized copy (the default) or replace it.
    #[serde(default = "yes")]
    keep_source: bool,
}

fn yes() -> bool {
    true
}

async fn anonymize(
    state: AppState,
    user: User,
    level: Level,
    id: String,
    req: AnonymizeRequest,
) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Edit)?;
    feature(state.settings.get().allow_anonymize, "Anonymizing is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    let mut replace = Map::new();
    for (tag, value) in [("PatientName", req.patient_name.trim()), ("PatientID", req.patient_id.trim())] {
        if !value.is_empty() {
            check_value(tag, value)?;
            replace.insert(tag.into(), json!(value));
        }
    }
    let body = json!({ "Replace": replace, "KeepSource": req.keep_source, "Force": true, "Asynchronous": true });
    let path = format!("/{}/{id}/anonymize", level.path());
    let audit = format!(
        "{id} (as '{}', original {})",
        req.patient_id.trim(),
        if req.keep_source { "kept" } else { "replaced" }
    );
    run_job(
        &state,
        (&orthanc, &resource),
        &user,
        &path,
        body,
        (if level == Level::Study { "anonymize_study" } else { "anonymize_series" }, audit),
    )
    .await
}

pub async fn anonymize_study(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<AnonymizeRequest>,
) -> AppResult<Json<Value>> {
    anonymize(s, u, Level::Study, id, req).await
}
pub async fn anonymize_series(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<AnonymizeRequest>,
) -> AppResult<Json<Value>> {
    anonymize(s, u, Level::Series, id, req).await
}

// ------------------------------------------------------------------------ add series

#[derive(Deserialize)]
pub struct AddSeries {
    #[serde(default)]
    description: String,
    /// `data:application/pdf;base64,...`, `data:image/png;base64,...` or `data:image/jpeg;base64,...`.
    files: Vec<String>,
}

/// The kind of a data URL the portal accepts, from its start.
fn data_kind(file: &str) -> Option<&'static str> {
    let head = file.get(..40).unwrap_or(file).to_ascii_lowercase();
    if head.starts_with("data:application/pdf;base64,") {
        Some("pdf")
    } else if head.starts_with("data:image/png;base64,") || head.starts_with("data:image/jpeg;base64,") {
        Some("image")
    } else {
        None
    }
}

/// Adds a series to a study: one PDF report per file, or one picture per image (`/tools/create-dicom`).
pub async fn add_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<AddSeries>,
) -> AppResult<Json<Value>> {
    user.require_or_admin(Permission::Edit)?;
    feature(state.settings.get().allow_add_series, "Adding a series is switched off")?;
    let (orthanc, resource) = state.orthancs.resolve(&user, &id)?;
    let id = resource.id.clone();
    if req.files.is_empty() || req.files.len() > 50 {
        return Err(AppError::BadRequest("Choose 1 to 50 files".into()));
    }
    let kinds: Vec<Option<&str>> = req.files.iter().map(|f| data_kind(f)).collect();
    if kinds.iter().any(Option::is_none) {
        return Err(AppError::BadRequest("Only PDF, PNG and JPEG files can be added".into()));
    }
    let pdf = kinds[0] == Some("pdf");
    if kinds.iter().any(|k| (*k == Some("pdf")) != pdf) {
        return Err(AppError::BadRequest("A series is either PDF files or pictures, not both".into()));
    }
    let description = req.description.trim();
    check_value("SeriesDescription", description)?;
    let description = if description.is_empty() {
        if pdf {
            "Report"
        } else {
            "Pictures"
        }
    } else {
        description
    };

    // The study must exist (and gives the line of the audit log).
    let what = describe_study(&orthanc, &id).await;
    orthanc.get_json(&format!("/studies/{id}")).await?;

    let series_uid = new_uid();
    let modality = if pdf { "DOC" } else { "OT" };
    let mut created = 0;
    for (n, file) in req.files.iter().enumerate() {
        let body = json!({
            "Parent": id,
            "Tags": {
                "Modality": modality,
                "SeriesDescription": description,
                "SeriesInstanceUID": series_uid,
                "SeriesNumber": "9000",
                "InstanceNumber": (n + 1).to_string(),
            },
            "Content": file,
        });
        orthanc.post_json("/tools/create-dicom", &body).await?;
        created += 1;
    }
    state.db.audit(
        &user.username,
        "add_series",
        &format!("{what}: {created} {} '{description}'", if pdf { "PDF" } else { "pictures" }),
    );
    Ok(Json(json!({ "created": created })))
}

// ------------------------------------------------------------------------------ jobs

/// The state of an Orthanc job (modify, anonymize, retrieve...): State, Progress, ErrorDescription.
pub async fn job_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let may =
        [Permission::Edit, Permission::Retrieve, Permission::Send, Permission::Delete].iter().any(|p| user.may(*p));
    if !may {
        return Err(AppError::Forbidden("edit"));
    }
    let (orthanc, job_ref) = state.orthancs.resolve(&user, &id).map_err(|e| match e {
        AppError::BadRequest(_) => AppError::BadRequest("Invalid job id".into()),
        other => other,
    })?;
    let job = orthanc.get_job(&job_ref.id).await?;
    let produced = job["Content"]["ID"].as_str().map(|id| job_ref.sibling(id).compound());
    Ok(Json(json!({
        "state": job["State"],
        "progress": job["Progress"],
        "error": job["ErrorDescription"],
        "type": job["Type"],
        // What the job produced (the new study of a modification...).
        "id": produced,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_tags_with_sane_values() {
        let one = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        assert!(replacements(Level::Study, &one("PatientName", "DOE^JANE")).is_ok());
        assert!(replacements(Level::Study, &one("SOPInstanceUID", "1.2")).is_err());
        assert!(replacements(Level::Series, &one("PatientName", "X")).is_err(), "patient tags are study-level");
        assert!(replacements(Level::Study, &one("StudyDate", "2024-05-17")).is_err());
        assert!(replacements(Level::Study, &one("StudyDate", "20240517")).is_ok());
        assert!(replacements(Level::Study, &one("PatientSex", "X")).is_err());
        assert!(replacements(Level::Study, &one("PatientID", " ")).is_err());
        assert!(replacements(Level::Study, &one("StudyDescription", "a\nb")).is_err());
        assert!(replacements(Level::Study, &BTreeMap::new()).is_err());
    }

    #[test]
    fn data_urls_accepted() {
        assert_eq!(data_kind("data:application/pdf;base64,JVBERi0"), Some("pdf"));
        assert_eq!(data_kind("data:image/PNG;base64,iVBOR"), Some("image"));
        assert_eq!(data_kind("data:text/html;base64,PGgx"), None);
        assert_eq!(data_kind("https://example.com/x.pdf"), None);
    }

    #[test]
    fn new_uids_are_valid_dicom_uids() {
        let uid = new_uid();
        assert!(crate::util::is_valid_dicom_uid(&uid), "{uid}");
        assert!(uid.len() <= 64);
        assert_ne!(new_uid(), uid);
    }
}
