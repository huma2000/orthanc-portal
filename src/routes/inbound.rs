//! # routes/inbound.rs
//!
//! **Upload links**: the mirror of share links. A staff member asks somebody outside (another
//! clinic, a patient with a CD of earlier imaging) to SEND studies: the portal makes a link and a
//! password; the person opens the link on the internet side, types the password and drops files or
//! a whole folder. The files go straight into Orthanc, with a label, and the staff member can be
//! told by email. Off until an administrator switches it on (Settings > Features); creating links
//! needs the `share` AND `upload` permissions.
//!
//! Staff (intranet):
//! * `POST /api/upload-links`              `{ minutes?, max_mb?, note?, label?, notify?, email? }`
//! * `GET  /api/upload-links`              the links (your own; administrators see everybody's)
//! * `PUT  /api/upload-links/{id}`         `{ minutes?, max_mb? }` more time, a bigger limit
//! * `POST /api/upload-links/{id}/revoke`  stop it at once
//! * `POST /api/upload-links/{id}/password` show its password again (audit log)
//! * `GET  /api/upload-links/{id}/qr`      the link as a QR code
//! * `GET  /api/upload-links/{id}/studies` the studies received
//!
//! Outside (internet listener):
//! * `GET  /u/<token>`                     the page (static/share.html + share.js, upload mode)
//! * `GET  /api/upload/<token>/status`     `unavailable` | `expired` | `locked` | `open`
//! * `POST /api/upload/<token>/unlock`     `{ password }` -> the `upload_session` cookie
//! * `POST /api/upload/<token>/file`       one file as the raw body (Content-Length required)
//! * `GET  /api/upload/<token>/summary`    files sent, size used, time left
//! * `POST /api/upload/<token>/lock`       end the session
//!
//! ## What the person outside can NOT do
//! See anything. The answers say only "accepted" or "not a DICOM file": no patient, no study, no id.
//! Their session cookie belongs to its own store, so it opens no shared study and no viewer. The
//! total size is reserved atomically before each file (two files at once cannot both slip under the
//! limit); a file Orthanc refuses gives its share back.

use crate::auth::{self, AuthUser};
use crate::db::{NewUploadLink, UploadLink, User};
use crate::dicom_peek::Peek;
use crate::error::{AppError, AppResult};
use crate::mailer::{self, LinkEvent, UploadInvitation};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::routes::shares::{asked_minutes, prepare_email, qr_response, EmailRequest};
use crate::sharing;
use crate::state::AppState;
use crate::util::{feature, now_unix};
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

const MB: i64 = 1024 * 1024;

/// The address of an upload link: `<public address>/u/<token>`.
pub fn upload_url(base: &str, token: &str) -> String {
    format!("{base}/u/{token}")
}

/// Upload links are switched on and this person may create them.
fn require_creator(state: &AppState, user: &User) -> AppResult<()> {
    feature(state.settings.get().upload_links, "Upload links are switched off")?;
    user.require(Permission::Share)?;
    user.require(Permission::Upload)
}

fn may_manage(user: &User, link: &UploadLink) -> bool {
    user.permissions.contains(Permission::Admin) || link.created_by.eq_ignore_ascii_case(&user.username)
}

// ================================================================== staff side

#[derive(Deserialize)]
pub struct CreateUploadLink {
    minutes: Option<u64>,
    /// Most the link may receive in total (MB): default and maximum from the settings.
    max_mb: Option<u64>,
    note: Option<String>,
    /// Label put on the studies received (default from the settings; "" = none).
    label: Option<String>,
    #[serde(default)]
    notify: bool,
    email: Option<EmailRequest>,
    /// The key of the Orthanc server the studies go to (empty = the default one).
    #[serde(default)]
    server: String,
}

