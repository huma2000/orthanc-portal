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
use crate::db::{NewShare, Share, SharedStudy};
use crate::error::{AppError, AppResult};
use crate::mailer::{self, Invitation};
use crate::models::StudySummary;
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::{compound, ResourceRef};
use crate::settings::Settings;
use crate::sharing;
use crate::state::AppState;
use crate::util::{feature, is_valid_orthanc_id, now_unix};
use axum::extract::{Path, State};
use axum::Json;
use futures_util::StreamExt;
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
pub(crate) struct PreparedEmail {
    pub(crate) to: Vec<Mailbox>,
    pub(crate) message: String,
    pub(crate) include_password: bool,
}

pub(crate) fn prepare_email(
    settings: &Settings,
    user: &str,
    req: Option<EmailRequest>,
) -> AppResult<Option<PreparedEmail>> {
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
        language: settings.language,
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
pub(crate) fn asked_minutes(minutes: Option<u64>, hours: Option<u64>, max_hours: u64) -> AppResult<Option<u64>> {
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
    /// `POST /api/shares` only: the studies of a link that shares several (Settings > Features).
    #[serde(default)]
    studies: Vec<String>,
    /// Share EVERY study of the patient of this study (it comes first).
    #[serde(default)]
    patient: bool,
    /// `POST /api/shares` only: share every study that has this label (instead of `studies`).
    #[serde(default)]
    label: Option<String>,
    /// Share anonymized copies (made now, deleted when the link ends), not the originals.
    #[serde(default)]
    anonymize: bool,
    /// Email me when the guest opens the link and when they download.
    #[serde(default)]
    notify: bool,
}

/// Label put on the anonymized copies made for links (so staff can recognise them in the archive).
pub const ANONYMIZED_LABEL: &str = "shared-anonymized";

/// Every study with this Orthanc label (at most [`MAX_STUDIES_PER_LINK`], newest study date first).
/// The server finds them itself: the link shares exactly the studies that have the label now.
async fn labelled_studies(state: &AppState, label: &str) -> AppResult<Vec<String>> {
    let settings = state.settings.get();
    feature(settings.share_label && settings.labels_enabled, "Sharing every study of a label is switched off")?;
    let label = label.trim();
    if !crate::routes::labels::valid_label(label) {
        return Err(AppError::BadRequest("Invalid label".into()));
    }
    let body = json!({
        "Level": "Study", "Query": {}, "Labels": [label], "LabelsConstraint": "All", "Expand": false,
        "OrderBy": [{ "Type": "DicomTag", "Key": "StudyDate", "Direction": "DESC" }],
        "Limit": MAX_STUDIES_PER_LINK + 1,
    });
    let ids: Vec<String> = state
        .orthanc
        .find(&body)
        .await?
        .iter()
        .filter_map(|v| v.as_str().or_else(|| v["ID"].as_str()).map(str::to_string))
        .filter(|id| is_valid_orthanc_id(id))
        .collect();
    if ids.is_empty() {
        return Err(AppError::BadRequest(format!("No study has the label '{label}'")));
    }
    if ids.len() > MAX_STUDIES_PER_LINK {
        return Err(AppError::BadRequest(format!(
            "More than {MAX_STUDIES_PER_LINK} studies have the label '{label}': one link shares at most {MAX_STUDIES_PER_LINK}"
        )));
    }
    Ok(ids)
}

/// Every study of the patient of study `id`, that one first (at most [`MAX_STUDIES_PER_LINK`]).
async fn patient_studies(orthanc: &OrthancClient, id: &str) -> AppResult<Vec<String>> {
    let study = orthanc.get_json(&format!("/studies/{id}")).await?;
    let patient_id = study["ParentPatient"].as_str().unwrap_or_default().to_string();
    if !is_valid_orthanc_id(&patient_id) {
        return Ok(vec![id.to_string()]);
    }
    let patient = orthanc.get_json(&format!("/patients/{patient_id}")).await?;
    let mut ids = vec![id.to_string()];
    for other in patient["Studies"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
        if other != id && is_valid_orthanc_id(other) && ids.len() < MAX_STUDIES_PER_LINK {
            ids.push(other.to_string());
        }
    }
    Ok(ids)
}

/// Makes anonymized copies of the studies for one link: one made-up patient for all of them
/// (`ANON-<code>`), the originals untouched. Returns the ids of the copies (`key:id` on another
/// server than the default one), in the same order. If one fails, the copies already made are
/// deleted again.
async fn anonymized_copies(
    state: &AppState,
    on: (&OrthancClient, &str),
    ids: &[String],
    code: &str,
) -> AppResult<Vec<String>> {
    let (orthanc, server) = on;
    let body = json!({
        "Replace": { "PatientName": "ANONYMOUS", "PatientID": format!("ANON-{code}") },
        "KeepSource": true,
        "Force": true,
    });
    let mut copies = Vec::with_capacity(ids.len());
    for id in ids {
        let made = orthanc.post_json_long(&format!("/studies/{id}/anonymize"), &body).await;
        match made.map(|answer| answer["ID"].as_str().unwrap_or_default().to_string()) {
            Ok(copy) if is_valid_orthanc_id(&copy) => {
                // Best effort: the label only helps staff recognise the copy.
                let _ = orthanc.put_empty(&format!("/studies/{copy}/labels/{ANONYMIZED_LABEL}")).await;
                copies.push(compound(server, &copy));
            }
            outcome => {
                delete_copies(state, &copies).await;
                return Err(match outcome {
                    Err(e) => e,
                    Ok(_) => AppError::Upstream("Orthanc did not say where the anonymized copy is".into()),
                });
            }
        }
    }
    Ok(copies)
}

/// Deletes anonymized copies from Orthanc, each on its server (those already gone count as
/// deleted; those on a server switched off or not answering are tried again later).
pub async fn delete_copies(state: &AppState, copies: &[String]) -> bool {
    let mut all = true;
    for copy in copies.iter().filter_map(|c| ResourceRef::try_parse(c)) {
        let Ok(orthanc) = state.orthancs.of(&copy) else {
            tracing::warn!("the anonymized copy {copy} could not be deleted: its Orthanc server is not available");
            all = false;
            continue;
        };
        match orthanc.delete(&format!("/studies/{}", copy.id)).await {
            Ok(()) | Err(AppError::NotFound) => {}
            Err(e) => {
                tracing::warn!("the anonymized copy {copy} could not be deleted: {e}");
                all = false;
            }
        }
    }
    all
}

/// Housekeeping (main.rs, and right after a revocation): the anonymized copies of links that ended.
pub async fn delete_ended_copies(state: &AppState) {
    let Ok(due) = state.db.copies_to_delete(now_unix()) else { return };
    for (id, copies) in due {
        if delete_copies(state, &copies).await && state.db.mark_copies_deleted(id).is_ok() {
            state.db.audit("portal", "share_copies_deleted", &format!("#{id}: {} anonymized studies", copies.len()));
        }
    }
}

/// Most studies one link may share.
const MAX_STUDIES_PER_LINK: usize = 50;

/// Creates ONE invitation for several studies: `POST /api/shares { studies: [...], minutes, ... }`.
pub async fn create_multi_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(mut req): Json<CreateShare>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    let mut ids = std::mem::take(&mut req.studies);
    if let Some(label) = req.label.take() {
        if !ids.is_empty() {
            return Err(AppError::BadRequest("Share either a list of studies or a label, not both".into()));
        }
        ids = labelled_studies(&state, &label).await?;
    }
    ids.dedup();
    feature(ids.len() < 2 || state.settings.get().multi_study_shares, "Links for several studies are switched off")?;
    if ids.is_empty() || ids.len() > MAX_STUDIES_PER_LINK {
        return Err(AppError::BadRequest(format!("Choose 1 to {MAX_STUDIES_PER_LINK} studies")));
    }
    create(state, user, ids, req).await
}

