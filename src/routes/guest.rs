//! # routes/guest.rs
//!
//! The internet side: what a **guest with an invitation** can do. Registered only on the
//! internet-facing listener. There are no accounts here, only invitation links created by
//! staff (`routes/shares.rs`).
//!
//! * `GET  /s/<token>`                 the invitation page (one small page, `static/share.html`)
//! * `GET  /api/share/<token>/status`  `unavailable` | `expired` | `locked` | `open`
//! * `POST /api/share/<token>/unlock`  body `{ "password": "..." }` -> sets the guest cookie
//! * `GET  /api/share/<token>/study`   study + series details, time and downloads left
//! * `GET  /api/share/<token>/download` the study as a ZIP; takes one of the allowed downloads
//! * `POST /api/share/<token>/lock`    forget this session (for shared computers)
//!
//! and, through the common viewer proxy, the **browser viewer** (OHIF) for that one study.
//!
//! ## Guarantees
//! * Nothing about the study (not even the patient's name) is shown before the password is right.
//! * Guessing is hopeless: the token has 192 bits, the password about 59 bits, and wrong
//!   passwords are throttled per link and per address.
//! * Time and downloads are enforced on every request on the server. When downloads run out the
//!   guest can still LOOK at the study until the link expires; only the ZIP stops.
//! * A download is reserved atomically before it starts, and given back if Orthanc cannot even
//!   begin the archive. A download the guest abandons halfway still counts.

use crate::auth::{self, authenticate_share};
use crate::db::Share;
use crate::error::{AppError, AppResult};
use crate::mailer::{self, LinkEvent};
use crate::models::{SeriesSummary, StudySummary};
use crate::orthanc::ArchiveKind;
use crate::routes::branding::render_template;
use crate::routes::download::zip_response;
use crate::sharing;
use crate::state::AppState;
use crate::util::{now_unix, sanitize_filename};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// The only page of the internet side. `{{TITLE}}` and the icon link are filled in per request.
const SHARE_HTML: &str = include_str!("../../static/share.html");

/// `GET /` and `GET /s/<token>`: always the same page, whatever the token (it asks the server).
pub async fn page(State(state): State<AppState>) -> Response {
    render_template(&state, SHARE_HTML)
}

/// The guest of THIS link: the cookie must belong to an invitation whose token is `token`.
fn guest_of(state: &AppState, headers: &HeaderMap, token: &str) -> AppResult<Share> {
    let share = authenticate_share(state, headers)?;
    if share.token != token {
        return Err(AppError::Unauthorized); // a session of another invitation
    }
    Ok(share)
}

/// Where the page stands, before it shows anything: tells the page which screen to draw.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Json<Value>> {
    let unavailable = || Json(json!({ "state": "unavailable" }));
    if !sharing::is_token_shape(&token) {
        return Ok(unavailable());
    }
    let Some(share) = state.db.get_share_by_token(&token)? else { return Ok(unavailable()) };
    if share.revoked_at.is_some() || share.copies_deleted {
        return Ok(unavailable());
    }
    if !share.is_active(now_unix()) {
        return Ok(Json(json!({ "state": "expired" })));
    }
    let open = guest_of(&state, &headers, &token).is_ok();
    Ok(Json(json!({ "state": if open { "open" } else { "locked" } })))
}

#[derive(Deserialize)]
pub struct UnlockRequest {
    password: String,
}

