//! # routes/recent.rs
//!
//! **Recently viewed**: each person's own short list of the studies they opened last (in a viewer,
//! the study window, its series), shown on the home page. Personal: nobody else, administrators
//! included, can read it. Settings > Appearance switches it off and sets its length.
//!
//! * `GET    /api/recent`              the studies, newest first (those gone from the archive are dropped)
//! * `POST   /api/recent/{id}`         `{ uid }` "I opened this study"
//! * `DELETE /api/recent`              empty my list

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::{feature, is_valid_dicom_uid, now_unix, orthanc_id};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct Viewed {
    uid: String,
}

pub async fn note(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<Viewed>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    feature(state.settings.get().recently_viewed, "Recently viewed studies are switched off")?;
    orthanc_id(&id)?;
    if !is_valid_dicom_uid(&req.uid) {
        return Err(AppError::BadRequest("Invalid StudyInstanceUID".into()));
    }
    state.db.note_view(user.id, &id, &req.uid, now_unix())?;
    Ok(Json(json!({ "ok": true })))
}

/// The list, read from Orthanc in ONE search (every StudyInstanceUID at once), in the order viewed.
pub async fn list(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Vec<StudySummary>>> {
    user.require(Permission::Read)?;
    let settings = state.settings.get();
    feature(settings.recently_viewed, "Recently viewed studies are switched off")?;
    let wanted = state.db.recent_views(user.id, settings.recently_viewed_count as i64)?;
    if wanted.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let uids: Vec<&str> = wanted.iter().map(|(_, uid)| uid.as_str()).collect();
    let body = json!({
        "Level": "Study",
        // A backslash-separated list of UIDs is DICOM's "any of".
        "Query": { "StudyInstanceUID": uids.join("\\") },
        "Expand": true,
        "RequestedTags": ["ModalitiesInStudy", "NumberOfStudyRelatedInstances", "NumberOfStudyRelatedSeries"],
        "Limit": wanted.len() + 5,
    });
    let found: HashMap<String, StudySummary> =
        state.orthanc.find(&body).await?.iter().map(StudySummary::from_orthanc).map(|s| (s.id.clone(), s)).collect();
    let mut gone = Vec::new();
    let mut out = Vec::with_capacity(wanted.len());
    for (id, _) in wanted {
        match found.get(&id) {
            Some(study) => out.push(study.clone()),
            None => gone.push(id),
        }
    }
    // Deleted (or replaced by a modification) since: no longer offered.
    if !gone.is_empty() {
        let _ = state.db.forget_views(user.id, &gone);
    }
    Ok(Json(out))
}

pub async fn clear(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    state.db.forget_views(user.id, &[])?;
    Ok(Json(json!({ "ok": true })))
}