/// Creates an invitation for a study and returns the link and the password (shown only now).
pub async fn create_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CreateShare>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Share)?;
    create(state, user, vec![id], req).await
}

async fn create(state: AppState, user: crate::db::User, ids: Vec<String>, req: CreateShare) -> AppResult<Json<Value>> {
    // One link, one Orthanc server: from here on `ids` are that server's own IDs.
    let raw: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (orthanc, server, mut ids) = state.orthancs.resolve_together(&raw)?;
    let settings = state.settings.get();
    if settings.share_public_url.is_empty() {
        return Err(AppError::BadRequest(
            "Sharing is not set up yet: an administrator must enter the public address for share links \
             in Settings > Share links."
                .into(),
        ));
    }
    feature(!req.anonymize || settings.share_anonymize, "Sharing anonymized copies is switched off")?;
    // "Email me": the address of the account, and a mail server to send through.
    let notify_email = if req.notify {
        feature(settings.share_notify, "Notifications of links are switched off")?;
        if !settings.email_configured() {
            return Err(AppError::BadRequest("Notifications need a mail server (Settings > Email)".into()));
        }
        if user.email.is_empty() {
            return Err(AppError::BadRequest(
                "Add your email address in My account first, so the portal knows where to tell you".into(),
            ));
        }
        Some(user.email.clone())
    } else {
        None
    };
    if req.patient {
        if ids.len() != 1 {
            return Err(AppError::BadRequest("Choose one study to share every study of its patient".into()));
        }
        feature(settings.share_patient, "Sharing every study of a patient is switched off")?;
        ids = patient_studies(&orthanc, &ids[0]).await?;
        feature(ids.len() < 2 || settings.multi_study_shares, "Links for several studies are switched off")?;
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

    let token = sharing::new_token();
    // Anonymized: the link points at copies made for it (the originals stay as they are).
    let owned_copies = if req.anonymize {
        let copies = anonymized_copies(&state, (&orthanc, &server), &ids, &token[..8].to_ascii_uppercase()).await?;
        ids = copies.iter().filter_map(|c| ResourceRef::try_parse(c)).map(|c| c.id).collect();
        copies
    } else {
        Vec::new()
    };

    // Reading the studies checks that they exist and gives what the link will point at.
    // Read a few at a time (a link can share up to 50 studies), in the order chosen.
    let orthanc = &orthanc;
    let read: Vec<AppResult<Value>> = futures_util::stream::iter(ids.clone())
        .map(|id| async move { orthanc.get_json(&format!("/studies/{id}")).await })
        .buffered(6)
        .collect()
        .await;
    let mut studies = Vec::with_capacity(ids.len());
    for study in read {
        let study = match study {
            Ok(study) => StudySummary::from_orthanc(&study),
            Err(e) => {
                delete_copies(&state, &owned_copies).await;
                return Err(e);
            }
        };
        if study.study_instance_uid.is_empty() {
            delete_copies(&state, &owned_copies).await;
            return Err(AppError::BadRequest("This study has no StudyInstanceUID and cannot be shared".into()));
        }
        studies.push(study);
    }
    let study = &studies[0];
    let id = compound(&server, &ids[0]);
    let extra: Vec<SharedStudy> = ids[1..]
        .iter()
        .zip(&studies[1..])
        .map(|(id, s)| SharedStudy { id: compound(&server, id), uid: s.study_instance_uid.clone() })
        .collect();

    let password = sharing::generate_password();
    let password_hash = auth::hash_password(sharing::normalize_password(&password)).await?;
    let expires_at = now_unix() + (minutes as i64) * 60;
    let label: String =
        format!("{} | {} | {}", study.patient_name.replace('^', " "), study.study_date, study.study_description)
            .chars()
            .take(140)
            .collect();
    let label = if extra.is_empty() { label } else { format!("{label} + {} more", extra.len()) };

    let created = state.db.create_share(&NewShare {
        token: &token,
        password_hash: &password_hash,
        expires_at,
        created_by: &user.username,
        study_id: &id,
        study_uid: &study.study_instance_uid,
        label: &label,
        note: &note,
        max_downloads: downloads as i64,
        extra_studies: &extra,
        password_copy: settings.share_keep_passwords.then_some(password.as_str()),
        notify_email: notify_email.as_deref(),
        owned_copies: &owned_copies,
    });
    let share = match created {
        Ok(share) => share,
        Err(e) => {
            delete_copies(&state, &owned_copies).await;
            return Err(e);
        }
    };
    state.db.audit(
        &user.username,
        "share_created",
        &format!(
            "#{} {label} ({}, {downloads} downloads{}{})",
            share.id,
            mailer::duration_words(minutes),
            if owned_copies.is_empty() { "" } else { ", anonymized copy" },
            if notify_email.is_some() { ", notify" } else { "" }
        ),
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
        "studies": 1 + extra.len(),
        "anonymized": !owned_copies.is_empty(),
        "notify": notify_email.is_some(),
    })))
}