/// The label asked for a link: a valid label, or empty.
fn asked_label(label: Option<String>, default: &str) -> AppResult<String> {
    let label = label.map(|l| l.trim().to_string()).unwrap_or_else(|| default.to_string());
    if !label.is_empty() && !crate::routes::labels::valid_label(&label) {
        return Err(AppError::BadRequest(
            "A label can have 1 to 64 letters, digits, '-' or '_' (no spaces or accents)".into(),
        ));
    }
    Ok(label)
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateUploadLink>,
) -> AppResult<Json<Value>> {
    require_creator(&state, &user)?;
    // What is sent through the link goes into the chosen server, which must be one of the person's.
    let server = if req.server.is_empty() { crate::settings::MAIN_SERVER.to_string() } else { req.server.clone() };
    if server == crate::settings::MAIN_SERVER {
        state.orthancs.default_for(&user)?;
    } else {
        state.orthancs.get(&user, &server)?;
    }
    let settings = state.settings.get();
    if settings.share_public_url.is_empty() {
        return Err(AppError::BadRequest(
            "Sharing is not set up yet: an administrator must enter the public address for share links \
             in Settings > Share links."
                .into(),
        ));
    }
    let minutes = asked_minutes(req.minutes, None, settings.share_max_hours)?
        .unwrap_or(settings.upload_link_default_hours.min(settings.share_max_hours) * 60);
    let max_mb = req.max_mb.unwrap_or(settings.upload_link_max_mb);
    if !(1..=settings.upload_link_max_mb).contains(&max_mb) {
        return Err(AppError::BadRequest(format!(
            "An upload link can receive 1 to {} MB",
            settings.upload_link_max_mb
        )));
    }
    let note: String = req.note.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(120).collect();
    let label = asked_label(req.label, &settings.upload_link_label)?;
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
    let email = prepare_email(&settings, &user.username, req.email)?;

    let password = sharing::generate_password();
    let hash = auth::hash_password(sharing::normalize_password(&password)).await?;
    let token = sharing::new_token();
    let expires_at = now_unix() + (minutes as i64) * 60;
    let link = state.db.create_upload_link(&NewUploadLink {
        token: &token,
        password_hash: &hash,
        password_copy: settings.share_keep_passwords.then_some(password.as_str()),
        expires_at,
        created_by: &user.username,
        note: &note,
        label: &label,
        max_bytes: (max_mb as i64) * MB,
        notify_email: notify_email.as_deref(),
        server: &server,
    })?;
    state.db.audit(
        &user.username,
        "upload_link_created",
        &format!("#{} {note} ({}, {max_mb} MB)", link.id, mailer::duration_words(minutes)),
    );
    let url = upload_url(&settings.share_public_url, &token);

    let emailed = match email {
        Some(email) => {
            let to_text = mailer::addresses(&email.to);
            // The password in a second email, without the link (Settings > Share links).
            let split = email.include_password && settings.share_email_split_password;
            let to = email.to.clone();
            let invitation = UploadInvitation {
                portal_name: &settings.portal_name,
                sender: &user.username,
                url: &url,
                minutes_left: minutes,
                max_mb: max_mb as i64,
                password: (email.include_password && !split).then_some(password.as_str()),
                password_follows: split,
                message: &email.message,
                footer: &settings.email_footer,
                language: settings.language,
            };
            Some(match mailer::send(&settings, email.to, &invitation.subject(), &invitation.body()).await {
                Ok(()) => {
                    state.db.audit(&user.username, "upload_link_emailed", &format!("#{} to {to_text}", link.id));
                    let mut answer = json!({ "sent": true, "to": to_text });
                    if split {
                        answer["password_email"] = crate::routes::shares::send_password_email(
                            &state,
                            &settings,
                            &user.username,
                            to,
                            &password,
                            true,
                            &format!("#{}", link.id),
                        )
                        .await;
                    }
                    answer
                }
                Err(error) => json!({ "sent": false, "to": to_text, "error": error }),
            })
        }
        None => None,
    };

    Ok(Json(json!({
        "id": link.id,
        "url": url,
        "password": password,
        "password_kept": settings.share_keep_passwords,
        "server": link.server,
        "expires_at": expires_at,
        "max_mb": max_mb,
        "label": label,
        "notify": notify_email.is_some(),
        "email": emailed,
    })))
}

/// One row of Shares > Upload links.
#[derive(Serialize)]
pub struct UploadLinkView {
    id: i64,
    note: String,
    label: String,
    created_by: String,
    created_at: i64,
    expires_at: i64,
    status: &'static str,
    url: String,
    password_available: bool,
    max_bytes: i64,
    bytes_received: i64,
    files_received: i64,
    files_rejected: i64,
    study_count: usize,
    access_count: i64,
    last_upload_at: Option<i64>,
    notify: bool,
}