/// Checks the password and, if right, starts a guest session (cookie).
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
    // The same limits as for staff logins: per link and, 4 times looser, per address.
    let (max_failures, lockout) = (settings.login_max_failures, settings.lockout_secs());
    let ip_limit = max_failures.saturating_mul(4);
    let not_valid = || AppError::BadRequest("The password is not correct, or the link is not valid.".into());

    state.throttle.check(&ip_key)?;
    if !sharing::is_token_shape(&token) || req.password.chars().count() > 200 {
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        return Err(not_valid());
    }
    let Some(share) = state.db.get_share_by_token(&token)? else {
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        return Err(not_valid());
    };
    let share_key = format!("share:{}", share.id);
    state.throttle.check(&share_key)?;

    let now = now_unix();
    if share.revoked_at.is_some() || share.copies_deleted {
        return Err(AppError::Gone("This link is no longer available.".into()));
    }
    if now >= share.expires_at {
        return Err(AppError::Gone("This link has expired.".into()));
    }

    let guest = format!("guest-{}", share.id);
    let correct =
        auth::verify_password(Some(share.password_hash.clone()), sharing::normalize_password(&req.password)).await;
    if !correct {
        state.throttle.record_failure(&share_key, max_failures, lockout);
        state.throttle.record_failure(&ip_key, ip_limit, lockout);
        state.db.audit(&guest, "share_unlock_failed", &format!("ip={ip}"));
        return Err(AppError::BadRequest("The password is not correct.".into()));
    }

    state.throttle.record_success(&share_key);
    state.db.record_share_access(share.id, now)?;
    state.db.audit(&guest, "share_unlock", &format!("ip={ip} ({})", share.label));
    // "Email me when it is opened": once, the first time.
    if share.notify_email.is_some() && state.db.first_open_notice(share.id)? {
        notify_creator(&state, &share, LinkEvent::Opened);
    }

    // The session ends when the link does (at most 12 hours: a forgotten tab does not stay open for days).
    let ttl = (share.expires_at - now).clamp(60, 12 * 3600);
    let session = state.share_sessions.issue(share.id, ttl, auth::MAX_SESSIONS_PER_USER);
    let cookie = auth::share_cookie(&session, ttl as u64, auth::cookie_secure(&settings, &headers));

    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}

/// What the guest's page shows about one study of the link.
async fn study_details(state: &AppState, study_id: &str) -> AppResult<Value> {
    let orthanc = &state.orthanc;
    let study_path = format!("/studies/{study_id}");
    let series_path = format!("{study_path}/series");
    let (study_json, series_json) = tokio::join!(orthanc.get_json(&study_path), orthanc.get_json(&series_path));
    let study = StudySummary::from_orthanc(&study_json?);
    let series_json = series_json?;
    let mut series: Vec<SeriesSummary> =
        series_json.as_array().map(|items| items.iter().map(SeriesSummary::from_orthanc).collect()).unwrap_or_default();
    series.sort_by_key(|s| s.series_number.parse::<i64>().unwrap_or(i64::MAX));

    let mut modalities: Vec<String> = series.iter().map(|s| s.modality.clone()).filter(|m| !m.is_empty()).collect();
    modalities.sort();
    modalities.dedup();
    Ok(json!({
        "patient": {
            "name": study.patient_name,
            "id": study.patient_id,
            "birth_date": study.patient_birth_date,
            "sex": study.patient_sex,
        },
        "study": {
            "date": study.study_date,
            "time": study.study_time,
            "description": study.study_description,
            "accession": study.accession_number,
            "modalities": modalities,
            "uid": study.study_instance_uid,
        },
        "series": series.iter().map(|s| json!({
            "number": s.series_number,
            "modality": s.modality,
            "description": s.description,
            "body_part": s.body_part,
            "images": s.instances_count,
        })).collect::<Vec<_>>(),
    }))
}

