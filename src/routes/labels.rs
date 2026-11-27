//! # routes/labels.rs
//!
//! **Labels** on studies (Orthanc's own labels, so they are also visible in Orthanc's tools):
//! short words such as `urgent`, `insurance` or `teaching` that staff put on studies to find
//! them again. Intranet listener only.
//!
//! * `GET    /api/labels`                      every label, with how many studies carry it (read)
//! * `PUT    /api/studies/{id}/labels/{label}` put a label on a study (labels or admin)
//! * `DELETE /api/studies/{id}/labels/{label}` take it off (labels or admin)
//!
//! An administrator can restrict the labels that may be created (Settings > Search & labels):
//! then only those can be put on studies (labels already in Orthanc can still be taken off).
//! The search itself (filter by labels, "without labels") is in `routes/studies.rs`.

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::ResourceRef;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

/// Orthanc's rule for labels: letters, digits, `_` and `-` (we also cap the length).
pub fn valid_label(label: &str) -> bool {
    (1..=64).contains(&label.len()) && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// May this user put labels on studies and take them off? The `labels` permission, or an administrator.
pub fn can_edit_labels(user: &User) -> bool {
    user.permissions.contains(Permission::Labels) || user.permissions.contains(Permission::Admin)
}

/// Every label Orthanc knows (`GET /tools/labels`).
pub async fn all_labels(state: &AppState) -> AppResult<Vec<String>> {
    let value = state.orthanc.get_json("/tools/labels").await?;
    Ok(value.as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect()).unwrap_or_default())
}

/// The labels to offer: those in Orthanc plus the allowed ones not used yet, with the number of
/// studies of each (when counting is switched on and Orthanc can count).
/// Labels can be switched off altogether (Settings > Search & labels).
fn labels_on(state: &AppState) -> AppResult<()> {
    if state.settings.get().labels_enabled {
        Ok(())
    } else {
        Err(AppError::Denied("Labels are switched off (Settings > Search & labels)".into()))
    }
}

pub async fn list_labels(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    labels_on(&state)?;
    let settings = state.settings.get();
    let mut names = all_labels(&state).await?;
    for allowed in &settings.available_labels {
        if !names.contains(allowed) {
            names.push(allowed.clone());
        }
    }
    names.sort_by_key(|n| n.to_lowercase());

    let orthanc = &state.orthanc;
    let counting = settings.label_counts;
    let labels: Vec<Value> = stream::iter(names)
        .map(|name| async move {
            let count = if counting {
                let body = json!({ "Level": "Study", "Query": {}, "Labels": [name], "LabelsConstraint": "All" });
                orthanc.post_json("/tools/count-resources", &body).await.ok().and_then(|v| v["Count"].as_u64())
            } else {
                None
            };
            json!({ "name": name, "count": count })
        })
        .buffered(8)
        .collect()
        .await;
    Ok(Json(json!({
        "labels": labels,
        "allowed": settings.available_labels,
        "can_edit": can_edit_labels(&user),
    })))
}

/// Checks the request and gives the client of the study's server and the study.
fn check_target(state: &AppState, user: &User, id: &str, label: &str) -> AppResult<(OrthancClient, ResourceRef)> {
    if !can_edit_labels(user) {
        return Err(AppError::Forbidden("labels"));
    }
    let target = state.orthancs.resolve(id)?;
    if !valid_label(label) {
        return Err(AppError::BadRequest(
            "A label can have 1 to 64 letters, digits, '-' or '_' (no spaces or accents)".into(),
        ));
    }
    Ok(target)
}

/// The labels of a study after a change.
async fn labels_of(orthanc: &OrthancClient, id: &str) -> AppResult<Json<Value>> {
    let labels = orthanc.get_json(&format!("/studies/{id}/labels")).await?;
    Ok(Json(json!({ "labels": labels })))
}

pub async fn add_label(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, label)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (orthanc, study) = check_target(&state, &user, &id, &label)?;
    labels_on(&state)?;
    let allowed = &state.settings.get().available_labels;
    if !allowed.is_empty() && !allowed.contains(&label) {
        return Err(AppError::BadRequest(format!(
            "'{label}' is not one of the labels allowed by the administrator: {}",
            allowed.join(", ")
        )));
    }
    orthanc.put_empty(&format!("/studies/{}/labels/{label}", study.id)).await?;
    state.db.audit(&user.username, "label_added", &format!("{label} on study {study}"));
    labels_of(&orthanc, &study.id).await
}

pub async fn remove_label(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, label)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let (orthanc, study) = check_target(&state, &user, &id, &label)?;
    labels_on(&state)?;
    orthanc.delete(&format!("/studies/{}/labels/{label}", study.id)).await?;
    state.db.audit(&user.username, "label_removed", &format!("{label} from study {study}"));
    labels_of(&orthanc, &study.id).await
}

#[cfg(test)]
mod tests {
    use super::valid_label;

    #[test]
    fn label_names() {
        assert!(valid_label("urgent") && valid_label("second-opinion") && valid_label("TEACHING_2026"));
        assert!(!valid_label("") && !valid_label("with space") && !valid_label("é") && !valid_label("a/b"));
        assert!(!valid_label(&"x".repeat(65)));
    }
}
