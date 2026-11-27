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
use crate::servers::ResourceRef;
use crate::state::AppState;
use crate::util::{feature, is_valid_dicom_uid, now_unix};
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
    let (_, study) = state.orthancs.resolve(&id)?;
    if !is_valid_dicom_uid(&req.uid) {
        return Err(AppError::BadRequest("Invalid StudyInstanceUID".into()));
    }
    state.db.note_view(user.id, &study.compound(), &req.uid, now_unix())?;
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
    // One search per server (every StudyInstanceUID of that server at once).
    let mut by_server: Vec<(String, Vec<&str>)> = Vec::new();
    for (id, uid) in &wanted {
        let Some(study) = ResourceRef::try_parse(id) else { continue };
        match by_server.iter_mut().find(|(server, _)| *server == study.server) {
            Some((_, uids)) => uids.push(uid),
            None => by_server.push((study.server, vec![uid])),
        }
    }
    let mut found: HashMap<String, StudySummary> = HashMap::new();
    // Servers that answered: a study missing there is gone. A server switched off or not
    // answering says nothing about its studies, which stay in the list.
    let mut answered: Vec<String> = Vec::new();
    for (server, uids) in by_server {
        let Ok(orthanc) = state.orthancs.get(&server) else { continue };
        let body = json!({
            "Level": "Study",
            // A backslash-separated list of UIDs is DICOM's "any of".
            "Query": { "StudyInstanceUID": uids.join("\\") },
            "Expand": true,
            "RequestedTags": ["ModalitiesInStudy", "NumberOfStudyRelatedInstances", "NumberOfStudyRelatedSeries"],
            "Limit": uids.len() + 5,
        });
        match orthanc.find(&body).await {
            Ok(list) => {
                for study in list.iter().map(|s| StudySummary::from_orthanc(s).on_server(&server)) {
                    found.insert(study.id.clone(), study);
                }
                answered.push(server);
            }
            Err(e) if answered.is_empty() && server == crate::settings::MAIN_SERVER => return Err(e),
            Err(e) => tracing::warn!("recently viewed: Orthanc server '{server}' did not answer: {e}"),
        }
    }
    let mut gone = Vec::new();
    let mut out = Vec::with_capacity(wanted.len());
    for (id, _) in wanted {
        match found.get(&id) {
            Some(study) => out.push(study.clone()),
            None => {
                let server = ResourceRef::try_parse(&id).map(|r| r.server);
                if server.is_none_or(|s| answered.contains(&s)) {
                    gone.push(id);
                }
            }
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