/// The link of an active invitation as a QR code (SVG picture), for a printed handout.
pub async fn share_qr(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<axum::response::Response> {
    user.require(Permission::Share)?;
    let share = state.db.get_share(id)?.ok_or(AppError::NotFound)?;
    if !may_manage(&user, &share) {
        return Err(AppError::Denied("You can only print the links you created".into()));
    }
    let settings = state.settings.get();
    feature(settings.share_qr, "QR codes of links are switched off")?;
    if !share.is_active(now_unix()) || settings.share_public_url.is_empty() {
        return Err(AppError::Conflict("This link is not active".into()));
    }
    qr_response(&sharing::share_url(&settings.share_public_url, &share.token))
}

/// An SVG QR code as an answer (never kept by browsers: it opens patient data).
pub fn qr_response(text: &str) -> AppResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;
    let svg = crate::totp::qr_svg(text).ok_or_else(|| AppError::Internal("the QR code could not be drawn".into()))?;
    Ok(([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "no-store")], svg).into_response())
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
    /// How many studies the link shares.
    study_count: usize,
    /// It shares anonymized copies (made for it, deleted when it ends).
    anonymized: bool,
    /// Its creator is emailed when it is opened or downloaded.
    notify: bool,
}

fn view(share: Share, base_url: &str, now: i64, keep_passwords: bool) -> ShareView {
    ShareView {
        study_count: 1 + share.extra_studies.len(),
        anonymized: !share.owned_copies.is_empty(),
        notify: share.notify_email.is_some(),
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
            "This password cannot be shown: the link is no longer active, or it was created before passwords were kept. Revoke it and create a new one if needed."
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
    if share.copies_deleted {
        return Err(AppError::Conflict(
            "The anonymized copy this link shared was deleted when it ended: create a new link".into(),
        ));
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
    // Anonymized copies made for the link are deleted now (in the background: Orthanc may take a while).
    if !share.owned_copies.is_empty() {
        let state = state.clone();
        tokio::spawn(async move { delete_ended_copies(&state).await });
    }
    Ok(Json(json!({ "ok": true })))
}