/// Everything the guest's page shows: the studies of the link (one or several), the time and
/// downloads left, and the viewer.
pub async fn study(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Json<Value>> {
    let share = guest_of(&state, &headers, &token)?;
    let ohif_mode = state.settings.get().ohif;
    let shared = share.studies();
    // The questions to Orthanc are independent: ask them at the same time.
    // A few studies at a time: a link can share up to 50, and each is two questions to Orthanc.
    let state_ref = &state;
    let details = futures_util::stream::iter(shared.iter().map(|s| s.id.clone()).collect::<Vec<_>>())
        .map(|id| async move { study_details(state_ref, &id).await })
        .buffered(6)
        .collect::<Vec<_>>();
    let (details, viewer_available) = tokio::join!(details, state.orthanc.ohif_available(ohif_mode));
    let mut studies = Vec::with_capacity(details.len());
    for (index, d) in details.into_iter().enumerate() {
        let mut d = d?;
        d["index"] = json!(index);
        d["viewer_url"] = json!(format!("/ohif/viewer?StudyInstanceUIDs={}", shared[index].uid));
        studies.push(d);
    }
    let first = studies[0].clone();
    let all_uids = share.study_uids().join(",");

    Ok(Json(json!({
        // The first study, as before (pages and tools that know only one study keep working).
        "patient": first["patient"],
        "study": first["study"],
        "series": first["series"],
        "studies": studies,
        // Both clocks come from the server, so a wrong clock on the guest's computer cannot
        // extend or shorten the link.
        "now": now_unix(),
        "expires_at": share.expires_at,
        "downloads": {
            "max": share.max_downloads,
            "used": share.downloads_used,
            "left": share.downloads_left(),
        },
        "viewer": {
            "available": viewer_available,
            "url": format!("/ohif/viewer?StudyInstanceUIDs={all_uids}"),
        },
    })))
}

#[derive(Deserialize)]
pub struct DownloadQuery {
    /// Which study of the link (0 = the first). Each download of a study uses one download.
    #[serde(default)]
    study: usize,
}

pub async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> AppResult<Response> {
    let share = guest_of(&state, &headers, &token)?;
    let shared = share.studies();
    let Some(target) = shared.get(q.study) else { return Err(AppError::NotFound) };

    // Read the study FIRST: if it is gone, fail before a download is used up. Also gives the file name.
    let study = StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{}", target.id)).await?);

    // Take the download atomically: two requests at the same moment cannot both use the last one.
    if !state.db.reserve_download(share.id, now_unix())? {
        return Err(AppError::Denied(if share.max_downloads == 0 {
            "Downloading is not enabled for this link. You can look at the study online.".into()
        } else {
            "No downloads are left for this link. You can still look at the study online.".into()
        }));
    }

    match state.orthanc.archive(ArchiveKind::Study, &target.id, false).await {
        Ok(upstream) => {
            let who = if study.patient_id.is_empty() { &study.patient_name } else { &study.patient_id };
            let file_name = format!("{}_{}.zip", sanitize_filename(who), sanitize_filename(&study.study_date));
            state.db.audit(&format!("guest-{}", share.id), "share_download", &format!("{} ({file_name})", share.label));
            if share.notify_email.is_some() {
                // `share` was read before this download was taken: one fewer is left now.
                notify_creator(&state, &share, LinkEvent::Downloaded { left: (share.downloads_left() - 1).max(0) });
            }
            zip_response(upstream, &file_name)
        }
        Err(e) => {
            let _ = state.db.refund_download(share.id); // Orthanc could not even start: give it back
            Err(e)
        }
    }
}

/// Emails the creator of the link what just happened (they asked for it when creating the link).
fn notify_creator(state: &AppState, share: &Share, event: LinkEvent) {
    let Some(to) = share.notify_email.clone() else { return };
    let settings = state.settings.get();
    if !settings.email_configured() {
        return;
    }
    let (subject, body) =
        mailer::link_notice(&settings.portal_name, &format!("#{}", share.id), &share.note, event, settings.language);
    mailer::send_in_background(settings, to, subject, body);
}

/// Ends this guest session (the cookie is forgotten on both sides).
pub async fn lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_token): Path<String>,
) -> AppResult<Response> {
    if let Some(cookie) = auth::read_cookie(&headers, auth::SHARE_COOKIE) {
        state.share_sessions.revoke(&cookie);
    }
    let clear = auth::clear_share_cookie(auth::cookie_secure(&state.settings.get(), &headers));
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&clear).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}
