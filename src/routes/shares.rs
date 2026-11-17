//! # routes/shares.rs
//!
//! Inviting people OUTSIDE the building to look at a study: the staff side of the feature.
//! Intranet listener only. All routes need the `share` permission.
//!
//! * `POST /api/studies/{id}/share`  create an invitation: body `{ minutes?, downloads?, note?, email? }`
//! * `GET  /api/shares`              the invitations (your own; administrators see everybody's)
//! * `POST /api/shares/{id}/revoke`  switch an invitation off at once
//! * `POST /api/shares/{id}/password` show the password of an active invitation again
//! * `PUT  /api/shares/{id}`         change it: validity, downloads left, note, a new password, email it
//!
//! Both can email the invitation (`email: { to, message?, include_password? }`), when a mail server
//! is set up (Settings > Email, see `mailer.rs`). The addresses are checked before anything is
//! created or changed; if the mail server then fails, the link is kept and the answer says why
//! the email was not sent (`email: { sent: false, error }`).
//!
//! ## What an invitation is
//! A link `https://<public address>/s/<token>` plus a **generated password**. The guest opens
//! the link, types the password, and can then look at that ONE study in the browser viewer and
//! download it as a ZIP, a limited number of times, until the link expires. Running out of
//! downloads does not stop the online viewing: only the expiry (or a revocation) does.
//! The guest side lives in `routes/guest.rs`; the rules that confine the guest to the one
//! study are in `sharing.rs`.
//!
//! The password is checked against an Argon2 hash. By default (Settings > Share links) a copy is
//! also kept so the person who created the link, or an administrator, can see it again from the
//! Shares list: only while the link is active, never in the list itself (a separate request),
//! and every time in the audit log. The copy is erased when the link is revoked, 30 days after it
//! expires (until then it can be extended and keep its password), and all copies are erased when the
//! setting is switched off; then the password is shown only once.
//! Give it to the guest through a different channel than the link (a phone call, an SMS...).

use crate::auth::{self, AuthUser};
use crate::db::{NewShare, Share};
use crate::error::{AppError, AppResult};
use crate::mailer::{self, Invitation};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::settings::Settings;
use crate::sharing;
use crate::state::AppState;
use crate::util::{is_valid_orthanc_id, now_unix};
use axum::extract::{Path, State};
use axum::Json;
use lettre::message::Mailbox;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// "Send the invitation by email" of the Share windows.
#[derive(Deserialize)]
pub struct EmailRequest {
    /// One or several addresses, separated by commas.
    to: String,
    /// Optional personal message.
    #[serde(default)]
    message: String,
    /// Put the password in the email (the safer way is another channel: phone, text message).
    #[serde(default)]
    include_password: bool,
}

/// An [`EmailRequest`] checked before anything is created or changed.
struct PreparedEmail {
    to: Vec<Mailbox>,
    message: String,
    include_password: bool,
}

fn prepare_email(settings: &Settings, user: &str, req: Option<EmailRequest>) -> AppResult<Option<PreparedEmail>> {
    let Some(req) = req else { return Ok(None) };
    if !settings.email_configured() {
        return Err(AppError::BadRequest(
            "Email is not set up: an administrator must enter the mail server in Settings > Email".into(),
        ));
    }
    let to = mailer::parse_recipients(&req.to).map_err(AppError::BadRequest)?;
    // Counted now, before anything is created: a refused email never leaves a half-made link.
    mailer::reserve_quota(user, to.len(), now_unix()).map_err(AppError::TooManyRequests)?;
    let message: String = req.message.trim().chars().filter(|c| *c == '\n' || !c.is_control()).take(1000).collect();
    Ok(Some(PreparedEmail { to, message, include_password: req.include_password }))
}

/// Sends the invitation of `share` and records it. Never fails the request: the link exists either
/// way, so the answer only says whether the email went out (`{ sent, to, error? }`).
async fn send_invitation(
    state: &AppState,
    settings: &Settings,
    sender: &str,
    share: &Share,
    email: PreparedEmail,
    password: Option<&str>,
    updated: bool,
) -> Value {
    let to_text = mailer::addresses(&email.to);
    let now = now_unix();
    if !share.is_active(now) {
        return json!({ "sent": false, "to": to_text, "error": "The link is not active: give it more time first." });
    }
    let password = if email.include_password { password } else { None };
    let url = sharing::share_url(&settings.share_public_url, &share.token);
    let invitation = Invitation {
        portal_name: &settings.portal_name,
        sender,
        url: &url,
        // Rounded up: a link made for 90 minutes says "1 hour 30 minutes", not 29.
        minutes_left: ((share.expires_at - now + 59) / 60).max(1) as u64,
        downloads_left: share.downloads_left(),
        password,
        message: &email.message,
        updated,
        footer: &settings.email_footer,
    };
    match mailer::send(settings, email.to, &invitation.subject(), &invitation.body()).await {
        Ok(()) => {
            let with = if password.is_some() { "with the password" } else { "without the password" };
            state.db.audit(sender, "share_emailed", &format!("#{} to {to_text} ({with})", share.id));
            json!({ "sent": true, "to": to_text })
        }
        Err(error) => json!({ "sent": false, "to": to_text, "error": error }),
    }
}