fn view(link: UploadLink, base: &str, now: i64, keep_passwords: bool) -> UploadLinkView {
    UploadLinkView {
        status: sharing::status_name(link.revoked_at.is_some(), now >= link.expires_at),
        url: if base.is_empty() { String::new() } else { upload_url(base, &link.token) },
        password_available: keep_passwords && link.has_password && link.is_active(now),
        study_count: link.studies.len(),
        notify: link.notify_email.is_some(),
        id: link.id,
        note: link.note,
        label: link.label,
        created_by: link.created_by,
        created_at: link.created_at,
        expires_at: link.expires_at,
        max_bytes: link.max_bytes,
        bytes_received: link.bytes_received,
        files_received: link.files_received,
        files_rejected: link.files_rejected,
        access_count: link.access_count,
        last_upload_at: link.last_upload_at,
    }
}

pub async fn list(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Vec<UploadLinkView>>> {
    require_creator(&state, &user)?;
    let only = if user.permissions.contains(Permission::Admin) { None } else { Some(user.username.as_str()) };
    let settings = state.settings.get();
    let now = now_unix();
    let links = state.db.list_upload_links(only, 100)?;
    Ok(Json(
        links.into_iter().map(|l| view(l, &settings.share_public_url, now, settings.share_keep_passwords)).collect(),
    ))
}

/// The link, if this person may manage it.
fn managed(state: &AppState, user: &User, id: i64) -> AppResult<UploadLink> {
    let link = state.db.get_upload_link(id)?.ok_or(AppError::NotFound)?;
    if !may_manage(user, &link) {
        return Err(AppError::Denied("You can only manage the upload links you created".into()));
    }
    Ok(link)
}

#[derive(Deserialize)]
pub struct UpdateUploadLink {
    minutes: Option<u64>,
    max_mb: Option<u64>,
}

/// More time (from now; brings an expired link back) or a bigger size limit.
pub async fn update(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUploadLink>,
) -> AppResult<Json<Value>> {
    require_creator(&state, &user)?;
    let link = managed(&state, &user, id)?;
    let settings = state.settings.get();
    let minutes = asked_minutes(req.minutes, None, settings.share_max_hours)?;
    if let Some(mb) = req.max_mb {
        if !(1..=settings.upload_link_max_mb).contains(&mb) {
            return Err(AppError::BadRequest(format!(
                "An upload link can receive 1 to {} MB",
                settings.upload_link_max_mb
            )));
        }
    }
    let expires_at = minutes.map(|m| now_unix() + (m as i64) * 60);
    if !state.db.update_upload_link(id, expires_at, req.max_mb.map(|mb| (mb as i64) * MB))? {
        return Err(AppError::Conflict("A revoked link cannot be changed: create a new one".into()));
    }
    state.db.audit(&user.username, "upload_link_updated", &format!("#{id} {}", link.note));
    Ok(Json(json!({ "ok": true })))
}

pub async fn revoke(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    require_creator(&state, &user)?;
    let link = managed(&state, &user, id)?;
    state.db.revoke_upload_link(id, now_unix())?;
    state.upload_sessions.revoke_user(id);
    state.db.audit(&user.username, "upload_link_revoked", &format!("#{id} {}", link.note));
    Ok(Json(json!({ "ok": true })))
}

pub async fn show_password(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    require_creator(&state, &user)?;
    let link = managed(&state, &user, id)?;
    if !state.settings.get().share_keep_passwords {
        return Err(AppError::Conflict("Showing passwords again is switched off (Settings > Share links)".into()));
    }
    let password = state
        .db
        .upload_link_password(id, now_unix())?
        .ok_or_else(|| AppError::Conflict("This password cannot be shown: the link is no longer active".into()))?;
    state.db.audit(&user.username, "upload_link_password_shown", &format!("#{id} {}", link.note));
    Ok(Json(json!({ "password": password })))
}

pub async fn qr(State(state): State<AppState>, AuthUser(user): AuthUser, Path(id): Path<i64>) -> AppResult<Response> {
    require_creator(&state, &user)?;
    let link = managed(&state, &user, id)?;
    let settings = state.settings.get();
    feature(settings.share_qr, "QR codes of links are switched off")?;
    if !link.is_active(now_unix()) || settings.share_public_url.is_empty() {
        return Err(AppError::Conflict("This link is not active".into()));
    }
    qr_response(&upload_url(&settings.share_public_url, &link.token))
}

/// The studies received through a link (those still in the archive), for its creator.
pub async fn studies(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<StudySummary>>> {
    require_creator(&state, &user)?;
    user.require(Permission::Read)?;
    let link = managed(&state, &user, id)?;
    let orthanc = &state.orthancs.get(&user, &link.server)?;
    let server = link.server.as_str();
    let found: Vec<StudySummary> = futures_util::stream::iter(link.studies.into_iter().take(200))
        .map(|study| async move { orthanc.get_json(&format!("/studies/{study}")).await.ok() })
        .buffered(6)
        .filter_map(|v| async move { v.map(|v| StudySummary::from_orthanc(&v).on_server(server)) })
        .collect()
        .await;
    Ok(Json(found))
}

// ================================================================== the person outside

/// The page: the same small page as the invitations (share.js switches to upload mode on `/u/`).
pub async fn page(state: State<AppState>) -> Response {
    crate::routes::guest::page(state).await
}

/// The link of the session cookie, if it is this link and it still works.
fn sender_of(state: &AppState, headers: &HeaderMap, token: &str) -> AppResult<UploadLink> {
    // Switched off: even a person who already entered the password can send nothing more.
    if !state.settings.get().upload_links {
        return Err(AppError::Gone("This link is no longer available.".into()));
    }
    let cookie = auth::read_cookie(headers, auth::UPLOAD_COOKIE).ok_or(AppError::Unauthorized)?;
    let id = state.upload_sessions.lookup(&cookie).ok_or(AppError::Unauthorized)?;
    let link = state.db.get_upload_link(id)?.ok_or(AppError::Unauthorized)?;
    if link.token != token {
        return Err(AppError::Unauthorized);
    }
    if link.revoked_at.is_some() {
        return Err(AppError::Gone("This link is no longer available.".into()));
    }
    if !link.is_active(now_unix()) {
        return Err(AppError::Gone("This link has expired.".into()));
    }
    Ok(link)
}

pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Json<Value>> {
    let unavailable = || Json(json!({ "state": "unavailable" }));
    if !sharing::is_token_shape(&token) || !state.settings.get().upload_links {
        return Ok(unavailable());
    }
    let Some(link) = state.db.get_upload_link_by_token(&token)? else { return Ok(unavailable()) };
    if link.revoked_at.is_some() {
        return Ok(unavailable());
    }
    if !link.is_active(now_unix()) {
        return Ok(Json(json!({ "state": "expired" })));
    }
    let open = sender_of(&state, &headers, &token).is_ok();
    Ok(Json(json!({ "state": if open { "open" } else { "locked" } })))
}

#[derive(Deserialize)]
pub struct UnlockRequest {
    password: String,
}

pub async fn unlock(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Json(req): Json<UnlockRequest>,
) -> AppResult<Response> {
    let settings = state.settings.get();
    let ip = auth::client_ip(&headers, peer);
    let ip_key = auth::ip_key(&ip);
    let (max_failures, lockout) = (settings.login_max_failures, settings.lockout_secs());
    let ip_limit = max_failures.saturating_mul(4);
    let not_valid = || AppError::BadRequest("The password is not correct, or the link is not valid.".into());

    state.throttle.check(&ip_key)?;
    if !settings.upload_links || !sharing::is_token_shape(&token) || req.password.chars().count() > 200 {
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        return Err(not_valid());
    }
    let Some(link) = state.db.get_upload_link_by_token(&token)? else {
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        return Err(not_valid());
    };
    let key = format!("upl:{}", link.id);
    state.throttle.check(&key)?;
    let now = now_unix();
    if link.revoked_at.is_some() {
        return Err(AppError::Gone("This link is no longer available.".into()));
    }
    if now >= link.expires_at {
        return Err(AppError::Gone("This link has expired.".into()));
    }
    let who = format!("sender-{}", link.id);
    if !auth::verify_password(Some(link.password_hash.clone()), sharing::normalize_password(&req.password)).await {
        state.throttle.record_failure(&key, max_failures, lockout);
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        state.db.audit(&who, "upload_link_unlock_failed", &format!("ip={ip}"));
        return Err(AppError::BadRequest("The password is not correct.".into()));
    }
    state.throttle.record_success(&key);
    state.db.record_upload_link_access(link.id)?;
    state.db.audit(&who, "upload_link_unlock", &format!("ip={ip} (#{} {})", link.id, link.note));

    // Sending a whole CD can take a while: up to 12 hours, never past the link's end.
    let ttl = (link.expires_at - now).clamp(60, 12 * 3600);
    let session = state.upload_sessions.issue(link.id, ttl, auth::MAX_SESSIONS_PER_USER);
    let cookie = auth::upload_cookie(&session, ttl as u64, auth::cookie_secure(&settings, &headers));
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}

/// What the page shows: how much was sent, how much is left, until when.
pub async fn summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Json<Value>> {
    let link = sender_of(&state, &headers, &token)?;
    Ok(Json(json!({
        "now": now_unix(),
        "expires_at": link.expires_at,
        "files": link.files_received,
        "rejected": link.files_rejected,
        "bytes": link.bytes_received,
        "max_bytes": link.max_bytes,
        "max_file_bytes": state.settings.get().max_upload_bytes(),
    })))
}

/// One file (DICOM or a ZIP of DICOM files), as the raw body. The answer says only whether Orthanc
/// took it: `{ accepted: true }`, or `{ accepted: false, reason: "not_dicom" }` (the page goes on
/// with the next file: a CD also holds a viewer program and other files that are not images).
pub async fn file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    body: Body,
) -> AppResult<Json<Value>> {
    let link = sender_of(&state, &headers, &token)?;
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|len| *len > 0)
        .ok_or_else(|| AppError::BadRequest("The size of the file is missing".into()))?;
    let now = now_unix();
    if !state.db.reserve_upload_bytes(link.id, length, now)? {
        return Err(AppError::Denied(
            "This link cannot receive more: its size limit is reached. Ask the person who sent it for a new one."
                .into(),
        ));
    }

    // Read the beginning of the file to learn its study BEFORE Orthanc sees it.
    let mut stream = body.into_data_stream();
    let mut head: Vec<u8> = Vec::new();
    let uid = loop {
        match crate::dicom_peek::study_uid(&head) {
            Peek::Found(uid) => break Some(uid),
            Peek::Unreadable => break None,
            Peek::NeedMore if head.len() >= PEEK_LIMIT || head.len() as i64 >= length => break None,
            Peek::NeedMore => match stream.next().await {
                Some(Ok(chunk)) => head.extend_from_slice(&chunk),
                Some(Err(_)) | None => {
                    let _ = state.db.refund_upload_bytes(link.id, length);
                    return Err(AppError::BadRequest("The file did not arrive completely".into()));
                }
            },
        }
    };
    let refuse = |reason: &'static str| -> AppResult<Json<Value>> {
        let _ = state.db.refund_upload_bytes(link.id, length);
        state.db.record_upload_file(link.id, None, now)?;
        Ok(Json(json!({ "accepted": false, "reason": reason })))
    };
    // Only plain DICOM files whose study is new, or came in through this same link.
    let Some(uid) = uid else { return refuse("not_dicom") };
    if let Admission::No { first } = may_receive(&state, &link, &uid).await? {
        if first {
            state.db.audit(
                &format!("sender-{}", link.id),
                "upload_link_refused",
                &format!("#{}: files of study {uid}, which is already in the archive", link.id),
            );
        }
        return refuse("already_in_archive");
    }

    // The bytes read so far, then the rest as it arrives.
    let rest = futures_util::stream::iter([Ok::<_, axum::Error>(axum::body::Bytes::from(head))]).chain(stream);
    // Into the link's server (checked when the link was made).
    let orthanc = state.orthancs.stored(&link.server)?;
    match orthanc.upload_instance(reqwest::Body::wrap_stream(rest), Some(length as u64)).await {
        Ok(result) => {
            let study = result["ParentStudy"].as_str().unwrap_or_default().to_string();
            let is_new = !study.is_empty() && !link.studies.contains(&study);
            state.db.record_upload_file(link.id, (!study.is_empty()).then_some(study.as_str()), now)?;
            if is_new {
                let settings = state.settings.get();
                if !link.label.is_empty() && settings.labels_enabled && crate::util::is_valid_orthanc_id(&study) {
                    let _ = orthanc.put_empty(&format!("/studies/{study}/labels/{}", link.label)).await;
                }
                state.db.audit(
                    &format!("sender-{}", link.id),
                    "upload_link_study",
                    &format!("#{} -> study {study}", link.id),
                );
            }
            notify_received(&state, &link, length);
            Ok(Json(json!({ "accepted": true })))
        }
        Err(AppError::BadRequest(_)) => {
            let _ = state.db.refund_upload_bytes(link.id, length);
            state.db.record_upload_file(link.id, None, now)?;
            Ok(Json(json!({ "accepted": false, "reason": "not_dicom" })))
        }
        Err(e) => {
            let _ = state.db.refund_upload_bytes(link.id, length);
            Err(e)
        }
    }
}

