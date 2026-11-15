//! # routes/shares.rs
//!
//! Inviting people OUTSIDE the building to look at a study: the staff side of the feature.
//! Intranet listener only. All routes need the `share` permission.
//!
//! * `POST /api/studies/{id}/share`  create an invitation: body `{ hours?, downloads?, note? }`
//! * `GET  /api/shares`              the invitations (your own; administrators see everybody's)
//! * `POST /api/shares/{id}/revoke`  switch an invitation off at once
//!
//! ## What an invitation is
//! A link `https://<public address>/s/<token>` plus a **generated password**. The guest opens
//! the link, types the password, and can then look at that ONE study in the browser viewer and
//! download it as a ZIP, a limited number of times, until the link expires. Running out of
//! downloads does not stop the online viewing: only the expiry (or a revocation) does.
//! The guest side lives in `routes/guest.rs`; the rules that confine the guest to the one
//! study are in `sharing.rs`.
//!
//! The password is shown ONCE, in the answer to the creation request, and is stored only as an
//! Argon2 hash: nobody (not even an administrator) can read it afterwards. Give it to the guest
//! through a different channel than the link (a phone call, an SMS...).

use crate::auth::{self, AuthUser};
use crate::db::{NewShare, Share};
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::sharing;
use crate::state::AppState;
use crate::util::{is_valid_orthanc_id, now_unix};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize)]
pub struct CreateShare {
    /// How long the link works, in hours. Default and maximum come from the settings.
    hours: Option<u64>,
    /// How many times the study may be downloaded as a ZIP (0 = view only). Same.
    downloads: Option<u64>,
    /// Optional reminder for the staff ("Dr. Smith, second opinion"); never shown to the guest.
    note: Option<String>,
}

/// Creates an invitation for a study and returns the link and the password (shown only now).
pub async fn create_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CreateShare>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid study id".into()));
    }
    let settings = state.settings.get();
    if settings.share_public_url.is_empty() {
        return Err(AppError::BadRequest(
            "Sharing is not set up yet: an administrator must enter the public address for share links \
             in Settings > Share links."
                .into(),
        ));
    }

    // The two limits: what was asked, or the defaults, never beyond the administrator's maximum.
    let hours = req.hours.unwrap_or(settings.share_default_hours);
    if hours < 1 || hours > settings.share_max_hours {
        return Err(AppError::BadRequest(format!("A link can be valid for 1 to {} hours", settings.share_max_hours)));
    }
    let downloads = req.downloads.unwrap_or(settings.share_default_downloads);
    if downloads > settings.share_max_downloads {
        return Err(AppError::BadRequest(format!("A link can allow 0 to {} downloads", settings.share_max_downloads)));
    }
    let note: String = req.note.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(120).collect();

    // Reading the study checks that it exists and gives what the link will point at.
    let study = StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{id}")).await?);
    if study.study_instance_uid.is_empty() {
        return Err(AppError::BadRequest("This study has no StudyInstanceUID and cannot be shared".into()));
    }

    let password = sharing::generate_password();
    let password_hash = auth::hash_password(sharing::normalize_password(&password)).await?;
    let token = sharing::new_token();
    let expires_at = now_unix() + (hours as i64) * 3600;
    let label: String =
        format!("{} | {} | {}", study.patient_name.replace('^', " "), study.study_date, study.study_description)
            .chars()
            .take(160)
            .collect();

    let share = state.db.create_share(&NewShare {
        token: &token,
        password_hash: &password_hash,
        expires_at,
        created_by: &user.username,
        study_id: &id,
        study_uid: &study.study_instance_uid,
        label: &label,
        note: &note,
        max_downloads: downloads as i64,
    })?;
    state.db.audit(
        &user.username,
        "share_created",
        &format!("#{} {label} ({hours} h, {downloads} downloads)", share.id),
    );

    Ok(Json(json!({
        "id": share.id,
        "url": sharing::share_url(&settings.share_public_url, &token),
        "password": password, // the ONLY time it can be read
        "expires_at": expires_at,
        "hours": hours,
        "max_downloads": downloads,
        "label": label,
    })))
}

/// One row of the Shares tab.
#[derive(Serialize)]
pub struct ShareView {
    id: i64,
    label: String,
    note: String,
    created_by: String,
    created_at: i64,
    expires_at: i64,
    max_downloads: i64,
    downloads_used: i64,
    /// `active`, `expired` or `revoked`.
    status: &'static str,
    /// The link (so it can be copied again; the password cannot).
    url: String,
    /// Times the password was entered correctly, and the last time.
    access_count: i64,
    last_access_at: Option<i64>,
}

fn view(share: Share, base_url: &str, now: i64) -> ShareView {
    ShareView {
        id: share.id,
        status: sharing::status_name(share.revoked_at.is_some(), now >= share.expires_at),
        url: if base_url.is_empty() { String::new() } else { sharing::share_url(base_url, &share.token) },
        label: share.label,
        note: share.note,
        created_by: share.created_by,
        created_at: share.created_at,
        expires_at: share.expires_at,
        max_downloads: share.max_downloads,
        downloads_used: share.downloads_used,
        access_count: share.access_count,
        last_access_at: share.last_access_at,
    }
}

/// The invitations. Users see their own; administrators see everybody's.
pub async fn list_shares(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Vec<ShareView>>> {
    user.require(Permission::Share)?;
    let only_user = if user.permissions.contains(Permission::Admin) { None } else { Some(user.username.as_str()) };
    let base_url = state.settings.get().share_public_url.clone();
    let now = now_unix();
    let shares = state.db.list_shares(only_user, 100)?;
    Ok(Json(shares.into_iter().map(|s| view(s, &base_url, now)).collect()))
}

/// Switches an invitation off: the guest's session ends and the link stops working at once.
pub async fn revoke_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    let share = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    let is_admin = user.permissions.contains(Permission::Admin);
    if !is_admin && !share.created_by.eq_ignore_ascii_case(&user.username) {
        return Err(AppError::Denied("You can only revoke the links you created".into()));
    }
    state.db.revoke_share(id, now_unix())?;
    // No need to end the guest's session by hand: every guest request re-reads the link from the
    // database, so a guest who is looking at it right now gets "410 Gone" on their very next
    // request, and the page then tells them the link is no longer available.
    state.db.audit(&user.username, "share_revoked", &format!("#{id} {}", share.label));
    Ok(Json(json!({ "ok": true })))
}