/// The validity asked: `minutes` (days / hours / minutes of the Share windows), or `hours` (older
/// clients). Between 1 minute and the maximum of the settings.
fn asked_minutes(minutes: Option<u64>, hours: Option<u64>, max_hours: u64) -> AppResult<Option<u64>> {
    match minutes.or(hours.map(|h| h.saturating_mul(60))) {
        Some(m) if (1..=max_hours * 60).contains(&m) => Ok(Some(m)),
        Some(_) => Err(AppError::BadRequest(format!(
            "A link can be valid for 1 minute to {}",
            mailer::duration_words(max_hours * 60)
        ))),
        None => Ok(None),
    }
}

#[derive(Deserialize)]
pub struct CreateShare {
    /// How long the link works, in minutes. Default and maximum come from the settings.
    minutes: Option<u64>,
    /// The same in hours (older clients); `minutes` wins.
    hours: Option<u64>,
    /// How many times the study may be downloaded as a ZIP (0 = view only). Same.
    downloads: Option<u64>,
    /// Optional reminder for the staff ("Dr. Smith, second opinion"); never shown to the guest.
    note: Option<String>,
    /// Also send the invitation by email.
    email: Option<EmailRequest>,
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
    let minutes =
        asked_minutes(req.minutes, req.hours, settings.share_max_hours)?.unwrap_or(settings.share_default_hours * 60);
    let downloads = req.downloads.unwrap_or(settings.share_default_downloads);
    if downloads > settings.share_max_downloads {
        return Err(AppError::BadRequest(format!("A link can allow 0 to {} downloads", settings.share_max_downloads)));
    }
    let note: String = req.note.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(120).collect();
    let email = prepare_email(&settings, &user.username, req.email)?;

    // Reading the study checks that it exists and gives what the link will point at.
    let study = StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{id}")).await?);
    if study.study_instance_uid.is_empty() {
        return Err(AppError::BadRequest("This study has no StudyInstanceUID and cannot be shared".into()));
    }