/// How much of a file is read, at most, to find its StudyInstanceUID (it is near the start).
const PEEK_LIMIT: usize = 1024 * 1024;

/// What each upload link decided per StudyInstanceUID: `true` = let in (a new study it started),
/// `false` = refused (a study already in the archive).
/// * Two files of the same new study arriving at once must both pass, even before Orthanc has the
///   first one; after a restart the studies recorded with the link do the same job.
/// * A CD of a study the practice already has is thousands of files: only the first one needs
///   Orthanc's answer (and a line in the audit log). A study in the archive stays there, so a refusal
///   stays valid.
fn decided() -> &'static Mutex<HashMap<i64, HashMap<String, bool>>> {
    static DECIDED: OnceLock<Mutex<HashMap<i64, HashMap<String, bool>>>> = OnceLock::new();
    DECIDED.get_or_init(Default::default)
}

/// What [`may_receive`] answers.
#[derive(PartialEq, Eq)]
enum Admission {
    Yes,
    /// Refused; `first` = the first file of that study refused for this link (worth an audit line).
    No {
        first: bool,
    },
}

/// May this link receive a file of study `uid`? Yes when the study is not in the archive yet (the
/// link starts it), or when it came in through this link. No when it is a study already there: an
/// outsider must never add images to a patient's existing study (Orthanc would file them under it).
async fn may_receive(state: &AppState, link: &UploadLink, uid: &str) -> AppResult<Admission> {
    let known = decided().lock().unwrap_or_else(|e| e.into_inner()).get(&link.id).and_then(|m| m.get(uid).copied());
    match known {
        Some(true) => return Ok(Admission::Yes),
        Some(false) => return Ok(Admission::No { first: false }),
        None => {}
    }
    let body = json!({ "Level": "Study", "Query": { "StudyInstanceUID": uid }, "Expand": false, "Limit": 20 });
    let found = state.orthancs.stored(&link.server)?.find(&body).await?;
    let ids: Vec<&str> = found.iter().filter_map(|v| v.as_str().or_else(|| v["ID"].as_str())).collect();
    let admitted = ids.iter().all(|id| link.studies.iter().any(|s| s == id));
    let mut map = decided().lock().unwrap_or_else(|e| e.into_inner());
    // Bounded: links rarely bring more than a few studies; forget everything past a size.
    if map.values().map(HashMap::len).sum::<usize>() > 100_000 {
        map.clear();
    }
    map.entry(link.id).or_default().insert(uid.to_string(), admitted);
    Ok(if admitted { Admission::Yes } else { Admission::No { first: true } })
}

/// "Files are arriving" to the creator, at most once per quarter of an hour (one email per batch).
fn notify_received(state: &AppState, link: &UploadLink, just_sent: i64) {
    let Some(to) = link.notify_email.clone() else { return };
    let settings = state.settings.get();
    if !settings.share_notify
        || !settings.email_configured()
        || !state.db.upload_notice_due(link.id, now_unix(), 15 * 60).unwrap_or(false)
    {
        return;
    }
    let event = LinkEvent::Received {
        files: link.files_received + 1,
        megabytes: (link.bytes_received + just_sent).div_euclid(MB).max(1),
    };
    let (subject, body) =
        mailer::link_notice(&settings.portal_name, &format!("#{}", link.id), &link.note, event, settings.language);
    mailer::send_in_background(settings, to, subject, body);
}

pub async fn lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_token): Path<String>,
) -> AppResult<Response> {
    if let Some(cookie) = auth::read_cookie(&headers, auth::UPLOAD_COOKIE) {
        state.upload_sessions.revoke(&cookie);
    }
    let clear = auth::upload_cookie("", 0, auth::cookie_secure(&state.settings.get(), &headers));
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&clear).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}