    let password = sharing::generate_password();
    let password_hash = auth::hash_password(sharing::normalize_password(&password)).await?;
    let token = sharing::new_token();
    let expires_at = now_unix() + (minutes as i64) * 60;
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
        password_copy: settings.share_keep_passwords.then_some(password.as_str()),
    })?;
    state.db.audit(
        &user.username,
        "share_created",
        &format!("#{} {label} ({}, {downloads} downloads)", share.id, mailer::duration_words(minutes)),
    );
    let emailed = match email {
        Some(email) => {
            Some(send_invitation(&state, &settings, &user.username, &share, email, Some(&password), false).await)
        }
        None => None,
    };

    Ok(Json(json!({
        "id": share.id,
        "url": sharing::share_url(&settings.share_public_url, &token),
        "password": password,
        // Can it be shown again later from the Shares list?
        "password_kept": settings.share_keep_passwords,
        "expires_at": expires_at,
        "hours": minutes / 60,
        "minutes": minutes,
        "email": emailed,
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
    /// The link (so it can be copied again).
    url: String,
    /// The password can be shown again (POST /api/shares/{id}/password).
    password_available: bool,
    /// Passwords are kept (Settings > Share links): a link without a kept password (made before) can
    /// be given a new one, which will be.
    password_kept: bool,
    /// Times the password was entered correctly, and the last time.
    access_count: i64,
    last_access_at: Option<i64>,
}

fn view(share: Share, base_url: &str, now: i64, keep_passwords: bool) -> ShareView {
    ShareView {
        id: share.id,
        password_available: keep_passwords && share.has_password && share.is_active(now),
        password_kept: keep_passwords,
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
    let settings = state.settings.get();
    let now = now_unix();
    let shares = state.db.list_shares(only_user, 100)?;
    Ok(Json(
        shares.into_iter().map(|s| view(s, &settings.share_public_url, now, settings.share_keep_passwords)).collect(),
    ))
}

/// May `user` manage this link (show its password, revoke it)? Its creator, or an administrator.
fn may_manage(user: &crate::db::User, share: &Share) -> bool {
    user.permissions.contains(Permission::Admin) || share.created_by.eq_ignore_ascii_case(&user.username)
}

/// Shows the password of an active invitation again (its creator or an administrator). A POST,
/// not a GET: it is recorded in the audit log, and browsers and proxies never keep the answer.
pub async fn show_password(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    let share = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    if !may_manage(&user, &share) {
        return Err(AppError::Denied("You can only see the passwords of the links you created".into()));
    }
    if !state.settings.get().share_keep_passwords {
        return Err(AppError::Conflict(
            "Showing passwords again is switched off (Settings > Share links): revoke the link and create a new one"
                .into(),
        ));
    }
    let password = state.db.share_password(id, now_unix())?.ok_or_else(|| {
        AppError::Conflict(
            "This password cannot be shown: the link is no longer active, or it was created before passwords              were kept. Revoke it and create a new one if needed."
                .into(),
        )
    })?;
    state.db.audit(&user.username, "share_password_shown", &format!("#{id} {}", share.label));
    Ok(Json(json!({ "password": password })))
}

/// What can be changed on an invitation. Absent = unchanged.
#[derive(Deserialize)]
pub struct UpdateShare {
    /// The link works for this many minutes from now (up to the maximum). Can bring back an expired link.
    minutes: Option<u64>,
    /// The same in hours (older clients); `minutes` wins.
    hours: Option<u64>,
    /// How many more downloads are allowed from now on (0 to the maximum).
    downloads_left: Option<u64>,
    note: Option<String>,
    /// Give the link a new password: the old one stops working and guests using it are signed out.
    #[serde(default)]
    new_password: bool,
    /// Send the (updated) invitation by email.
    email: Option<EmailRequest>,
}

/// Changes an invitation (its creator or an administrator): more time, more downloads, the note,
/// or a new password. A revoked link cannot be changed: revoking is final.
pub async fn update_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateShare>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    let share = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    if !may_manage(&user, &share) {
        return Err(AppError::Denied("You can only change the links you created".into()));
    }
    let revoked = || AppError::Conflict("A revoked link cannot be changed: create a new one".into());
    if share.revoked_at.is_some() {
        return Err(revoked());
    }
    let settings = state.settings.get();
    let now = now_unix();

    let minutes = asked_minutes(req.minutes, req.hours, settings.share_max_hours)?;
    let expires_at = minutes.map(|m| now + (m as i64) * 60);
    let downloads_left = match req.downloads_left {
        Some(d) if d <= settings.share_max_downloads => Some(d as i64),
        Some(_) => {
            return Err(AppError::BadRequest(format!(
                "A link can allow 0 to {} more downloads",
                settings.share_max_downloads
            )))
        }
        None => None,
    };
    let note: Option<String> = req.note.map(|n| n.trim().chars().filter(|c| !c.is_control()).take(120).collect());
    let email = prepare_email(&settings, &user.username, req.email)?;
    // The password can go in the email only if the portal has it: a new one, or a kept copy.
    if email.as_ref().is_some_and(|e| e.include_password)
        && !req.new_password
        && !(settings.share_keep_passwords && share.has_password)
    {
        return Err(AppError::BadRequest(
            "The password of this link is not kept, so it cannot be put in the email: untick \"Include the \
             password\", or give the link a new password"
                .into(),
        ));
    }

    if !state.db.update_share(id, expires_at, downloads_left, note.as_deref())? {
        return Err(revoked());
    }
    let mut password = None;
    if req.new_password {
        let new = sharing::generate_password();
        let hash = auth::hash_password(sharing::normalize_password(&new)).await?;
        let copy = settings.share_keep_passwords.then_some(new.as_str());
        if !state.db.set_share_password(id, &hash, copy)? {
            return Err(revoked());
        }
        // Whoever opened the link with the old password must type the new one.
        state.share_sessions.revoke_user(id);
        password = Some(new);
    }

    let mut changes = Vec::new();
    if let Some(m) = minutes {
        changes.push(format!("{} from now", mailer::duration_words(m)));
    }
    if let Some(d) = req.downloads_left {
        changes.push(format!("{d} downloads left"));
    }
    if note.is_some() && note.as_deref() != Some(share.note.as_str()) {
        changes.push("note".into());
    }
    if password.is_some() {
        changes.push("new password".into());
    }
    state.db.audit(&user.username, "share_updated", &format!("#{id} {} ({})", share.label, changes.join(", ")));

    let updated = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    let emailed = match email {
        Some(email) => {
            let kept = if password.is_none() { state.db.share_password(id, now_unix())? } else { None };
            let for_email = password.as_deref().or(kept.as_deref());
            Some(send_invitation(&state, &settings, &user.username, &updated, email, for_email, true).await)
        }
        None => None,
    };
    Ok(Json(json!({
        "ok": true,
        "email": emailed,
        "password": password,
        "password_kept": settings.share_keep_passwords,
        "expires_at": updated.expires_at,
        "max_downloads": updated.max_downloads,
        "downloads_used": updated.downloads_used,
    })))
}

/// Switches an invitation off: the guest's session ends and the link stops working at once.
pub async fn revoke_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    let share = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    if !may_manage(&user, &share) {
        return Err(AppError::Denied("You can only revoke the links you created".into()));
    }
    state.db.revoke_share(id, now_unix())?;
    // No need to end the guest's session by hand: every guest request re-reads the link from the
    // database, so a guest who is looking at it right now gets "410 Gone" on their very next
    // request, and the page then tells them the link is no longer available.
    state.db.audit(&user.username, "share_revoked", &format!("#{id} {}", share.label));
    Ok(Json(json!({ "ok": true })))
}
